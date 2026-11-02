//! Request frames: `[opcode:2 BE][payload]`, parsed into borrowed views.

use core::num::NonZeroU16;

use crate::wire::Opcode;

/// How the panel should refresh after an image. Spectra 6 has one waveform, so
/// the device treats both the same; the value is kept for completeness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshMode {
    /// Full refresh (wire 0, and any value other than 1).
    Full,
    /// Fast refresh (wire 1).
    Fast,
}

impl RefreshMode {
    fn from_byte(byte: u8) -> Self {
        if byte == 1 { Self::Fast } else { Self::Full }
    }
}

/// How a DIRECT_WRITE image arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectWriteStart<'a> {
    /// Raw panel bytes follow in DATA frames.
    Raw,
    /// A zlib stream, starting in this frame.
    Zlib {
        /// Size of the image once inflated.
        uncompressed_size: u32,
        /// The first bytes of the zlib stream (may be empty).
        first: &'a [u8],
    },
}

/// One parsed request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request<'a> {
    /// 0x000F.
    Reboot,
    /// 0x0040.
    ConfigRead,
    /// 0x0041, payload as sent: a whole blob, or `[total:2 LE]` + 200 bytes.
    ConfigWrite(&'a [u8]),
    /// 0x0042, continuation bytes.
    ConfigChunk(&'a [u8]),
    /// 0x0043.
    FirmwareVersion,
    /// 0x0044.
    ReadMsd,
    /// 0x0045.
    ConfigClear,
    /// 0x0050, payload as sent.
    Authenticate(&'a [u8]),
    /// 0x0051.
    EnterDfu,
    /// 0x0052.
    PowerOff,
    /// 0x0053, with the one-shot wake timer if the client gave one.
    DeepSleep {
        /// Seconds until wake for this sleep only; `None` uses the configured
        /// interval.
        override_secs: Option<NonZeroU16>,
    },
    /// 0x0070.
    DirectWriteStart(DirectWriteStart<'a>),
    /// 0x0071, image or zlib bytes.
    DirectWriteData(&'a [u8]),
    /// 0x0072.
    DirectWriteEnd {
        /// Requested refresh.
        refresh: RefreshMode,
        /// Etag to remember for this image, if the client sent one.
        new_etag: Option<u32>,
    },
    /// 0x0073, payload as sent.
    LedActivate(&'a [u8]),
    /// 0x0075, payload as sent.
    LedStop(&'a [u8]),
    /// 0x0076, payload as sent.
    PartialWriteStart(&'a [u8]),
    /// 0x0077, payload as sent.
    Buzzer(&'a [u8]),
    /// 0x0080, payload as sent.
    PipeWriteStart(&'a [u8]),
    /// 0x0081, payload as sent.
    PipeWriteData(&'a [u8]),
    /// 0x0082, payload as sent.
    PipeWriteEnd(&'a [u8]),
    /// 0x0083, payload as sent.
    NfcEndpoint(&'a [u8]),
}

/// Why a frame could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Fewer than two bytes: there is no opcode to answer.
    NoOpcode,
    /// An opcode the spec does not define.
    UnknownOpcode(u16),
    /// A known opcode with a payload of the wrong shape.
    Malformed(Opcode),
}

impl<'a> Request<'a> {
    /// Parse one request frame.
    pub fn parse(frame: &'a [u8]) -> Result<Self, ParseError> {
        let [high, low, ref payload @ ..] = *frame else {
            return Err(ParseError::NoOpcode);
        };
        let raw = u16::from_be_bytes([high, low]);
        let opcode = Opcode::from_u16(raw).ok_or(ParseError::UnknownOpcode(raw))?;
        Ok(match opcode {
            Opcode::Reboot => Self::Reboot,
            Opcode::ConfigRead => Self::ConfigRead,
            Opcode::ConfigWrite => Self::ConfigWrite(payload),
            Opcode::ConfigChunk => Self::ConfigChunk(payload),
            Opcode::FirmwareVersion => Self::FirmwareVersion,
            Opcode::ReadMsd => Self::ReadMsd,
            Opcode::ConfigClear => Self::ConfigClear,
            Opcode::Authenticate => Self::Authenticate(payload),
            Opcode::EnterDfu => Self::EnterDfu,
            Opcode::PowerOff => Self::PowerOff,
            // Big-endian, unlike the rest of the protocol. An empty payload,
            // zero, or a lone byte all mean "use the configured interval";
            // trailing bytes are reserved and ignored.
            Opcode::DeepSleep => Self::DeepSleep {
                override_secs: match *payload {
                    [high, low, ..] => NonZeroU16::new(u16::from_be_bytes([high, low])),
                    _ => None,
                },
            },
            Opcode::DirectWriteStart => Self::DirectWriteStart(match *payload {
                [] => DirectWriteStart::Raw,
                [a, b, c, d, ref first @ ..] => DirectWriteStart::Zlib {
                    uncompressed_size: u32::from_le_bytes([a, b, c, d]),
                    first,
                },
                _ => return Err(ParseError::Malformed(opcode)),
            }),
            Opcode::DirectWriteData => Self::DirectWriteData(payload),
            // `[refresh]` then an optional big-endian etag, present by length.
            Opcode::DirectWriteEnd => Self::DirectWriteEnd {
                refresh: RefreshMode::from_byte(payload.first().copied().unwrap_or(0)),
                new_etag: match *payload {
                    [_, a, b, c, d, ..] => Some(u32::from_be_bytes([a, b, c, d])),
                    _ => None,
                },
            },
            Opcode::LedActivate => Self::LedActivate(payload),
            Opcode::LedStop => Self::LedStop(payload),
            Opcode::PartialWriteStart => Self::PartialWriteStart(payload),
            Opcode::Buzzer => Self::Buzzer(payload),
            Opcode::PipeWriteStart => Self::PipeWriteStart(payload),
            Opcode::PipeWriteData => Self::PipeWriteData(payload),
            Opcode::PipeWriteEnd => Self::PipeWriteEnd(payload),
            Opcode::NfcEndpoint => Self::NfcEndpoint(payload),
        })
    }

    /// The request's opcode.
    pub fn opcode(&self) -> Opcode {
        match self {
            Self::Reboot => Opcode::Reboot,
            Self::ConfigRead => Opcode::ConfigRead,
            Self::ConfigWrite(_) => Opcode::ConfigWrite,
            Self::ConfigChunk(_) => Opcode::ConfigChunk,
            Self::FirmwareVersion => Opcode::FirmwareVersion,
            Self::ReadMsd => Opcode::ReadMsd,
            Self::ConfigClear => Opcode::ConfigClear,
            Self::Authenticate(_) => Opcode::Authenticate,
            Self::EnterDfu => Opcode::EnterDfu,
            Self::PowerOff => Opcode::PowerOff,
            Self::DeepSleep { .. } => Opcode::DeepSleep,
            Self::DirectWriteStart(_) => Opcode::DirectWriteStart,
            Self::DirectWriteData(_) => Opcode::DirectWriteData,
            Self::DirectWriteEnd { .. } => Opcode::DirectWriteEnd,
            Self::LedActivate(_) => Opcode::LedActivate,
            Self::LedStop(_) => Opcode::LedStop,
            Self::PartialWriteStart(_) => Opcode::PartialWriteStart,
            Self::Buzzer(_) => Opcode::Buzzer,
            Self::PipeWriteStart(_) => Opcode::PipeWriteStart,
            Self::PipeWriteData(_) => Opcode::PipeWriteData,
            Self::PipeWriteEnd(_) => Opcode::PipeWriteEnd,
            Self::NfcEndpoint(_) => Opcode::NfcEndpoint,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixed_and_borrowed_payloads() {
        assert_eq!(Request::parse(&[0x00, 0x40]), Ok(Request::ConfigRead));
        assert_eq!(
            Request::parse(&[0x00, 0x41, 1, 2]),
            Ok(Request::ConfigWrite(&[1, 2]))
        );
        assert_eq!(Request::parse(&[0x00]), Err(ParseError::NoOpcode));
        assert_eq!(
            Request::parse(&[0x01, 0x00]),
            Err(ParseError::UnknownOpcode(0x0100))
        );
    }

    #[test]
    fn deep_sleep_seconds_are_big_endian_and_optional() {
        let sleep = |payload: &[u8]| {
            let mut frame = [0u8; 8];
            frame[..2].copy_from_slice(&[0x00, 0x53]);
            frame[2..2 + payload.len()].copy_from_slice(payload);
            match Request::parse(&frame[..2 + payload.len()]) {
                Ok(Request::DeepSleep { override_secs }) => override_secs.map(NonZeroU16::get),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(sleep(&[]), None);
        assert_eq!(sleep(&[0x01]), None);
        assert_eq!(sleep(&[0x00, 0x00]), None);
        assert_eq!(sleep(&[0x01, 0x2C]), Some(300));
        assert_eq!(sleep(&[0x01, 0x2C, 0xFF]), Some(300));
    }

    #[test]
    fn direct_write_start_and_end() {
        assert_eq!(
            Request::parse(&[0x00, 0x70]),
            Ok(Request::DirectWriteStart(DirectWriteStart::Raw))
        );
        assert_eq!(
            Request::parse(&[0x00, 0x70, 0x00, 0xEE, 0x02, 0x00, 0x78, 0x9C]),
            Ok(Request::DirectWriteStart(DirectWriteStart::Zlib {
                uncompressed_size: 192_000,
                first: &[0x78, 0x9C],
            }))
        );
        assert_eq!(
            Request::parse(&[0x00, 0x70, 1, 2]),
            Err(ParseError::Malformed(Opcode::DirectWriteStart))
        );
        assert_eq!(
            Request::parse(&[0x00, 0x72, 0x01, 0xDE, 0xAD, 0xBE, 0xEF]),
            Ok(Request::DirectWriteEnd {
                refresh: RefreshMode::Fast,
                new_etag: Some(0xDEAD_BEEF),
            })
        );
        assert_eq!(
            Request::parse(&[0x00, 0x72]),
            Ok(Request::DirectWriteEnd {
                refresh: RefreshMode::Full,
                new_etag: None,
            })
        );
    }

    #[test]
    fn opcode_matches_the_parsed_frame() {
        for opcode in Opcode::ALL {
            let [high, low] = (opcode as u16).to_be_bytes();
            if let Ok(request) = Request::parse(&[high, low]) {
                assert_eq!(request.opcode(), opcode);
            }
        }
    }
}
