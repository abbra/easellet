//! Click and hold for the frame's three buttons, from levels sampled every tick.
//!
//! BOOT (GPIO0) and KEY (GPIO4) read low when pressed; PWR reaches GPIO5 high
//! through a transistor, beside the PMIC's own PWRON input
//! (`research/p7-investigation.md`). The caller turns each level into "pressed"
//! and feeds it here once per main-loop tick (20 ms); this module only decides
//! what a person did:
//!
//! - **Debounce.** A level must hold for [`DEBOUNCE`] before it counts.
//! - **Click on release, hold while held.** A press shorter than [`HOLD`] is a
//!   click, reported when the button comes up, so the same button can carry a
//!   second meaning when held. A hold is reported once, the moment it reaches
//!   [`HOLD`], and its release reports nothing.
//! - **A press must start after we started watching.** A button already down
//!   at boot, or one a console command consumed by waiting for it, is ignored
//!   until it has been seen up once; otherwise its release would read as a
//!   click nobody made.

use std::time::{Duration, Instant};

/// How long a level must be stable before it counts.
pub const DEBOUNCE: Duration = Duration::from_millis(30);

/// How long a press must last to be a hold rather than a click. 1.5 s is the
/// PMIC's own short/long boundary (REG27 IRQLEVEL reset value), so PWR feels
/// the same whether the PMIC or this module is listening.
pub const HOLD: Duration = Duration::from_millis(1_500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Click,
    Hold,
}

#[derive(Debug)]
pub struct Button {
    /// Last sampled level and when it last changed, before debouncing.
    raw: bool,
    raw_since: Instant,
    /// Debounced level.
    stable: bool,
    /// When the debounced press began.
    pressed_at: Option<Instant>,
    /// The current press already produced its hold.
    held: bool,
    /// The button has been seen up since it started being watched.
    armed: bool,
}

impl Button {
    pub fn new(now: Instant) -> Self {
        Self {
            raw: false,
            raw_since: now,
            stable: false,
            pressed_at: None,
            held: false,
            armed: false,
        }
    }

    /// Ignore the current press, if any: it belongs to someone else (a console
    /// command that waited for this button), so its release is not a click.
    pub fn disarm(&mut self) {
        self.armed = false;
        self.pressed_at = None;
    }

    /// Feed one sample; returns what the person did, if this sample completes it.
    pub fn update(&mut self, pressed: bool, now: Instant) -> Option<Event> {
        if pressed != self.raw {
            self.raw = pressed;
            self.raw_since = now;
            return None;
        }
        if now.duration_since(self.raw_since) < DEBOUNCE {
            return None;
        }
        if pressed != self.stable {
            self.stable = pressed;
            if pressed {
                self.pressed_at = self.armed.then_some(now);
                self.held = false;
                return None;
            }
            let click = self.pressed_at.take().is_some() && !self.held;
            self.armed = true;
            return click.then_some(Event::Click);
        }
        if !self.stable {
            self.armed = true;
        }
        if let Some(start) = self.pressed_at
            && !self.held
            && now.duration_since(start) >= HOLD
        {
            self.held = true;
            return Some(Event::Hold);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `pressed` every 20 ms for `ms`, collecting events.
    fn run(button: &mut Button, now: &mut Instant, pressed: bool, ms: u64) -> Vec<Event> {
        let mut events = Vec::new();
        for _ in 0..ms / 20 {
            *now += Duration::from_millis(20);
            events.extend(button.update(pressed, *now));
        }
        events
    }

    fn armed() -> (Button, Instant) {
        let mut now = Instant::now();
        let mut button = Button::new(now);
        assert!(run(&mut button, &mut now, false, 100).is_empty());
        (button, now)
    }

    #[test]
    fn a_short_press_is_a_click_on_release() {
        let (mut button, mut now) = armed();
        assert!(run(&mut button, &mut now, true, 200).is_empty());
        assert_eq!(run(&mut button, &mut now, false, 100), [Event::Click]);
    }

    #[test]
    fn a_long_press_is_one_hold_and_no_click() {
        let (mut button, mut now) = armed();
        assert_eq!(run(&mut button, &mut now, true, 3_000), [Event::Hold]);
        assert!(run(&mut button, &mut now, false, 100).is_empty());
    }

    #[test]
    fn bounces_shorter_than_the_debounce_are_ignored() {
        let (mut button, mut now) = armed();
        for _ in 0..5 {
            now += Duration::from_millis(10);
            assert_eq!(button.update(true, now), None);
            now += Duration::from_millis(10);
            assert_eq!(button.update(false, now), None);
        }
        assert!(run(&mut button, &mut now, false, 100).is_empty());
    }

    #[test]
    fn a_press_held_at_start_does_nothing_until_released() {
        let mut now = Instant::now();
        let mut button = Button::new(now);
        // Down from the first sample: neither its hold nor its release counts.
        assert!(run(&mut button, &mut now, true, 3_000).is_empty());
        assert!(run(&mut button, &mut now, false, 100).is_empty());
        // The next press is an ordinary one.
        assert!(run(&mut button, &mut now, true, 100).is_empty());
        assert_eq!(run(&mut button, &mut now, false, 100), [Event::Click]);
    }

    #[test]
    fn a_disarmed_press_does_not_click() {
        let (mut button, mut now) = armed();
        assert!(run(&mut button, &mut now, true, 200).is_empty());
        button.disarm();
        assert!(run(&mut button, &mut now, false, 100).is_empty());
        assert!(run(&mut button, &mut now, true, 100).is_empty());
        assert_eq!(run(&mut button, &mut now, false, 100), [Event::Click]);
    }
}
