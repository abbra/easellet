//! Always-listening voice detection for the idle watcher.
//!
//! The device has no wake word: on this board the only honest trigger is
//! "somebody is talking near the frame", and the cheapest way to get that is
//! loudness. This module is the part of that idea that can be tested without a
//! microphone — an energy detector with a pre-roll buffer, a hangover window and
//! a duration cap — driven by mono frames the caller has already captured.
//!
//! Three details exist for reasons that only show up on hardware:
//!
//! - **Pre-roll.** The trigger can only fire once the voice is already loud, by
//!   which point the first word is in the past. Keeping the last
//!   [`PREROLL_MS`] of audio and prepending it to the utterance is what stops
//!   «Сколько будет…» from arriving as «будет…».
//! - **Hangover.** Speech has gaps inside it — a stop consonant is silence for
//!   tens of milliseconds. Ending on the first quiet frame would cut every
//!   sentence in half, so an utterance ends only after [`END_SILENCE_MS`] of
//!   quiet.
//! - **Cap.** A television in the next room, or a fan, can hold the detector
//!   above threshold indefinitely. [`MAX_UTTERANCE_MS`] bounds both the memory a
//!   turn can allocate and the request it can send.
//!
//! The detector does not resample and does not know about stereo: the caller
//! down-mixes and passes one frame at a time, which is also what makes the tests
//! below ordinary arithmetic.

/// Frame duration the detector expects. The console loop already wakes every
/// 20 ms, so a frame of this size costs no extra scheduling.
pub const FRAME_MS: u32 = 20;

/// Audio kept before the trigger fires. A second covers the pause before the
/// first word as well as a soft onset, and costs 48 KB of a 24 kHz 16-bit
/// buffer. The ring is cheap next to losing half a question.
pub const PREROLL_MS: u32 = 1_000;

/// How long the signal must stay above the threshold before a trigger. One
/// frame (20 ms) is too little — a door click clears it — and 300 ms is long
/// enough to swallow a short «да».
pub const MIN_SPEECH_MS: u32 = 160;

/// Quiet that ends an utterance.
pub const END_SILENCE_MS: u32 = 700;

/// Quiet that ends an utterance with under [`NAME_VOICE_MS`] of voice in it.
///
/// People call the frame by name and then pause: «Рамка… покажи погоду». With
/// [`END_SILENCE_MS`] the name alone became an utterance, and while it was being
/// recognised (2-3 s, microphone closed) the start of the question was lost:
/// «…жи погоду в Вашингтоне». A short burst of voice is far more often a name
/// than a whole question, so it waits longer for the rest; a lone «да» is
/// answered 0.8 s later, which nobody notices next to the recogniser's time.
pub const NAME_PAUSE_MS: u32 = 1_500;

/// Voice below which an utterance is taken for a name that may go on.
pub const NAME_VOICE_MS: u32 = 1_000;

/// Quiet that ends an utterance holding `voiced_ms` of voice.
#[must_use]
pub fn end_silence_ms(voiced_ms: u32) -> u32 {
    if voiced_ms < NAME_VOICE_MS {
        NAME_PAUSE_MS
    } else {
        END_SILENCE_MS
    }
}

/// Longest utterance the detector will hand back, sentence or not.
pub const MAX_UTTERANCE_MS: u32 = 10_000;

/// Smallest trigger level, in 16-bit sample units: 600 ≈ -35 dBFS.
///
/// Three values were tried on hardware. 80 (≈ -52 dBFS) and 120 (≈ -49 dBFS) sit
/// at or below this room's own noise, so the watcher fired on a television, a
/// cough or a door and the recogniser then invented a sentence for the noise.
/// 600 leaves the room quiet and still hears a spoken question across the room,
/// so it is the floor here. The gate still follows the room above it, which is
/// what [`Detector::floor`] is for; this constant is only the floor of that
/// floor.
pub const DEFAULT_THRESHOLD: f32 = 600.0;

