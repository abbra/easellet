//! Auto-advancing slideshow over the images in one directory of the card.
//!
//! Kept free of `esp-idf` types so the scheduling can be tested on the host: the
//! panel, the SD card and the clock appear only as a list of file names and an
//! `Instant` the caller supplies. The rules that matter are all here.
//!
//! Two of them are not obvious. A Spectra 6 refresh takes about 31 s and holds the
//! panel and the SPI bus for the whole time, so the interval must cover the
//! refresh *and* leave the finished picture on the wall long enough to look at -
//! clamping to one refresh alone would mean the panel is never once still. That
//! is why [`MIN_INTERVAL_SECS`] is a refresh plus a long dwell rather than one
//! refresh. And the slideshow must
//! never start a refresh while somebody is talking to the frame, because that
//! refresh is what makes the frame deaf for the next half minute - the caller
//! passes `quiet` and a skipped advance is retried, not dropped.

use std::time::{Duration, Instant};

/// Shortest interval worth configuring: one refresh to draw the picture, and a
/// long dwell afterwards to stand still and be looked at.
///
/// Clamping to a single refresh (30 s) would have the frame drawing continuously:
/// the panel would be settling for 31 s of every 30 s cycle, so nobody would ever
/// see a finished picture. Measured refresh is ~31 s, so the picture is only
/// still for the remainder - two minutes leaves about a minute and a half of it.
pub const MIN_INTERVAL_SECS: u32 = 120;

/// An hour is plenty; the frame is a clock as much as a picture frame, and the
/// card is not going anywhere.
pub const MAX_INTERVAL_SECS: u32 = 60 * 60;

/// Five minutes: long enough to be a background rather than a slideshow, short
/// enough that an afternoon of standing there cycles a card full of pictures.
pub const DEFAULT_INTERVAL_SECS: u32 = 300;

/// The slideshow, off until `start`.
#[derive(Debug)]
pub struct Gallery {
    /// Directory as the user named it, relative to the card root.
    dir: String,
    /// Relative paths of the images found there, sorted, never empty while running.
    images: Vec<String>,
    /// Which entry `take_next` will hand out.
    index: usize,
    interval: Duration,
    /// When the current picture has had its turn; `None` means "as soon as the
    /// panel is free and the room is quiet", which is what a fresh start wants.
    due: Option<Instant>,
    /// Pictures actually put on the panel, for `gallery status`.
    advances: u32,
    /// Advances skipped because a turn was in progress or the panel was busy.
    /// Reported so a slideshow that looks stuck is visibly not stuck. Counted
    /// once per deadline: the loop asks every 10 ms and a raw count would be
    /// three digits long after one conversation.
    skipped: u32,
    /// Whether the refusal at the current deadline has already been counted.
    refused: bool,
}

impl Default for Gallery {
    fn default() -> Self {
        Self::stopped()
    }
}

impl Gallery {
    /// A gallery that shows nothing until it is started.
    pub fn stopped() -> Self {
        Self {
            dir: String::new(),
            images: Vec::new(),
            index: 0,
            interval: Duration::from_secs(u64::from(DEFAULT_INTERVAL_SECS)),
            due: None,
            advances: 0,
            skipped: 0,
            refused: false,
        }
    }

