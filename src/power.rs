//! What a wake from deep sleep means, and when the frame should sleep again.
//!
//! The chip side (wake sources, RTC memory, `esp_deep_sleep_start`) is in
//! `sleep.rs`; this module is the policy, kept free of ESP-IDF so it is tested
//! on the host. The vendor's behaviour, which this copies
//! (`research/p7-investigation.md`):
//!
//! - **Timer or BOOT wakes show the next picture and sleep again** without
//!   bringing up Wi-Fi: that is the slideshow on battery.
//! - **KEY or PWR wakes, and a cold boot, stay awake**: somebody wants the
//!   portal, OpenDisplay or the assistant. After [`Settings::awake_secs`] with
//!   nothing happening the frame sleeps again, if sleeping is enabled at all.
//!
//! [`Settings::awake_secs`]: crate::config::Settings::awake_secs

use std::time::Duration;

/// GPIOs that can wake the chip. BOOT and KEY read low when pressed and share
/// ext1 (any-low); PWR reads high and has ext0 to itself, because ext1 takes one
/// polarity for its whole mask.
pub const BOOT_GPIO: u32 = 0;
pub const KEY_GPIO: u32 = 4;
pub const PWR_GPIO: u32 = 5;

/// The cause the chip reports, before it is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// Power-on, reset, or anything that is not a deep-sleep wake.
    Reset,
    Timer,
    /// ext0: PWR.
    Ext0,
    /// ext1, with the mask of pins that fired.
    Ext1(u64),
    Other,
}

/// Why this boot happened, as far as the frame's behaviour is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    Cold,
    Timer,
    Boot,
    Key,
    Pwr,
}

impl Wake {
    pub fn from_cause(cause: Cause) -> Self {
        match cause {
            Cause::Timer => Self::Timer,
            Cause::Ext0 => Self::Pwr,
            Cause::Ext1(pins) if pins & (1 << BOOT_GPIO) != 0 => Self::Boot,
            Cause::Ext1(pins) if pins & (1 << KEY_GPIO) != 0 => Self::Key,
            // A wake whose pin is unknown is treated as a person at the frame:
            // staying awake is the safe reading of an ambiguous wake.
            Cause::Ext1(_) => Self::Key,
            Cause::Reset | Cause::Other => Self::Cold,
        }
    }

    /// Show the next picture and go back to sleep, without Wi-Fi.
    pub fn shows_picture(self) -> bool {
        matches!(self, Self::Timer | Self::Boot)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Timer => "timer",
            Self::Boot => "boot",
            Self::Key => "key",
            Self::Pwr => "pwr",
        }
    }
}

/// Should an awake frame go to sleep now?
///
/// Only when sleeping is enabled (`sleep_secs > 0`), nothing is in progress
/// (`busy`: a refresh, a client, a voice turn being spoken), and nothing has
/// happened for `awake`.
pub fn sleep_due(sleep_secs: u32, busy: bool, idle_for: Duration, awake: Duration) -> bool {
    sleep_secs > 0 && !busy && idle_for >= awake
}

/// The battery as the AXP2101 reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    /// A cell is connected (REG00[3]).
    pub present: bool,
    /// USB power is good (REG00[5]).
    pub usb: bool,
    /// Current flows into the cell (REG01[6:5] = 01).
    pub charging: bool,
    /// Cell voltage, REG34[5:0]:REG35, 1 mV per bit.
    pub mv: u16,
    /// The fuel gauge's estimate, REG A4, 0..=100.
    pub percent: u8,
}

impl Battery {
    /// Decode the five registers the frame reads (all read-only).
    pub fn from_registers(status1: u8, status2: u8, vbat_h: u8, vbat_l: u8, percent: u8) -> Self {
        Self {
            present: status1 & (1 << 3) != 0,
            usb: status1 & (1 << 5) != 0,
            charging: (status2 >> 5) & 0b11 == 0b01,
            mv: (u16::from(vbat_h & 0x3F) << 8) | u16::from(vbat_l),
            percent: percent.min(100),
        }
    }

    /// Too empty to keep running: on battery alone, and either the gauge says
    /// 5 % or less or the cell is under 3.4 V (where a Li-ion cell's curve
    /// falls off and a ~30 s refresh could brown the board out). With USB
    /// power the frame runs regardless, and without a cell there is nothing
    /// to protect.
    pub fn low(&self) -> bool {
        self.present && !self.usb && (self.percent <= 5 || self.mv < 3_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakes_are_read_from_their_pins() {
        assert_eq!(Wake::from_cause(Cause::Reset), Wake::Cold);
        assert_eq!(Wake::from_cause(Cause::Timer), Wake::Timer);
        assert_eq!(Wake::from_cause(Cause::Ext0), Wake::Pwr);
        assert_eq!(Wake::from_cause(Cause::Ext1(1 << BOOT_GPIO)), Wake::Boot);
        assert_eq!(Wake::from_cause(Cause::Ext1(1 << KEY_GPIO)), Wake::Key);
        // Both at once: BOOT wins, as the vendor's handler checks it first.
        assert_eq!(
            Wake::from_cause(Cause::Ext1((1 << BOOT_GPIO) | (1 << KEY_GPIO))),
            Wake::Boot
        );
        assert_eq!(Wake::from_cause(Cause::Ext1(0)), Wake::Key);
        assert_eq!(Wake::from_cause(Cause::Other), Wake::Cold);
    }

    #[test]
    fn only_timer_and_boot_wakes_show_a_picture_and_sleep() {
        assert!(Wake::Timer.shows_picture());
        assert!(Wake::Boot.shows_picture());
        for wake in [Wake::Cold, Wake::Key, Wake::Pwr] {
            assert!(!wake.shows_picture(), "{wake:?}");
        }
    }

    #[test]
    fn sleep_waits_for_idle_and_never_while_busy_or_disabled() {
        let awake = Duration::from_secs(120);
        assert!(sleep_due(600, false, Duration::from_secs(120), awake));
        assert!(!sleep_due(600, false, Duration::from_secs(119), awake));
        assert!(!sleep_due(600, true, Duration::from_secs(900), awake));
        assert!(!sleep_due(0, false, Duration::from_secs(900), awake));
    }

    #[test]
    fn battery_registers_decode() {
        // Bench reading: USB good, cell present, standby, 4172 mV, 100 %.
        let bench = Battery::from_registers(0b0010_1000, 0b0000_0100, 0x10, 0x4C, 100);
        assert!(bench.present && bench.usb && !bench.charging);
        assert_eq!((bench.mv, bench.percent), (4_172, 100));
        let charging = Battery::from_registers(0b0010_1000, 0b0010_0010, 0x0F, 0xA0, 60);
        assert!(charging.charging);
        assert_eq!(charging.mv, 4_000);
    }

    #[test]
    fn low_battery_needs_a_cell_without_usb_that_is_empty() {
        let battery = |usb, mv, percent| Battery {
            present: true,
            usb,
            charging: false,
            mv,
            percent,
        };
        assert!(battery(false, 3_700, 5).low());
        assert!(battery(false, 3_390, 40).low());
        assert!(!battery(false, 3_700, 6).low());
        assert!(!battery(true, 3_300, 1).low(), "USB keeps it running");
        let absent = Battery {
            present: false,
            ..battery(false, 0, 0)
        };
        assert!(!absent.low(), "no cell, nothing to protect");
    }
}