/// How far a frame must rise above the measured noise floor to count as speech.
///
/// Four is a compromise: a syllable next to the frame is far louder than its
/// own room, while a television across the room is only a few dB above it. Too
/// low and the room talks; too high and a soft-spoken child does not.
pub const NOISE_MARGIN: f32 = 4.0;

/// Per-frame weight of the noise-floor average, at one frame per 20 ms.
///
/// 0.01 gives a time constant of about two seconds: fast enough to follow a room
/// that gets noisy, slow enough that a single loud word cannot raise the gate in
/// the middle of that word.
pub const FLOOR_RATE: f32 = 0.01;

/// Ceiling of the adaptive trigger.
///
/// A very loud room must not push the gate so high that a person standing at the
/// frame is inaudible; above this the room is simply too loud for an energy
/// detector and the panel says so.
pub const MAX_THRESHOLD: f32 = 4_000.0;

/// Root-mean-square of a frame, in sample units.
pub fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    // `f32`, not `f64`: the S3 has a single-precision FPU, so double-precision
    // arithmetic here would be emulated in software for no benefit — a 480-sample
    // sum of squares does not need the extra mantissa bits.
    let sum: f32 = samples
        .iter()
        .map(|&sample| {
            let value = f32::from(sample);
            value * value
        })
        .sum();
    (sum / samples.len() as f32).sqrt()
}

/// What the detector has to say after a frame.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// Nothing happened; keep feeding frames.
    Idle,
    /// Speech started; the utterance being assembled already contains the
    /// pre-roll.
    Started,
    /// The utterance ended. The audio is mono at the rate given to [`Detector::new`].
    Captured(Vec<i16>),
}

#[derive(Debug, PartialEq)]
enum State {
    Idle,
    Speech,
}

pub struct Detector {
    rate: u32,
    /// Lowest trigger the room may produce; [`Detector::new`] sets it.
    threshold: f32,
    /// Slow average of what the room sounds like when nobody is speaking.
    floor: f32,
    /// `max(floor * NOISE_MARGIN, threshold)`, recomputed per frame.
    trigger: f32,
    preroll_frames: usize,
    /// Ring of the most recent quiet frames, oldest first once drained.
    preroll: Vec<Vec<i16>>,
    state: State,
    speech_ms: u32,
    silence_ms: u32,
    /// Frames above the gate since the utterance started (the trigger's own
    /// frames included), in ms: what tells a name from a sentence.
    voiced_ms: u32,
    utterance: Vec<i16>,
}

impl Detector {
    /// `rate` is the sample rate of the frames the caller will pass.
    #[must_use]
    pub fn new(rate: u32, threshold: f32) -> Self {
        let preroll_frames = PREROLL_MS as usize * rate as usize / 1_000 / frame_len(rate).max(1);
        Self {
            rate,
            threshold,
            floor: 0.0,
            trigger: threshold,
            preroll_frames: preroll_frames.max(1),
            preroll: Vec::new(),
            state: State::Idle,
            speech_ms: 0,
            silence_ms: 0,
            voiced_ms: 0,
            utterance: Vec::new(),
        }
    }

    /// Configured floor the trigger never goes below, as given to [`Detector::new`].
    #[must_use]
    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    /// Trigger level in force right now, after the noise floor is applied.
    #[must_use]
    pub fn trigger(&self) -> f32 {
        self.trigger
    }

    /// Current estimate of the room's own loudness.
    #[must_use]
    pub fn floor(&self) -> f32 {
        self.floor
    }

    /// Recompute the gate and follow the room.
    ///
    /// The floor is a leaky minimum, not an average: a quiet frame is believed at
    /// once, a loud one pulls the estimate up only slowly. That asymmetry is what
    /// makes a television stop mattering within a few seconds even though it
    /// never pauses, while the gaps between sentences drop the gate back to the
    /// room immediately. An average would instead be dragged up by speech itself.
    fn follow_room(&mut self, level: f32) -> f32 {
        if level < self.floor {
            self.floor = level;
        } else {
            self.floor += (level - self.floor) * FLOOR_RATE;
        }
        self.recompute_trigger()
    }

