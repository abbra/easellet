//! LAN transport framing: `[len:2 LE][payload]`, payload 1..=4094 bytes.
//!
//! A reader takes exactly two bytes, checks them with [`payload_len`], then
//! reads exactly that many. A bad length means the stream is out of step, so
//! the connection must be dropped.

use crate::wire::LAN_MAX_PAYLOAD;

/// A length prefix outside 1..=4094.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadLength(pub usize);

/// The payload length announced by a prefix.
pub fn payload_len(prefix: [u8; 2]) -> Result<usize, BadLength> {
    let len = usize::from(u16::from_le_bytes(prefix));
    if (1..=LAN_MAX_PAYLOAD).contains(&len) {
        Ok(len)
    } else {
        Err(BadLength(len))
    }
}

/// The prefix to send in front of a payload.
pub fn prefix(payload: &[u8]) -> Result<[u8; 2], BadLength> {
    let len = payload.len();
    if (1..=LAN_MAX_PAYLOAD).contains(&len) {
        // In range, so it fits in a u16.
        Ok((len as u16).to_le_bytes())
    } else {
        Err(BadLength(len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_one_to_4094() {
        assert_eq!(payload_len([0x01, 0x00]), Ok(1));
        assert_eq!(payload_len([0xFE, 0x0F]), Ok(4094));
        assert_eq!(payload_len([0x00, 0x00]), Err(BadLength(0)));
        assert_eq!(payload_len([0xFF, 0x0F]), Err(BadLength(4095)));
        assert_eq!(prefix(&[0; 300]), Ok([0x2C, 0x01]));
        assert_eq!(prefix(&[]), Err(BadLength(0)));
    }
}