    /// Parse the interval, clamped to what the panel can actually keep up with.
    ///
    /// Refusing instead of clamping would be friendlier to a typo, but the number
    /// that comes back in the reply is the number that will be used, so a clamp
    /// that says so is clear enough.
    pub fn interval_from_secs(secs: u32) -> Duration {
        let clamped = secs.clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS);
        Duration::from_secs(u64::from(clamped))
    }

    /// Begin (or restart) the slideshow at the first image.
    pub fn start(&mut self, dir: impl Into<String>, mut images: Vec<String>, interval: Duration) {
        // Unstable on purpose: equal strings are indistinguishable, and the stable sort
        // keeps a 4 KB scratch buffer on the stack (the 8 KB main task runs this).
        images.sort_unstable();
        images.dedup();
        self.dir = dir.into();
        self.images = images;
        self.index = 0;
        self.interval = interval.clamp(
            Duration::from_secs(u64::from(MIN_INTERVAL_SECS)),
            Duration::from_secs(u64::from(MAX_INTERVAL_SECS)),
        );
        self.due = None;
        self.advances = 0;
        self.skipped = 0;
        self.refused = false;
    }

    /// Stop showing anything; the panel keeps whatever is on it.
    pub fn stop(&mut self) {
        let interval = self.interval;
        *self = Self::stopped();
        self.interval = interval;
    }

    /// Take a new list of pictures after the card changed under a running show.
    ///
    /// An upload lands while the slideshow is cycling, so the list it started
    /// with is stale by definition. `focus` names the new picture: the next
    /// advance then draws the thing the user just sent, instead of waiting a
    /// full lap for it. Without a focus the current position is kept, clamped,
    /// and the deadline is untouched - a file appearing is not a reason to
    /// restart the interval.
    pub fn replace(&mut self, mut images: Vec<String>, focus: Option<&str>) {
        if !self.is_running() {
            return;
        }
        images.sort_unstable();
        images.dedup();
        if let Some(focus) = focus
            && let Some(position) = images.iter().position(|image| image == focus)
        {
            self.images = images;
            self.index = position;
            self.refused = false;
            return;
        }
        self.index = if images.is_empty() {
            0
        } else {
            self.index.min(images.len() - 1)
        };
        self.images = images;
    }

    /// Directory the entries came from, as the user named it.
    pub fn dir(&self) -> &str {
        &self.dir
    }

    /// True when a slideshow is configured, whether or not an advance is due.
    pub fn is_running(&self) -> bool {
        !self.images.is_empty()
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Pictures in the running slideshow; zero when it is off.
    pub fn count(&self) -> usize {
        self.images.len()
    }

    /// Make `position` (0-based) the next picture, drawn as soon as the panel
    /// is free: «покажи пятую картинку». `false` when there is no such picture.
    pub fn aim(&mut self, position: usize) -> bool {
        if position >= self.images.len() {
            return false;
        }
        self.index = position;
        self.due = None;
        self.refused = false;
        true
    }

    /// Draw the next picture as soon as the panel is free.
    pub fn hurry(&mut self) {
        self.due = None;
        self.refused = false;
    }

    /// Position of the picture before the one on the panel. The panel shows
    /// the entry before `index` (the next one), so that is two back.
    pub fn previous_position(&self) -> usize {
        let len = self.images.len().max(1);
        (self.index + 2 * len - 2) % len
    }

    /// Advance every `interval` from now on (clamped); the current picture
    /// gets a full new interval.
    pub fn set_interval(&mut self, interval: Duration, now: Instant) {
        self.interval =
            Self::interval_from_secs(u32::try_from(interval.as_secs()).unwrap_or(u32::MAX));
        if self.is_running() && self.due.is_some() {
            self.due = Some(now + self.interval);
        }
    }

    /// The next path to draw, or `None` when the slideshow is off.
    ///
    /// Takes the entry so a caller that fails to decode cannot draw it twice; the
    /// schedule is pushed out as well, which also bounds the retry rate of a
    /// broken file to one attempt per interval.
    pub fn take_next(&mut self, now: Instant) -> Option<String> {
        if !self.is_running() {
            return None;
        }
        let path = self.images[self.index].clone();
        self.index = (self.index + 1) % self.images.len();
        self.due = Some(now + self.interval);
        self.refused = false;
        Some(path)
    }

    /// True when the slideshow wants the panel: running, due, and allowed.
    ///
    /// `allowed` carries the caller's judgement - quiet room, panel free - and a
    /// refusal is counted but does not move the deadline, so the advance happens
    /// as soon as the reason for the refusal clears rather than an interval later.
    pub fn wants_advance(&mut self, now: Instant, allowed: bool) -> bool {
        let Some(due) = self.due else {
            // A fresh start draws immediately; nothing is on screen yet.
            if self.is_running() && !allowed {
                self.refuse();
            }
            return self.is_running() && allowed;
        };
        if !self.is_running() || now < due {
            return false;
        }
        if !allowed {
            self.refuse();
            return false;
        }
        true
    }

    /// Count this deadline's refusal once, however often the loop asks.
    fn refuse(&mut self) {
        if !self.refused {
            self.refused = true;
            self.skipped = self.skipped.saturating_add(1);
        }
    }

    /// Record that a picture reached the panel.
    pub fn record_advance(&mut self) {
        self.advances = self.advances.saturating_add(1);
    }

    /// Push the next advance out by a full interval, without drawing anything.
    ///
    /// Used after a decode failure, so an unreadable file cannot spin the loop.
    pub fn postpone(&mut self, now: Instant) {
        self.due = Some(now + self.interval);
        self.refused = false;
    }

    /// One line for `gallery status`.
    pub fn status(&self) -> String {
        if !self.is_running() {
            return "OK gallery=off\r\n".to_owned();
        }
        format!(
            "OK gallery=on dir=/sdcard/{} images={} next={} interval_s={} advances={} skipped={}\r\n",
            self.dir,
            self.images.len(),
            self.images[self.index],
            self.interval.as_secs(),
            self.advances,
            self.skipped,
        )
    }
}