    /// `max(floor * NOISE_MARGIN, threshold)`, capped so a loud room cannot bury
    /// a person standing at the frame.
    fn recompute_trigger(&mut self) -> f32 {
        let trigger = (self.floor * NOISE_MARGIN).max(self.threshold);
        self.trigger = trigger.min(MAX_THRESHOLD.max(self.threshold));
        self.trigger
    }

    /// Move the floor of the gate while the room estimate is kept.
    ///
    /// This exists because the only honest way to pick a threshold on this board
    /// is to say something and read the level back: the same room measured 20..60
    /// RMS of noise on one evening and near-silence on another, and the level
    /// that a question produces depends on where the person stands. A constant
    /// that is right for one of those is deaf for the other.
    pub fn set_threshold(&mut self, threshold: f32) {
        self.threshold = threshold;
        self.recompute_trigger();
    }

    #[must_use]
    pub fn state_name(&self) -> &'static str {
        match self.state {
            State::Idle => "idle",
            State::Speech => "speech",
        }
    }

    /// Drop everything: the caller is changing modes or the stream broke.
    pub fn reset(&mut self) {
        self.preroll.clear();
        self.state = State::Idle;
        self.speech_ms = 0;
        self.silence_ms = 0;
        self.voiced_ms = 0;
        self.utterance.clear();
        // The room has not changed, so the floor survives a reset; only the
        // frames around it are forgotten.
    }

    /// Feed one frame of mono audio. Frames shorter than [`FRAME_MS`] are
    /// accepted and accounted by their real duration, so a short read at the
    /// tail of a buffer does not distort the timing.
    pub fn push(&mut self, frame: &[i16]) -> Event {
        if frame.is_empty() {
            return Event::Idle;
        }
        let frame_ms = duration_ms(frame.len(), self.rate);
        let level = rms(frame);

        match self.state {
            State::Idle => {
                self.push_preroll(frame);
                let trigger = self.follow_room(level);
                if level <= trigger {
                    // Below threshold for a moment: nothing is being said.
                    self.speech_ms = 0;
                    return Event::Idle;
                }
                self.speech_ms = self.speech_ms.saturating_add(frame_ms);
                if self.speech_ms < MIN_SPEECH_MS {
                    return Event::Idle;
                }
                // Loud long enough to be speech. The pre-roll already holds this
                // frame, so the utterance starts before the trigger did.
                self.state = State::Speech;
                self.silence_ms = 0;
                self.voiced_ms = self.speech_ms;
                self.utterance = self
                    .preroll
                    .drain(..)
                    .flat_map(|held| held.into_iter())
                    .collect();
                Event::Started
            }
            State::Speech => {
                self.utterance.extend_from_slice(frame);
                // Keep learning the room while speaking, so a television that is
                // still playing after this turn does not trigger the next one.
                let _ = self.follow_room(level);
                // The gate itself is frozen for the length of the utterance: a room
                // that got louder mid-sentence must not cut that sentence short.
                if level > self.trigger {
                    self.silence_ms = 0;
                    self.voiced_ms = self.voiced_ms.saturating_add(frame_ms);
                } else {
                    self.silence_ms = self.silence_ms.saturating_add(frame_ms);
                }
                let total_ms = duration_ms(self.utterance.len(), self.rate);
                if self.silence_ms >= end_silence_ms(self.voiced_ms) || total_ms >= MAX_UTTERANCE_MS
                {
                    let pcm = std::mem::take(&mut self.utterance);
                    self.state = State::Idle;
                    self.speech_ms = 0;
                    self.silence_ms = 0;
                    self.voiced_ms = 0;
                    return Event::Captured(pcm);
                }
                Event::Idle
            }
        }
    }

    fn push_preroll(&mut self, frame: &[i16]) {
        self.preroll.push(frame.to_vec());
        while self.preroll.len() > self.preroll_frames {
            self.preroll.remove(0);
        }
    }
}

/// Samples in one [`FRAME_MS`] frame at `rate`.
#[must_use]
pub fn frame_len(rate: u32) -> usize {
    usize::try_from(u64::from(rate) * u64::from(FRAME_MS) / 1_000).unwrap_or(0)
}

