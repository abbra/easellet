//! Pure decoding for the read-only I2C diagnostics.
//!
//! Nothing here touches hardware; host tests exercise it directly.

/// Sensirion CRC-8: polynomial 0x31, init 0xFF, no reflection, no final XOR.
pub fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0xFF_u8;
    for &byte in bytes {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x31
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// SHTC3 product code occupies bit 11 and bits 5:0.
pub fn shtc3_id_valid(id: u16) -> bool {
    id & 0x083F == 0x0807
}

/// SHTC3 temperature in hundredths of a degree Celsius.
pub fn shtc3_temperature(raw: u16) -> i32 {
    -4500 + (17_500 * i32::from(raw)) / 65_536
}

/// SHTC3 relative humidity in hundredths of a percent.
pub fn shtc3_humidity(raw: u16) -> u32 {
    10_000 * u32::from(raw) / 65_536
}

#[derive(Debug, PartialEq, Eq)]
pub struct Clock {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub weekday: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

fn bcd(value: u8) -> Option<u8> {
    let (tens, units) = (value >> 4, value & 0x0F);
    if tens > 9 || units > 9 {
        return None;
    }
    Some(tens * 10 + units)
}

/// Decode PCF85063A registers 04h..=0Ah (seconds through years).
///
/// `control_1` selects the hour mode. Returns `None` for the oscillator-stop
/// flag, 12-hour mode, reserved bits, or any out-of-range field, so nothing is
/// guessed from a half-valid clock.
pub fn pcf85063_time(control_1: u8, regs: &[u8; 7]) -> Option<Clock> {
    // This decoder only implements 24-hour mode (Control_1 bit 1 == 0).
    if control_1 & 0x02 != 0 {
        return None;
    }
    let seconds = *regs.first()?;
    if seconds & 0x80 != 0 {
        return None;
    }
    let second = bcd(seconds & 0x7F)?;
    let minute = bcd(regs[1] & 0x7F)?;
    if regs[2] & 0xC0 != 0 {
        return None;
    }
    let hour = bcd(regs[2] & 0x3F)?;
    let day = bcd(regs[3] & 0x3F)?;
    let weekday = regs[4] & 0x07;
    if regs[4] & 0xF8 != 0 {
        return None;
    }
    let month = bcd(regs[5] & 0x1F)?;
    if regs[5] & 0xE0 != 0 {
        return None;
    }
    let year = bcd(regs[6])?;
    let valid = second <= 59
        && minute <= 59
        && hour <= 23
        && (1..=31).contains(&day)
        && (1..=12).contains(&month);
    if !valid {
        return None;
    }
    Some(Clock {
        year: 2000 + u16::from(year),
        month,
        day,
        weekday,
        hour,
        minute,
        second,
    })
}

/// AXP2101 reg62h[4:0] to milliamps; codes 1-3 are reserved.
pub fn axp2101_charge_current(code: u8) -> Option<u16> {
    match code & 0x1F {
        0 => Some(0),
        4 => Some(100),
        5 => Some(125),
        6 => Some(150),
        7 => Some(175),
        8 => Some(200),
        9 => Some(300),
        10 => Some(400),
        11 => Some(500),
        12 => Some(600),
        13 => Some(700),
        14 => Some(800),
        15 => Some(900),
        16 => Some(1000),
        _ => None,
    }
}

/// AXP2101 reg64h[2:0] to millivolts.
pub fn axp2101_charge_voltage(code: u8) -> Option<u16> {
    match code & 0x07 {
        1 => Some(4000),
        2 => Some(4100),
        3 => Some(4200),
        4 => Some(4350),
        5 => Some(4400),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_datasheet_examples() {
        assert_eq!(crc8(&[0x00]), 0xAC);
        assert_eq!(crc8(&[0xBE, 0xEF]), 0x92);
    }

    #[test]
    fn shtc3_identifier() {
        assert!(shtc3_id_valid(0x0807));
        assert!(shtc3_id_valid(0x5C07)); // unspecified bits vary per part
        assert!(!shtc3_id_valid(0x0806));
    }

    #[test]
    fn shtc3_conversions() {
        assert_eq!(shtc3_temperature(0x0000), -4500);
        assert_eq!(shtc3_temperature(0xFFFF), 12999);
        assert_eq!(shtc3_humidity(0x0000), 0);
        assert_eq!(shtc3_humidity(0x8000), 5000);
    }

    #[test]
    fn rtc_decodes_and_rejects_invalid() {
        let regs = [0x45, 0x59, 0x23, 0x31, 0x06, 0x12, 0x25];
        assert_eq!(
            pcf85063_time(0x00, &regs),
            Some(Clock {
                year: 2025,
                month: 12,
                day: 31,
                weekday: 6,
                hour: 23,
                minute: 59,
                second: 45
            })
        );
        let stopped = [0xC5, 0x59, 0x23, 0x31, 0x06, 0x12, 0x25];
        assert_eq!(pcf85063_time(0x00, &stopped), None);
        let bad_bcd = [0x4A, 0x59, 0x23, 0x31, 0x06, 0x12, 0x25];
        assert_eq!(pcf85063_time(0x00, &bad_bcd), None);
        let bad_month = [0x00, 0x00, 0x00, 0x01, 0x00, 0x13, 0x25];
        assert_eq!(pcf85063_time(0x00, &bad_month), None);
        // 12-hour mode is rejected rather than decoded as 24-hour time.
        assert_eq!(pcf85063_time(0x02, &regs), None);
    }

    #[test]
    fn axp2101_encodings() {
        assert_eq!(axp2101_charge_current(0), Some(0));
        assert_eq!(axp2101_charge_current(8), Some(200));
        assert_eq!(axp2101_charge_current(9), Some(300));
        assert_eq!(axp2101_charge_current(16), Some(1000));
        assert_eq!(axp2101_charge_current(1), None);
        assert_eq!(axp2101_charge_current(17), None);
        assert_eq!(axp2101_charge_voltage(0), None);
        assert_eq!(axp2101_charge_voltage(3), Some(4200));
        assert_eq!(axp2101_charge_voltage(5), Some(4400));
        assert_eq!(axp2101_charge_voltage(6), None);
    }
}

/// AXP2101 reg01h[6:5] battery current direction.
pub fn axp2101_direction(status2: u8) -> &'static str {
    match (status2 >> 5) & 0x03 {
        0 => "standby",
        1 => "charging",
        2 => "discharging",
        _ => "reserved",
    }
}

/// AXP2101 reg01h[2:0] charge state.
pub fn axp2101_charge_state(status2: u8) -> &'static str {
    match status2 & 0x07 {
        0 => "trickle",
        1 => "precharge",
        2 => "cc",
        3 => "cv",
        4 => "done",
        5 => "not-charging",
        _ => "reserved",
    }
}

#[cfg(test)]
mod pmu_status_tests {
    use super::*;

    #[test]
    fn axp2101_status_fields() {
        assert_eq!(axp2101_direction(0b0010_0010), "charging");
        assert_eq!(axp2101_direction(0b0100_0000), "discharging");
        assert_eq!(axp2101_charge_state(0b0000_0100), "done");
        assert_eq!(axp2101_charge_state(0b0000_0101), "not-charging");
    }
}