/// The picture to show after `last` in the sorted `images`: the first name
/// after it, wrapping to the start. `last` need not be in the list any more
/// (deleted while the frame slept); the order still continues from where it was.
pub fn after<'a>(images: &'a [String], last: &str) -> Option<&'a str> {
    images
        .iter()
        .find(|name| name.as_str() > last)
        .or_else(|| images.first())
        .map(String::as_str)
}

/// Insert `item` into the sorted `kept`, holding at most `limit` items: the
/// smallest ones seen so far. A directory larger than the limit then yields
/// the same pictures whatever order the card returns them in, and memory stays
/// bounded by the limit rather than by the directory.
pub fn keep_first<T: Ord>(kept: &mut Vec<T>, item: T, limit: usize) {
    if kept.len() == limit && kept.last().is_some_and(|last| &item >= last) {
        return;
    }
    let at = kept.partition_point(|existing| existing < &item);
    kept.insert(at, item);
    kept.truncate(limit);
}

/// The lowest `n` in `1..=max` with no `UPnnnn.EL6` among `names`, compared
/// without regard to case. One pass over the names a directory read already
/// produced, instead of asking the card about each number in turn.
pub fn first_free_upload<'a>(names: impl IntoIterator<Item = &'a str>, max: u32) -> Option<u32> {
    let mut taken = vec![false; max as usize + 1];
    for name in names {
        let upper = name.to_ascii_uppercase();
        let Some(digits) = upper
            .strip_prefix("UP")
            .and_then(|rest| rest.strip_suffix(".EL6"))
        else {
            continue;
        };
        if digits.len() == 4
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && let Ok(number) = digits.parse::<usize>()
            && number <= max as usize
        {
            taken[number] = true;
        }
    }
    (1..=max).find(|&number| !taken[number as usize])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn images(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_stopped_gallery_never_wants_the_panel() {
        let mut gallery = Gallery::stopped();
        let now = Instant::now();
        assert!(!gallery.is_running());
        assert!(!gallery.wants_advance(now, true));
        assert_eq!(gallery.take_next(now), None);
        assert!(gallery.status().contains("gallery=off"));
    }

    #[test]
    fn a_fresh_start_draws_immediately_and_then_waits() {
        let mut gallery = Gallery::stopped();
        let now = Instant::now();
        // One tick short of the floor rather than a literal: the interval is
        // clamped, so a test that names a number below it is really testing the
        // clamp and not the schedule.
        let interval = Duration::from_secs(u64::from(MIN_INTERVAL_SECS));
        gallery.start("EASELLET/GALLERY", images(&["b.bmp", "a.bmp"]), interval);
        // Sorted, so the card's own order does not decide what shows first.
        assert!(gallery.wants_advance(now, true));
        assert_eq!(gallery.take_next(now).as_deref(), Some("a.bmp"));
        assert!(!gallery.wants_advance(now, true));
        assert!(!gallery.wants_advance(now + interval - Duration::from_secs(1), true));
        assert!(gallery.wants_advance(now + interval, true));
    }

    #[test]
    fn an_upload_jumps_the_queue_and_a_removal_keeps_the_position() {
        let mut gallery = Gallery::stopped();
        gallery.start(
            "EASELLET/UPLOADS",
            images(&["a.el6", "b.el6", "c.el6"]),
            Duration::from_secs(300),
        );
        // The slideshow is about to draw "a".
        assert_eq!(gallery.take_next(Instant::now()).unwrap(), "a.el6");

        gallery.replace(
            images(&["a.el6", "b.el6", "c.el6", "UP0001.EL6"]),
            Some("UP0001.EL6"),
        );
        assert_eq!(gallery.take_next(Instant::now()).unwrap(), "UP0001.EL6");

        // A file disappearing must not index past the end.
        gallery.replace(images(&["UP0001.EL6"]), None);
        assert_eq!(gallery.take_next(Instant::now()).unwrap(), "UP0001.EL6");
    }

    #[test]
    fn images_wrap_around() {
        let mut gallery = Gallery::stopped();
        let now = Instant::now();
        gallery.start(
            "g",
            images(&["one.bmp", "two.el6"]),
            Duration::from_secs(60),
        );
        assert_eq!(gallery.take_next(now).as_deref(), Some("one.bmp"));
        assert_eq!(gallery.take_next(now).as_deref(), Some("two.el6"));
        assert_eq!(gallery.take_next(now).as_deref(), Some("one.bmp"));
    }

    #[test]
    fn a_busy_panel_is_retried_not_skipped() {
        let mut gallery = Gallery::stopped();
        let now = Instant::now();
        let interval = Duration::from_secs(u64::from(MIN_INTERVAL_SECS));
        gallery.start("g", images(&["a.bmp"]), interval);
        assert_eq!(gallery.take_next(now).as_deref(), Some("a.bmp"));
        let later = now + interval + Duration::from_secs(1);
        // Somebody is talking, so the refresh waits; the deadline does not move.
        assert!(!gallery.wants_advance(later, false));
        // The loop asks every 10 ms; one conversation must not read as thousands
        // of skipped pictures.
        assert!(!gallery.wants_advance(later, false));
        assert!(!gallery.wants_advance(later, false));
        assert_eq!(gallery.skipped, 1);
        assert!(gallery.wants_advance(later, true));
        assert_eq!(gallery.advances, 0, "skipping must not count as an advance");
    }

    #[test]
    fn a_decode_failure_cannot_spin_the_loop() {
        let mut gallery = Gallery::stopped();
        let now = Instant::now();
        let interval = Duration::from_secs(u64::from(MIN_INTERVAL_SECS));
        gallery.start("g", images(&["broken.bmp"]), interval);
        assert!(gallery.wants_advance(now, true));
        let _ = gallery.take_next(now);
        gallery.postpone(now);
        assert!(!gallery.wants_advance(now + Duration::from_secs(1), true));
        assert!(gallery.wants_advance(now + interval, true));
    }

    #[test]
    fn the_interval_is_clamped_to_what_the_panel_can_keep_up_with() {
        // A Spectra 6 refresh is ~31 s, so the floor has to cover the refresh and
        // a dwell long enough to see it; a one-second slideshow would queue work
        // the panel could never show.
        assert_eq!(
            Gallery::interval_from_secs(1),
            Duration::from_secs(u64::from(MIN_INTERVAL_SECS))
        );
        assert_eq!(
            Gallery::interval_from_secs(3600),
            Duration::from_secs(u64::from(MAX_INTERVAL_SECS))
        );
        assert_eq!(Gallery::interval_from_secs(600), Duration::from_secs(600));
        let mut gallery = Gallery::stopped();
        gallery.start("g", images(&["a.bmp"]), Duration::from_secs(1));
        assert_eq!(
            gallery.interval(),
            Duration::from_secs(u64::from(MIN_INTERVAL_SECS))
        );
    }

    #[test]
    fn stopping_keeps_the_interval_for_next_time() {
        let mut gallery = Gallery::stopped();
        gallery.start("g", images(&["a.bmp"]), Duration::from_secs(120));
        gallery.stop();
        assert!(!gallery.is_running());
        assert_eq!(gallery.interval(), Duration::from_secs(120));
    }

    #[test]
    fn duplicates_and_order_do_not_confuse_the_cursor() {
        let mut gallery = Gallery::stopped();
        let now = Instant::now();
        gallery.start(
            "g",
            images(&["b.bmp", "a.bmp", "b.bmp"]),
            Duration::from_secs(30),
        );
        assert_eq!(gallery.images.len(), 2);
        assert!(gallery.status().contains("images=2"));
        assert_eq!(gallery.take_next(now).as_deref(), Some("a.bmp"));
        assert_eq!(gallery.take_next(now).as_deref(), Some("b.bmp"));
    }

    #[test]
    fn status_names_the_next_picture_and_the_counts() {
        let mut gallery = Gallery::stopped();
        let now = Instant::now();
        let interval = Duration::from_secs(u64::from(MIN_INTERVAL_SECS));
        gallery.start("EASELLET/GALLERY", images(&["a.bmp", "b.bmp"]), interval);
        let _ = gallery.take_next(now);
        gallery.record_advance();
        let status = gallery.status();
        assert!(status.contains("dir=/sdcard/EASELLET/GALLERY"), "{status}");
        assert!(status.contains("next=b.bmp"), "{status}");
        assert!(
            status.contains(&format!("interval_s={}", interval.as_secs())),
            "{status}"
        );
        assert!(status.contains("advances=1"), "{status}");
    }

    #[test]
    fn the_first_names_are_kept_whatever_the_order() {
        let mut kept = Vec::new();
        for name in ["d", "b", "e", "a", "c"] {
            keep_first(&mut kept, name, 3);
        }
        assert_eq!(kept, ["a", "b", "c"]);
        let mut all = Vec::new();
        for name in ["b", "a"] {
            keep_first(&mut all, name, 3);
        }
        assert_eq!(all, ["a", "b"]);
    }

    #[test]
    fn an_upload_takes_the_lowest_free_number() {
        assert_eq!(first_free_upload([], 9_999), Some(1));
        assert_eq!(
            first_free_upload(["UP0001.EL6", "up0002.el6", "UP0004.EL6", "x.bmp"], 9_999),
            Some(3)
        );
        // Names that only look similar do not take a number.
        assert_eq!(
            first_free_upload(
                ["UP1.EL6", "UP0001.BMP", "UPABCD.EL6", "UP00001.EL6"],
                9_999
            ),
            Some(1)
        );
        assert_eq!(first_free_upload(["UP0001.EL6", "UP0002.EL6"], 2), None);
    }

    #[test]
    fn the_slideshow_continues_after_the_last_picture() {
        let images: Vec<String> = ["A/1.EL6", "A/2.EL6", "A/4.EL6"]
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        assert_eq!(after(&images, "A/1.EL6"), Some("A/2.EL6"));
        assert_eq!(after(&images, "A/4.EL6"), Some("A/1.EL6"));
        // A deleted picture: the next one in order still follows.
        assert_eq!(after(&images, "A/3.EL6"), Some("A/4.EL6"));
        assert_eq!(after(&[], "A/1.EL6"), None);
    }

    #[test]
    fn aiming_moves_the_next_picture_and_draws_at_once() {
        let mut gallery = Gallery::stopped();
        let now = Instant::now();
        gallery.start(
            "P",
            vec!["a".into(), "b".into(), "c".into()],
            Duration::from_secs(300),
        );
        assert_eq!(gallery.take_next(now).as_deref(), Some("a"));
        assert_eq!(gallery.take_next(now).as_deref(), Some("b"));
        // «b» is on the panel: the previous one is «a».
        assert_eq!(gallery.previous_position(), 0);
        assert!(gallery.aim(gallery.previous_position()));
        assert!(
            gallery.wants_advance(now, true),
            "an aimed picture is due at once"
        );
        assert_eq!(gallery.take_next(now).as_deref(), Some("a"));
        // From «a», previous wraps to «c».
        assert_eq!(gallery.previous_position(), 2);
        assert!(!gallery.aim(3), "there is no fourth picture");
        assert!(gallery.aim(2));
        assert_eq!(gallery.take_next(now).as_deref(), Some("c"));
    }

    #[test]
    fn a_new_interval_is_clamped_and_restarts_the_wait() {
        let mut gallery = Gallery::stopped();
        let now = Instant::now();
        gallery.start("P", vec!["a".into()], Duration::from_secs(300));
        let _ = gallery.take_next(now);
        gallery.set_interval(Duration::from_secs(10), now);
        assert_eq!(gallery.interval().as_secs(), u64::from(MIN_INTERVAL_SECS));
        assert!(!gallery.wants_advance(now + Duration::from_secs(60), true));
        assert!(gallery.wants_advance(now + gallery.interval(), true));
    }
}
