//! Calendar arithmetic for setting the PCF85063 from a network epoch.
//!
//! The firmware deliberately carries no date/time crate: converting Unix seconds
//! to a civil date is a dozen lines, and a host test can cover it exhaustively
//! against known epochs.

/// A civil UTC timestamp with a Sunday-based weekday.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub year: u32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// 0 = Sunday .. 6 = Saturday, the PCF85063 weekday convention used here.
    pub weekday: u8,
}

/// Convert Unix seconds (UTC) to a civil date.
///
/// Days are split with the era-based algorithm that avoids branch-per-month
/// tables; it is exact for the whole `u64` range this firmware can see.
pub fn from_unix(secs: u64) -> DateTime {
    let days = secs / 86_400;
    let rem = secs % 86_400;

    // Shift the epoch to 0000-03-01 so leap days land at the end of the year.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u8::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u8::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = u32::try_from(yoe + era * 400 + u64::from(month <= 2)).unwrap_or(0);

    DateTime {
        year,
        month,
        day,
        hour: u8::try_from(rem / 3_600).unwrap_or(0),
        minute: u8::try_from(rem % 3_600 / 60).unwrap_or(0),
        second: u8::try_from(rem % 60).unwrap_or(0),
        // 1970-01-01 was a Thursday, which is 4 with Sunday = 0.
        weekday: u8::try_from((days + 4) % 7).unwrap_or(0),
    }
}

/// Encode one field as two-digit BCD, rejecting values that do not fit.
pub fn bcd(value: u8) -> Option<u8> {
    if value > 99 {
        return None;
    }
    Some(((value / 10) << 4) | (value % 10))
}

impl DateTime {
    /// PCF85063A registers 04h..=0Ah in order: seconds, minutes, hours, days,
    /// weekdays, months, years (two-digit).
    pub fn pcf85063_regs(&self) -> Option<[u8; 7]> {
        let year = self.year.checked_sub(2000)?;
        if year > 99 {
            return None;
        }
        Some([
            bcd(self.second)?,
            bcd(self.minute)?,
            bcd(self.hour)?,
            bcd(self.day)?,
            self.weekday & 0x07,
            bcd(self.month)?,
            bcd(u8::try_from(year).ok()?)?,
        ])
    }

    /// `2026-10-31 15:42:08` — fixed width so a reply is easy to parse.
    pub fn iso(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Epoch seconds and the UTC date printed by an independent implementation.
    const VECTORS: [(u64, &str, u8); 6] = [
        (0, "1970-01-01 00:00:00", 4),
        (951_782_400, "2000-02-29 00:00:00", 2),
        (1_000_000_000, "2001-09-09 01:46:40", 0),
        (1_700_000_000, "2023-11-14 22:13:20", 2),
        (1_767_225_600, "2026-01-01 00:00:00", 4),
        (1_793_000_000, "2026-10-26 07:33:20", 1),
    ];

    #[test]
    fn unix_seconds_decode_to_civil_dates() {
        for (secs, expected, weekday) in VECTORS {
            let stamp = from_unix(secs);
            assert_eq!(stamp.iso(), expected, "epoch {secs}");
            assert_eq!(stamp.weekday, weekday, "epoch {secs}");
        }
    }

    #[test]
    fn pcf85063_registers_are_bcd() {
        let stamp = from_unix(1_793_000_000);
        assert_eq!(stamp.iso(), "2026-10-26 07:33:20");
        assert_eq!(
            stamp.pcf85063_regs(),
            Some([0x20, 0x33, 0x07, 0x26, 0x01, 0x10, 0x26])
        );
    }

    #[test]
    fn impossible_dates_are_refused() {
        let mut stamp = from_unix(1_793_000_000);
        stamp.year = 1999;
        assert_eq!(stamp.pcf85063_regs(), None, "PCF85063 has no century bit");
        stamp.year = 2100;
        assert_eq!(stamp.pcf85063_regs(), None);
        assert_eq!(bcd(100), None);
        assert_eq!(bcd(9), Some(0x09));
        assert_eq!(bcd(42), Some(0x42));
    }
}
