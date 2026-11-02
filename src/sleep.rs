//! Deep sleep on the chip: wake sources, the little state that survives, and the
//! jump into `esp_deep_sleep_start`.
//!
//! The policy (what a wake means, when to sleep) is `power.rs`; getting the rest
//! of the board quiet first (servers, radio, codecs, panel) is the main loop's,
//! because it owns those. This module only touches the RTC domain.
//!
//! State lives in `.rtc.data`, which IDF keeps powered through deep sleep and
//! initialises from the image at power-on. Atomics rather than `static mut`: they
//! have the layout of the plain integer, need no `unsafe` to touch, and nothing
//! here points into the heap, which does not survive.

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use esp_idf_svc::sys::{self, EspError};

use crate::power::{self, Cause};

/// Longest picture path remembered across sleep. Gallery paths are 8.3 names
/// under `gallery.dir`, whose own bound is 96 bytes.
const NAME_CAP: usize = 112;

#[unsafe(link_section = ".rtc.data")]
static WAKES: AtomicU32 = AtomicU32::new(0);
/// Seconds of the timed wake the frame went to sleep with, so a picture wake
/// can sleep again for the same interval even if the settings say 0 (`sleep
/// <secs>` from the console).
#[unsafe(link_section = ".rtc.data")]
static TIMER_SECS: AtomicU32 = AtomicU32::new(0);
#[unsafe(link_section = ".rtc.data")]
static SHOWN_LEN: AtomicU8 = AtomicU8::new(0);
#[unsafe(link_section = ".rtc.data")]
static SHOWN: [AtomicU8; NAME_CAP] = [const { AtomicU8::new(0) }; NAME_CAP];

/// The low-battery notice is on the panel; it is painted once per discharge,
/// not at every wake, since each refresh costs what little charge is left.
#[unsafe(link_section = ".rtc.data")]
static LOW_NOTICE: AtomicU8 = AtomicU8::new(0);

pub fn low_notice_shown() -> bool {
    LOW_NOTICE.load(Ordering::Relaxed) != 0
}

pub fn set_low_notice_shown(shown: bool) {
    LOW_NOTICE.store(u8::from(shown), Ordering::Relaxed);
}

/// ETag of the bridge picture on the panel (two halves of a u64), so a timed
/// wake can ask "anything newer than this?" and get a cheap 304.
#[unsafe(link_section = ".rtc.data")]
static ETAG_HIGH: AtomicU32 = AtomicU32::new(0);
#[unsafe(link_section = ".rtc.data")]
static ETAG_LOW: AtomicU32 = AtomicU32::new(0);

pub fn bridge_etag() -> Option<u64> {
    let value = (u64::from(ETAG_HIGH.load(Ordering::Relaxed)) << 32)
        | u64::from(ETAG_LOW.load(Ordering::Relaxed));
    (value != 0).then_some(value)
}

pub fn set_bridge_etag(value: Option<u64>) {
    let value = value.unwrap_or(0);
    ETAG_HIGH.store((value >> 32) as u32, Ordering::Relaxed);
    ETAG_LOW.store(value as u32, Ordering::Relaxed);
}

/// What woke the chip this time. Counts deep-sleep wakes as a side effect, so
/// call it once, at boot.
pub fn wake() -> power::Wake {
    // SAFETY: plain getters of the sleep controller's latched state.
    let cause = unsafe { sys::esp_sleep_get_wakeup_cause() };
    let cause = match cause {
        sys::esp_sleep_source_t_ESP_SLEEP_WAKEUP_TIMER => Cause::Timer,
        sys::esp_sleep_source_t_ESP_SLEEP_WAKEUP_EXT0 => Cause::Ext0,
        sys::esp_sleep_source_t_ESP_SLEEP_WAKEUP_EXT1 => {
            Cause::Ext1(unsafe { sys::esp_sleep_get_ext1_wakeup_status() })
        }
        sys::esp_sleep_source_t_ESP_SLEEP_WAKEUP_UNDEFINED => Cause::Reset,
        _ => Cause::Other,
    };
    let wake = power::Wake::from_cause(cause);
    if wake == power::Wake::Cold {
        // RTC memory is reloaded from the image on power-on, but not on a
        // software reset; a cold start begins counting again either way.
        WAKES.store(0, Ordering::Relaxed);
        TIMER_SECS.store(0, Ordering::Relaxed);
    } else {
        WAKES.fetch_add(1, Ordering::Relaxed);
    }
    wake
}

/// Deep-sleep wakes since the last cold start.
pub fn wakes() -> u32 {
    WAKES.load(Ordering::Relaxed)
}

/// The timed wake the frame last slept with, in seconds; 0 = buttons only.
pub fn timer_secs() -> u32 {
    TIMER_SECS.load(Ordering::Relaxed)
}

/// Remember the picture on the panel, so the next wake continues after it.
pub fn remember_shown(path: &str) {
    let bytes = path.as_bytes();
    if bytes.len() > NAME_CAP {
        SHOWN_LEN.store(0, Ordering::Relaxed);
        return;
    }
    for (slot, byte) in SHOWN.iter().zip(bytes) {
        slot.store(*byte, Ordering::Relaxed);
    }
    SHOWN_LEN.store(bytes.len() as u8, Ordering::Relaxed);
}

/// The picture shown before the last sleep, if one was remembered.
pub fn last_shown() -> Option<String> {
    let len = usize::from(SHOWN_LEN.load(Ordering::Relaxed));
    if len == 0 || len > NAME_CAP {
        return None;
    }
    let bytes: Vec<u8> = SHOWN[..len]
        .iter()
        .map(|slot| slot.load(Ordering::Relaxed))
        .collect();
    String::from_utf8(bytes).ok()
}

/// Arm the wake sources and sleep. Does not return.
///
/// BOOT and KEY wake on low (ext1), PWR on high (ext0); `timer_secs` of 0 means
/// buttons only. The caller has already quietened the board and waited for the
/// buttons to be released: a pin still in its wake state would wake the chip
/// immediately.
pub fn enter(timer_secs: u32) -> Result<std::convert::Infallible, EspError> {
    TIMER_SECS.store(timer_secs, Ordering::Relaxed);
    let low_pins = (1_u64 << power::BOOT_GPIO) | (1_u64 << power::KEY_GPIO);
    // SAFETY: these configure the RTC controller for the coming sleep; the GPIO
    // numbers are RTC-capable on the S3 (0-21).
    unsafe {
        EspError::convert(sys::esp_sleep_enable_ext1_wakeup_io(
            low_pins,
            sys::esp_sleep_ext1_wakeup_mode_t_ESP_EXT1_WAKEUP_ANY_LOW,
        ))?;
        // ext0 keeps the RTC peripherals powered, so these pulls hold in sleep:
        // up on the two active-low buttons (they also have board pull-ups), and
        // PWR's line, which the board drives low through its transistor.
        EspError::convert(sys::rtc_gpio_pullup_en(power::BOOT_GPIO as i32))?;
        EspError::convert(sys::rtc_gpio_pullup_en(power::KEY_GPIO as i32))?;
        EspError::convert(sys::esp_sleep_enable_ext0_wakeup(power::PWR_GPIO as i32, 1))?;
        if timer_secs > 0 {
            EspError::convert(sys::esp_sleep_enable_timer_wakeup(
                u64::from(timer_secs) * 1_000_000,
            ))?;
        }
        sys::esp_deep_sleep_start()
    }
}
