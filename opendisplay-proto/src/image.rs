//! DIRECT_WRITE reassembly into the caller's frame buffer.
//!
//! Raw images are copied at a running offset. zlib images are inflated straight
//! into the frame with a non-wrapping output buffer: the already-written frame
//! is the inflater's history, so any window size works (clients use 512 bytes)
//! and no separate 32 KiB dictionary exists. The frame is passed in on every
//! call rather than held, so the firmware keeps owning it between frames.

use miniz_oxide::inflate::TINFLStatus;
use miniz_oxide::inflate::core::inflate_flags::{
    TINFL_FLAG_HAS_MORE_INPUT, TINFL_FLAG_PARSE_ZLIB_HEADER,
    TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
};
use miniz_oxide::inflate::core::{DecompressorOxide, decompress};

use crate::request::DirectWriteStart;

/// Why an image transfer failed. Every error ends the transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// DATA or END without a START.
    NotStarted,
    /// The image would not be exactly one frame.
    SizeMismatch {
        /// Frame size.
        expected: usize,
        /// Size the client declared.
        declared: usize,
    },
    /// More bytes than the frame holds.
    Overflow,
    /// The zlib stream is invalid.
    Corrupt,
    /// END before the whole frame arrived.
    Incomplete {
        /// Bytes of the frame written so far.
        written: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Raw { written: usize },
    Zlib { written: usize, done: bool },
}

/// Reassembles one image at a time.
pub struct ImageReceiver {
    state: State,
    // About 11 KB; reused across transfers rather than rebuilt on the stack.
    inflater: DecompressorOxide,
}

impl core::fmt::Debug for ImageReceiver {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImageReceiver")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Default for ImageReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageReceiver {
    /// An idle receiver.
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            inflater: DecompressorOxide::new(),
        }
    }

    /// True between a successful START and the END or an error.
    pub fn is_active(&self) -> bool {
        self.state != State::Idle
    }

    /// Bytes of the frame written so far.
    pub fn written(&self) -> usize {
        match self.state {
            State::Idle => 0,
            State::Raw { written } | State::Zlib { written, .. } => written,
        }
    }

    /// Begin an image, replacing any transfer in progress.
    pub fn start(
        &mut self,
        start: DirectWriteStart<'_>,
        frame: &mut [u8],
    ) -> Result<(), ImageError> {
        self.state = State::Idle;
        match start {
            DirectWriteStart::Raw => {
                self.state = State::Raw { written: 0 };
                Ok(())
            }
            DirectWriteStart::Zlib {
                uncompressed_size,
                first,
            } => {
                let declared = usize::try_from(uncompressed_size).unwrap_or(usize::MAX);
                if declared != frame.len() {
                    return Err(ImageError::SizeMismatch {
                        expected: frame.len(),
                        declared,
                    });
                }
                self.inflater.init();
                self.state = State::Zlib {
                    written: 0,
                    done: false,
                };
                self.data(first, frame)
            }
        }
    }

    /// Add one DATA chunk.
    pub fn data(&mut self, chunk: &[u8], frame: &mut [u8]) -> Result<(), ImageError> {
        let result = match &mut self.state {
            State::Idle => return Err(ImageError::NotStarted),
            State::Raw { written } => {
                let end = *written + chunk.len();
                match frame.get_mut(*written..end) {
                    Some(target) => {
                        target.copy_from_slice(chunk);
                        *written = end;
                        Ok(())
                    }
                    None => Err(ImageError::Overflow),
                }
            }
            State::Zlib { written, done } => {
                inflate(&mut self.inflater, chunk, frame, written, done)
            }
        };
        if result.is_err() {
            self.state = State::Idle;
        }
        result
    }

    /// End the transfer; succeeds only if exactly one whole frame arrived.
    pub fn finish(&mut self, frame_len: usize) -> Result<(), ImageError> {
        let state = core::mem::replace(&mut self.state, State::Idle);
        match state {
            State::Idle => Err(ImageError::NotStarted),
            State::Raw { written }
            | State::Zlib {
                written,
                done: true,
            } if written == frame_len => Ok(()),
            State::Raw { written } | State::Zlib { written, .. } => {
                Err(ImageError::Incomplete { written })
            }
        }
    }

    /// Drop the transfer in progress, if any.
    pub fn abort(&mut self) {
        self.state = State::Idle;
    }
}

