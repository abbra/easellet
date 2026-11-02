//! The client side of the protocol, for the LAN bridge: build requests, read the
//! answers. Sans-IO like [`crate::server`]: the caller owns the socket, frames
//! each request with [`crate::lan::prefix`] and hands every response payload
//! back here.
//!
//! Only what a host that drives frames needs: read the config (to learn the
//! panel), read the MSD, and show one full-frame image with `DirectWrite`.

use crate::config::MAX_CONFIG_SIZE;
use crate::server::RefreshOutcome;
use crate::wire::{LAN_MAX_PAYLOAD, Opcode, RESP_ACK, RESP_REFRESH_SUCCESS, RESP_REFRESH_TIMEOUT};
use crate::{RefreshMode, msd};

/// Image bytes in one `DirectWriteData` request on the LAN: the payload minus
/// the opcode.
pub const LAN_DATA_CHUNK: usize = LAN_MAX_PAYLOAD - 2;
/// Bytes of the zlib stream that fit into `DirectWriteStart` itself, after the
/// opcode and the 4-byte uncompressed size.
pub const LAN_FIRST_CHUNK: usize = LAN_MAX_PAYLOAD - 6;

/// What went wrong with an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// The device refused the request; the byte after the echo, if any, is the
    /// opcode-specific error code.
    Refused(Opcode, Option<u8>),
    /// The answer belongs to another request, or is too short to be one.
    Unexpected,
    /// The config chunks do not add up.
    BadConfig,
}

/// A request with no payload.
pub fn simple(opcode: Opcode) -> [u8; 2] {
    (opcode as u16).to_be_bytes()
}

/// Check a plain ACK for `opcode`.
pub fn expect_ack(frame: &[u8], opcode: Opcode) -> Result<(), ClientError> {
    match *frame {
        [RESP_ACK, echo, ..] if echo == opcode.echo() => Ok(()),
        [_, echo, ref rest @ ..] if echo == opcode.echo() => {
            Err(ClientError::Refused(opcode, rest.first().copied()))
        }
        _ => Err(ClientError::Unexpected),
    }
}

/// The 16-byte record from a `ReadMsd` answer.
pub fn msd_record(frame: &[u8]) -> Result<[u8; msd::LEN], ClientError> {
    expect_ack(frame, Opcode::ReadMsd)?;
    frame
        .get(2..2 + msd::LEN)
        .and_then(|record| record.try_into().ok())
        .ok_or(ClientError::Unexpected)
}

/// Collects the chunked answer to `ConfigRead`: `[ack][echo][n:2 LE]`, the
/// first chunk also carrying the total length, then data.
pub struct ConfigReader {
    buf: [u8; MAX_CONFIG_SIZE],
    len: usize,
    total: Option<usize>,
    next: u16,
}

impl Default for ConfigReader {
    fn default() -> Self {
        Self {
            buf: [0; MAX_CONFIG_SIZE],
            len: 0,
            total: None,
            next: 0,
        }
    }
}

impl ConfigReader {
    /// Feed one answer; returns the whole blob once the last chunk is in.
    pub fn feed(&mut self, frame: &[u8]) -> Result<Option<&[u8]>, ClientError> {
        expect_ack(frame, Opcode::ConfigRead)?;
        let [_, _, low, high, ref rest @ ..] = *frame else {
            return Err(ClientError::Unexpected);
        };
        if u16::from_le_bytes([low, high]) != self.next {
            return Err(ClientError::BadConfig);
        }
        let data = if self.next == 0 {
            let [low, high, ref data @ ..] = *rest else {
                return Err(ClientError::BadConfig);
            };
            let total = usize::from(u16::from_le_bytes([low, high]));
            if total == 0 || total > MAX_CONFIG_SIZE {
                return Err(ClientError::BadConfig);
            }
            self.total = Some(total);
            data
        } else {
            rest
        };
        let total = self.total.ok_or(ClientError::BadConfig)?;
        let end = self.len + data.len();
        if end > total {
            return Err(ClientError::BadConfig);
        }
        self.buf[self.len..end].copy_from_slice(data);
        self.len = end;
        self.next += 1;
        Ok((end == total).then(|| &self.buf[..end]))
    }
}

/// `DirectWriteStart` for a zlib stream: opcode, uncompressed size (LE), and
/// up to [`LAN_FIRST_CHUNK`] bytes of the stream. Returns the request length
/// and how many stream bytes it carries.
pub fn write_start_zlib(uncompressed: u32, stream: &[u8], out: &mut [u8]) -> (usize, usize) {
    let first = stream
        .len()
        .min(LAN_FIRST_CHUNK)
        .min(out.len().saturating_sub(6));
    out[..2].copy_from_slice(&simple(Opcode::DirectWriteStart));
    out[2..6].copy_from_slice(&uncompressed.to_le_bytes());
    out[6..6 + first].copy_from_slice(&stream[..first]);
    (6 + first, first)
}

/// `DirectWriteData` with up to [`LAN_DATA_CHUNK`] bytes. Returns the request
/// length and how many bytes it carries.
pub fn write_data(chunk: &[u8], out: &mut [u8]) -> (usize, usize) {
    let take = chunk
        .len()
        .min(LAN_DATA_CHUNK)
        .min(out.len().saturating_sub(2));
    out[..2].copy_from_slice(&simple(Opcode::DirectWriteData));
    out[2..2 + take].copy_from_slice(&chunk[..take]);
    (2 + take, take)
}

/// `DirectWriteEnd`: refresh mode, then the image's etag when there is one.
pub fn write_end(refresh: RefreshMode, etag: Option<u32>, out: &mut [u8; 7]) -> usize {
    out[..2].copy_from_slice(&simple(Opcode::DirectWriteEnd));
    out[2] = match refresh {
        RefreshMode::Full => 0,
        RefreshMode::Fast => 1,
    };
    match etag {
        Some(etag) => {
            out[3..7].copy_from_slice(&etag.to_be_bytes());
            7
        }
        None => 3,
    }
}

/// The unsolicited answer once the panel has refreshed (0x73) or given up (0x74).
pub fn refresh_outcome(frame: &[u8]) -> Option<RefreshOutcome> {
    match *frame {
        [RESP_ACK, RESP_REFRESH_SUCCESS, ..] => Some(RefreshOutcome::Done),
        [RESP_ACK, RESP_REFRESH_TIMEOUT, ..] => Some(RefreshOutcome::TimedOut),
        _ => None,
    }
}
