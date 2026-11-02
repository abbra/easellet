//! The config blob's CRC: CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF, no
//! reflection, no final XOR), stored little-endian after the packets.

/// CRC-16/CCITT-FALSE over `data`.
pub fn crc16_ccitt_false(data: &[u8]) -> u16 {
    data.iter().fold(0xFFFF, |crc, &byte| step(crc, byte))
}

/// The config CRC over everything before the stored CRC, with the 2-byte length
/// field counted as zero whatever it holds. The toolbox computes the CRC before
/// it patches the length in, so every peer has to do the same.
pub fn config_crc(body: &[u8]) -> u16 {
    let (length, rest) = body.split_at(body.len().min(2));
    let crc = length.iter().fold(0xFFFF, |crc, _| step(crc, 0));
    rest.iter().fold(crc, |crc, &byte| step(crc, byte))
}

fn step(crc: u16, byte: u8) -> u16 {
    let mut crc = crc ^ (u16::from(byte) << 8);
    for _ in 0..8 {
        crc = if crc & 0x8000 != 0 {
            (crc << 1) ^ 0x1021
        } else {
            crc << 1
        };
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vectors from py-opendisplay tests/unit/test_config_crc.py.
    #[test]
    fn matches_the_client_vectors() {
        assert_eq!(config_crc(&[0x00, 0x00, 0x01]), 0xDCBD);
        assert_eq!(config_crc(&[0x2A, 0x00, 0x01, 0xAA, 0xBB]), 0xC239);
    }

    #[test]
    fn ignores_the_length_field() {
        let mut body = [0u8; 19];
        body[2..5].copy_from_slice(&[0x01, 0x00, 0x01]);
        assert_eq!(config_crc(&body), 0x0D7F);
        body[0] = 0x42;
        assert_eq!(config_crc(&body), 0x0D7F);
    }

    #[test]
    fn is_ccitt_false() {
        assert_eq!(crc16_ccitt_false(b"123456789"), 0x29B1);
    }
}