fn inflate(
    inflater: &mut DecompressorOxide,
    mut input: &[u8],
    frame: &mut [u8],
    written: &mut usize,
    done: &mut bool,
) -> Result<(), ImageError> {
    const FLAGS: u32 = TINFL_FLAG_PARSE_ZLIB_HEADER
        | TINFL_FLAG_HAS_MORE_INPUT
        | TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
    loop {
        if *done {
            // Bytes after the end of the stream are a client error.
            return if input.is_empty() {
                Ok(())
            } else {
                Err(ImageError::Overflow)
            };
        }
        if input.is_empty() {
            return Ok(());
        }
        let (status, consumed, produced) = decompress(inflater, input, frame, *written, FLAGS);
        input = &input[consumed..];
        *written += produced;
        match status {
            TINFLStatus::Done => *done = true,
            TINFLStatus::NeedsMoreInput => {}
            // Returned near the end of the buffer even when the stream fits
            // exactly, so it only means "call again". A stream that really
            // overflows stops making progress here, or stays unfinished and is
            // caught by `finish`.
            TINFLStatus::HasMoreOutput if consumed > 0 || produced > 0 => {}
            TINFLStatus::HasMoreOutput => return Err(ImageError::Overflow),
            _ => return Err(ImageError::Corrupt),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;
    use miniz_oxide::deflate::compress_to_vec_zlib;

    fn picture(len: usize) -> Vec<u8> {
        // Mostly runs with some noise, like a dithered photo.
        (0..len)
            .map(|i| {
                let i = u32::try_from(i).unwrap();
                ((i / 97) as u8 & 0x33) ^ (i.wrapping_mul(2_654_435_761) >> 29) as u8
            })
            .collect()
    }

    #[test]
    fn raw_image_lands_in_the_frame() {
        let image = picture(1000);
        let mut frame = [0u8; 1000];
        let mut receiver = ImageReceiver::new();
        receiver.start(DirectWriteStart::Raw, &mut frame).unwrap();
        for chunk in image.chunks(230) {
            receiver.data(chunk, &mut frame).unwrap();
        }
        receiver.finish(frame.len()).unwrap();
        assert_eq!(&frame[..], &image[..]);
        assert!(!receiver.is_active());
    }

    #[test]
    fn raw_overflow_and_short_image_fail() {
        let mut frame = [0u8; 10];
        let mut receiver = ImageReceiver::new();
        receiver.start(DirectWriteStart::Raw, &mut frame).unwrap();
        assert_eq!(
            receiver.data(&[0; 11], &mut frame),
            Err(ImageError::Overflow)
        );
        assert!(!receiver.is_active());

        receiver.start(DirectWriteStart::Raw, &mut frame).unwrap();
        receiver.data(&[0; 4], &mut frame).unwrap();
        assert_eq!(
            receiver.finish(frame.len()),
            Err(ImageError::Incomplete { written: 4 })
        );
    }

    #[test]
    fn zlib_image_inflates_across_any_chunking() {
        let image = picture(192_000);
        let stream = compress_to_vec_zlib(&image, 6);
        for (first, chunk) in [(0, 1), (194, 230), (194, 4092), (stream.len(), 1)] {
            let first = first.min(stream.len());
            let mut frame = std::vec![0u8; image.len()];
            let mut receiver = ImageReceiver::new();
            let start = DirectWriteStart::Zlib {
                uncompressed_size: 192_000,
                first: &stream[..first],
            };
            receiver.start(start, &mut frame).unwrap();
            for piece in stream[first..].chunks(chunk) {
                receiver.data(piece, &mut frame).unwrap();
            }
            receiver.finish(frame.len()).unwrap();
            assert!(frame == image, "first={first} chunk={chunk}");
        }
    }

    #[test]
    fn zlib_errors_end_the_transfer() {
        let mut frame = [0u8; 64];
        let mut receiver = ImageReceiver::new();
        let wrong = DirectWriteStart::Zlib {
            uncompressed_size: 65,
            first: &[],
        };
        assert_eq!(
            receiver.start(wrong, &mut frame),
            Err(ImageError::SizeMismatch {
                expected: 64,
                declared: 65
            })
        );

        // Too much data surfaces as soon as the inflater stalls, or at the END
        // because the stream never finished; either way the image is refused.
        let too_big = compress_to_vec_zlib(&picture(300), 6);
        for split in [too_big.len(), 10] {
            let start = DirectWriteStart::Zlib {
                uncompressed_size: 64,
                first: &too_big[..split],
            };
            let result = receiver
                .start(start, &mut frame)
                .and_then(|()| receiver.data(&too_big[split..], &mut frame))
                .and_then(|()| receiver.finish(frame.len()));
            assert!(
                matches!(
                    result,
                    Err(ImageError::Overflow | ImageError::Incomplete { .. })
                ),
                "{result:?}"
            );
            assert!(!receiver.is_active());
        }

        let start = DirectWriteStart::Zlib {
            uncompressed_size: 64,
            first: &[0x78, 0x9C, 0xFF, 0xFF, 0xFF],
        };
        assert_eq!(receiver.start(start, &mut frame), Err(ImageError::Corrupt));

        let short = compress_to_vec_zlib(&[7u8; 63], 6);
        let start = DirectWriteStart::Zlib {
            uncompressed_size: 64,
            first: &short,
        };
        receiver.start(start, &mut frame).unwrap();
        assert_eq!(
            receiver.finish(frame.len()),
            Err(ImageError::Incomplete { written: 63 })
        );
    }
}