fn duration_ms(samples: usize, rate: u32) -> u32 {
    if rate == 0 {
        return 0;
    }
    u32::try_from(samples as u64 * 1_000 / u64::from(rate)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 24_000;

    fn frame(amplitude: i16) -> Vec<i16> {
        // Loudness is all this detector reads, so a square wave is a fair
        // stand-in for a syllable and keeps the arithmetic obvious.
        vec![amplitude; frame_len(RATE)]
    }

    fn speech_frames(count: u32) -> Vec<Vec<i16>> {
        (0..count).map(|_| frame(4_000)).collect()
    }

    fn feed(detector: &mut Detector, frames: &[Vec<i16>]) -> Vec<Event> {
        frames
            .iter()
            .map(|frame| detector.push(frame))
            .filter(|event| *event != Event::Idle)
            .collect()
    }

    #[test]
    fn quiet_room_never_triggers() {
        let mut detector = Detector::new(RATE, DEFAULT_THRESHOLD);
        let quiet: Vec<Vec<i16>> = (0..100).map(|_| frame(40)).collect();
        assert_eq!(feed(&mut detector, &quiet), Vec::new());
        assert_eq!(detector.state_name(), "idle");
    }

    #[test]
    fn speech_starts_after_the_minimum_and_ends_after_silence() {
        let mut detector = Detector::new(RATE, DEFAULT_THRESHOLD);
        let quiet = frame(0);

        // MIN_SPEECH_MS / FRAME_MS frames of voice are needed before a trigger.
        let needed = MIN_SPEECH_MS.div_ceil(FRAME_MS);
        for _ in 0..needed - 1 {
            assert_eq!(detector.push(&frame(4_000)), Event::Idle);
        }
        assert_eq!(detector.push(&frame(4_000)), Event::Started);

        // Hangover: a short gap must not end the utterance.
        assert_eq!(detector.push(&quiet), Event::Idle);
        assert_eq!(detector.push(&frame(4_000)), Event::Idle);

        // Under a second of voice: the name-length pause applies.
        let hangover = NAME_PAUSE_MS.div_ceil(FRAME_MS);
        let mut events = Vec::new();
        for _ in 0..hangover {
            if let event @ Event::Captured(_) = detector.push(&quiet) {
                events.push(event);
            }
        }
        let [Event::Captured(pcm)] = events.as_slice() else {
            panic!("expected exactly one captured utterance, got {events:?}");
        };
        assert!(!pcm.is_empty());
        assert_eq!(detector.state_name(), "idle");
    }

    #[test]
    fn the_utterance_starts_before_the_trigger() {
        let mut detector = Detector::new(RATE, DEFAULT_THRESHOLD);
        // A full pre-roll of quiet, so the buffer is truly saturated when voice
        // arrives. The voiced frames then displace quiet ones from the front.
        let capacity = PREROLL_MS.div_ceil(FRAME_MS) as usize;
        for _ in 0..capacity {
            assert_eq!(detector.push(&frame(0)), Event::Idle);
        }
        let needed = MIN_SPEECH_MS.div_ceil(FRAME_MS) as usize;
        for _ in 0..needed {
            let _ = detector.push(&frame(4_000));
        }
        // Ending the utterance yields the pre-roll plus the hangover frames.
        let mut captured = None;
        for _ in 0..NAME_PAUSE_MS.div_ceil(FRAME_MS) {
            if let Event::Captured(pcm) = detector.push(&frame(0)) {
                captured = Some(pcm);
                break;
            }
        }
        let pcm = captured.expect("utterance");
        let preroll_samples = PREROLL_MS as usize * RATE as usize / 1_000;
        assert!(
            pcm.len() >= preroll_samples,
            "utterance {} is shorter than the pre-roll {preroll_samples}",
            pcm.len()
        );
        // The utterance opens with the quiet that preceded the voice, and the
        // voice itself begins before the trigger did — that is the whole point
        // of the hold buffer.
        assert_eq!(pcm[0], 0, "the pre-roll quiet must survive");
        let first_voiced = pcm
            .iter()
            .position(|&sample| sample == 4_000)
            .expect("the utterance must contain the voice");
        let expected = preroll_samples - frame_len(RATE) * needed;
        assert_eq!(
            first_voiced, expected,
            "voice must begin {needed} frames before the trigger"
        );
    }

    /// Voice above [`MAX_THRESHOLD`], so a long stretch of it stays voice
    /// however far the floor follows it.
    fn loud(ms: u32) -> Vec<Vec<i16>> {
        (0..ms / FRAME_MS).map(|_| frame(8_000)).collect()
    }

    #[test]
    fn a_name_and_a_pause_wait_for_the_question() {
        let mut detector = Detector::new(RATE, DEFAULT_THRESHOLD);
        let quiet = frame(0);
        // «Рамка»: half a second of voice, then a one-second pause.
        let events = feed(&mut detector, &loud(500));
        assert_eq!(events, [Event::Started]);
        for _ in 0..1_000 / FRAME_MS {
            assert_eq!(
                detector.push(&quiet),
                Event::Idle,
                "the name must not end it"
            );
        }
        // «покажи погоду…»: the same utterance goes on.
        assert_eq!(feed(&mut detector, &loud(1_500)), []);
        // Now two seconds of voice: the ordinary ending applies.
        let mut ended_after = None;
        for index in 1..=NAME_PAUSE_MS / FRAME_MS {
            if let Event::Captured(_) = detector.push(&quiet) {
                ended_after = Some(index * FRAME_MS);
                break;
            }
        }
        assert_eq!(ended_after, Some(END_SILENCE_MS));
    }

    #[test]
    fn a_lone_short_word_still_ends() {
        let mut detector = Detector::new(RATE, DEFAULT_THRESHOLD);
        let _ = feed(&mut detector, &loud(400));
        let mut ended_after = None;
        for index in 1..=2 * NAME_PAUSE_MS / FRAME_MS {
            if let Event::Captured(_) = detector.push(&frame(0)) {
                ended_after = Some(index * FRAME_MS);
                break;
            }
        }
        assert_eq!(ended_after, Some(NAME_PAUSE_MS));
        assert_eq!(end_silence_ms(NAME_VOICE_MS), END_SILENCE_MS);
    }

    #[test]
    fn the_preroll_is_bounded() {
        let mut detector = Detector::new(RATE, DEFAULT_THRESHOLD);
        for _ in 0..200 {
            let _ = detector.push(&frame(10));
        }
        let capacity = PREROLL_MS.div_ceil(FRAME_MS) as usize;
        assert!(detector.preroll.len() <= capacity + 1);
    }

    #[test]
    fn a_long_noise_cannot_run_forever() {
        let mut detector = Detector::new(RATE, DEFAULT_THRESHOLD);
        let frames = MAX_UTTERANCE_MS.div_ceil(FRAME_MS) + 2;
        let events = feed(&mut detector, &speech_frames(frames));
        let captured = events
            .iter()
            .find_map(|event| match event {
                Event::Captured(pcm) => Some(pcm),
                _ => None,
            })
            .expect("the cap must end the utterance");
        assert!(
            captured.len()
                <= MAX_UTTERANCE_MS as usize * RATE as usize / 1_000 + 2 * frame_len(RATE)
        );
    }

    fn frames(amplitude: i16, count: u32) -> Vec<Vec<i16>> {
        (0..count).map(|_| frame(amplitude)).collect()
    }

    #[test]
    fn room_noise_alone_never_triggers() {
        // 60 RMS is what this board's microphone reports in a quiet room, and it
        // is exactly the level that kept setting the frame talking.
        let mut detector = Detector::new(RATE, DEFAULT_THRESHOLD);
        assert_eq!(feed(&mut detector, &frames(60, 250)), Vec::new());
        assert!(detector.floor() < 100.0, "floor={}", detector.floor());
        // A room this quiet never lifts the gate: the configured floor is already
        // above four times the noise, so 600 stays in force and speech clears it.
        assert_eq!(detector.trigger(), DEFAULT_THRESHOLD);
    }

    #[test]
    fn speech_over_that_noise_still_triggers() {
        let mut detector = Detector::new(RATE, DEFAULT_THRESHOLD);
        let _ = feed(&mut detector, &frames(60, 250));
        let events = feed(&mut detector, &frames(900, 20));
        assert!(
            matches!(events.first(), Some(Event::Started)),
            "events={events:?}"
        );
    }

    #[test]
    fn a_steady_loud_room_stops_triggering() {
        // A television that never pauses: one capture is unavoidable before the
        // gate has heard the room, and after that the gate must be above it.
        let mut detector = Detector::new(RATE, DEFAULT_THRESHOLD);
        let _ = feed(&mut detector, &frames(2_000, 3 * 1_000 / FRAME_MS));
        assert!(detector.floor() > 1_000.0, "floor={}", detector.floor());
        assert!(
            detector.trigger() > 2_000.0,
            "trigger={}",
            detector.trigger()
        );
    }

    #[test]
    fn a_quiet_voice_after_a_loud_room_is_heard_again() {
        // Quiet frames are believed immediately, so the gate comes back down as
        // soon as the noise stops rather than staying at the television's level.
        let mut detector = Detector::new(RATE, DEFAULT_THRESHOLD);
        let _ = feed(&mut detector, &frames(2_000, 3 * 1_000 / FRAME_MS));
        let _ = feed(&mut detector, &frames(60, 10));
        let events = feed(&mut detector, &frames(900, 20));
        assert!(
            matches!(events.first(), Some(Event::Started)),
            "events={events:?}"
        );
    }

    #[test]
    fn a_live_threshold_takes_effect_without_losing_the_room() {
        // Raising the gate above the signal must silence it at once, and lowering
        // it must hear the same frames again: this is the knob the console exposes,
        // so it must work on a running detector and keep the room estimate.
        let mut detector = Detector::new(RATE, DEFAULT_THRESHOLD);
        let _ = feed(&mut detector, &frames(60, 10));
        let floor = detector.floor();

        detector.set_threshold(8_000.0);
        assert_eq!(detector.floor(), floor, "the room estimate must survive");
        assert_eq!(detector.trigger(), 8_000.0);
        assert_eq!(feed(&mut detector, &speech_frames(50)), Vec::new());

        // Silence again, so the room estimate is low for the second half.
        let _ = feed(&mut detector, &frames(60, 10));
        detector.set_threshold(200.0);
        assert!(
            (200.0..=260.0).contains(&detector.trigger()),
            "trigger={}",
            detector.trigger()
        );
        let events = feed(&mut detector, &speech_frames(20));
        assert!(
            matches!(events.first(), Some(Event::Started)),
            "events={events:?}"
        );
    }

    #[test]
    fn a_threshold_above_the_signal_keeps_it_quiet() {
        let mut detector = Detector::new(RATE, 8_000.0);
        assert_eq!(feed(&mut detector, &speech_frames(50)), Vec::new());
        assert_eq!(detector.state_name(), "idle");
    }

    #[test]
    fn reset_forgets_an_unfinished_utterance() {
        let mut detector = Detector::new(RATE, DEFAULT_THRESHOLD);
        let needed = MIN_SPEECH_MS.div_ceil(FRAME_MS);
        for _ in 0..needed {
            let _ = detector.push(&frame(4_000));
        }
        assert_eq!(detector.state_name(), "speech");
        detector.reset();
        assert_eq!(detector.state_name(), "idle");
        assert!(detector.utterance.is_empty());
    }

    #[test]
    fn rms_is_the_root_of_the_mean_square() {
        assert!((rms(&[3, 4]) - (12.5_f32).sqrt()).abs() < 1e-3);
        assert_eq!(rms(&[]), 0.0);
        assert!((rms(&[1_000; 10]) - 1_000.0).abs() < 1e-3);
    }
}
