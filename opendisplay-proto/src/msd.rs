//! The 16-byte manufacturer-specific data record (MSD, protocol v1 layout).
//!
//! The same bytes answer `ReadMsd` (0x0044), and without the company id they
//! are the `msd` mDNS TXT value (28 hex digits). Layout, as py-opendisplay's
//! `models/advertisement.py` decodes it:
//!
//! | bytes | meaning |
//! | --- | --- |
//! | 0..2 | company id 0x2446, little endian |
//! | 2..13 | dynamic block; a temperature/humidity sensor writes 3 bytes at its slot |
//! | 13 | chip temperature, `(°C + 40) * 2` |
//! | 14 | battery voltage in 10 mV, low 8 bits |
//! | 15 | bit 0 battery bit 8, bit 1 reboot flag, bit 2 connection requested, bits 4..7 counter |
//!
//! The ambient block (`SHT40` in the spec's sensor list, which is the layout the
//! frame's SHTC3 readings are published in) is a 24-bit little-endian word:
//! `rh_deci | (t_deci + 400) << 10`. `FF FF FF` means "the read failed".

/// The company id the spec assigns to OpenDisplay.
pub const COMPANY_ID: u16 = 0x2446;
/// Bytes in the record, company id included.
pub const LEN: usize = 16;
/// Where the ambient sensor's 3 bytes start inside the dynamic block (the
/// spec's default slot, used when the config's start byte is 0 or 0xFF).
pub const AMBIENT_SLOT: usize = 7;

/// One ambient reading, in tenths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ambient {
    /// Temperature in 0.1 °C, -40.0..=125.0 as the sensor spec allows.
    pub temp_deci: i16,
    /// Relative humidity in 0.1 %, 0..=100.0.
    pub rh_deci: u16,
}

/// What the frame knows about itself right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Telemetry {
    /// Battery voltage, if a battery is fitted and was read.
    pub battery_mv: Option<u16>,
    /// The microcontroller's own temperature, whole degrees.
    pub chip_temp_c: Option<i8>,
    /// The ambient sensor; `None` publishes the failure sentinel.
    pub ambient: Option<Ambient>,
    /// Set until the first client has seen a record after a reboot.
    pub rebooted: bool,
    /// Low 4 bits go out; clients use it to tell a fresh record from a repeat.
    pub counter: u8,
}

impl Telemetry {
    /// The record, company id included.
    pub fn encode(&self) -> [u8; LEN] {
        let mut out = [0u8; LEN];
        out[..2].copy_from_slice(&COMPANY_ID.to_le_bytes());
        let ambient = match self.ambient {
            Some(Ambient { temp_deci, rh_deci }) => {
                let temp = (i32::from(temp_deci).clamp(-400, 1250) + 400) as u32;
                let rh = u32::from(rh_deci.min(1000));
                rh | (temp << 10)
            }
            None => 0x00FF_FFFF,
        };
        let slot = 2 + AMBIENT_SLOT;
        out[slot..slot + 3].copy_from_slice(&ambient.to_le_bytes()[..3]);
        out[13] = match self.chip_temp_c {
            // Byte range: -40 °C .. 87.5 °C.
            Some(celsius) => ((i16::from(celsius) + 40).clamp(0, 127) * 2) as u8,
            None => 0,
        };
        let tens = self.battery_mv.map_or(0, |mv| (mv / 10).min(0x1FF));
        out[14] = (tens & 0xFF) as u8;
        out[15] = (tens >> 8) as u8 | u8::from(self.rebooted) << 1 | (self.counter & 0x0F) << 4;
        out
    }
}

/// The TXT form: the record without its company id, as 28 lowercase hex digits.
pub fn txt(record: &[u8; LEN]) -> [u8; 2 * (LEN - 2)] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 2 * (LEN - 2)];
    let (pairs, _) = out.as_chunks_mut::<2>();
    for (pair, byte) in pairs.iter_mut().zip(&record[2..]) {
        *pair = [HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 0x0F)]];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_land_where_the_client_reads_them() {
        let record = Telemetry {
            battery_mv: Some(4_172),
            chip_temp_c: Some(31),
            ambient: Some(Ambient {
                temp_deci: 270,
                rh_deci: 335,
            }),
            rebooted: true,
            counter: 0x13,
        }
        .encode();
        assert_eq!(record[..2], [0x46, 0x24]);
        assert_eq!(record[13], (31 + 40) * 2);
        // 417 tens of millivolts: 0xA1 low, bit 8 set.
        assert_eq!(record[14], 0xA1);
        assert_eq!(record[15], 0b0011_0011);
        let word = u32::from_le_bytes([record[9], record[10], record[11], 0]);
        assert_eq!(word & 0x3FF, 335);
        assert_eq!(word >> 10, 270 + 400);
    }

    #[test]
    fn a_missing_reading_is_the_failure_sentinel() {
        let record = Telemetry::default().encode();
        assert_eq!(record[9..12], [0xFF, 0xFF, 0xFF]);
        assert_eq!(record[13..], [0, 0, 0]);
    }

    #[test]
    fn the_txt_value_is_28_hex_digits_without_the_company_id() {
        let mut record = [0u8; LEN];
        record[2] = 0xAB;
        record[15] = 0x01;
        let txt = txt(&record);
        assert_eq!(&txt[..2], b"ab");
        assert_eq!(&txt[26..], b"01");
        assert_eq!(txt.len(), 28);
    }
}
