//! What the frames asked and what they were told, for the web UI.
//!
//! A frame's voice turn is three requests: `/stt` (audio -> transcript), `/llm`
//! (transcript -> answer), `/tts` (answer -> speech). They arrive one after
//! another from the same address, so a turn is assembled by peer: `/stt` opens a
//! turn, and `/llm` and `/tts` from the same peer within [`TURN_WINDOW`] fill it in.
//! A rejected `/stt` ("not addressed", "no speech") is a turn too, closed at once:
//! seeing what the bridge refused is half of tuning the wake word and the gate.
//! An `/llm` with no open turn (`ai ask` typed on the console) starts one.
//!
//! Kept in memory, newest last, bounded by [`CAPACITY`].

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Turns kept for the UI.
pub const CAPACITY: usize = 200;
/// How long after `/stt` the `/llm` and `/tts` of the same turn may arrive.
pub const TURN_WINDOW: Duration = Duration::from_secs(120);
/// Longest text kept per field; answers are short, prompts can be long.
const MAX_FIELD: usize = 2_000;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Turn {
    pub id: u64,
    /// Unix time of the first request, in milliseconds.
    pub at_ms: u64,
    /// Who asked: the frame's address.
    pub peer: String,
    /// `key`, `voice`, `console`, or empty (older frame, or text only).
    pub trigger: String,
    /// What the recogniser heard, wake word included.
    pub heard: Option<String>,
    /// Why the transcript went no further, when it did not.
    pub rejected: Option<String>,
    pub answer: Option<String>,
    pub stt_ms: Option<u64>,
    pub llm_ms: Option<u64>,
    pub tts_ms: Option<u64>,
    /// Bytes of µ-law speech sent back.
    pub speech_bytes: Option<usize>,
    pub error: Option<String>,
    #[serde(skip)]
    opened: Option<Instant>,
}

#[derive(Debug, Default)]
pub struct History {
    turns: VecDeque<Turn>,
    next_id: u64,
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_FIELD) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

impl History {
    fn open(&mut self, peer: &str, trigger: &str) -> &mut Turn {
        self.next_id += 1;
        if self.turns.len() == CAPACITY {
            self.turns.pop_front();
        }
        self.turns.push_back(Turn {
            id: self.next_id,
            at_ms: now_ms(),
            peer: peer.to_owned(),
            trigger: trigger.to_owned(),
            heard: None,
            rejected: None,
            answer: None,
            stt_ms: None,
            llm_ms: None,
            tts_ms: None,
            speech_bytes: None,
            error: None,
            opened: Some(Instant::now()),
        });
        self.turns.back_mut().expect("just pushed")
    }

    /// The open turn of `peer` that is still waiting for `field`.
    fn open_turn(&mut self, peer: &str, waiting: fn(&Turn) -> bool) -> Option<&mut Turn> {
        self.turns.iter_mut().rev().find(|turn| {
            turn.peer == peer
                && turn.rejected.is_none()
                && turn.error.is_none()
                && turn.opened.is_some_and(|at| at.elapsed() < TURN_WINDOW)
                && waiting(turn)
        })
    }

    /// `/stt` answered: `heard` is the transcript, `rejected` the reason it went
    /// no further (or `None` when it was passed on).
    pub fn heard(
        &mut self,
        peer: &str,
        trigger: &str,
        heard: Option<&str>,
        rejected: Option<&str>,
        ms: u64,
    ) {
        let turn = self.open(peer, trigger);
        turn.heard = heard.map(clip);
        turn.rejected = rejected.map(clip);
        turn.stt_ms = Some(ms);
    }

    /// `/llm` answered (or failed).
    pub fn answered(&mut self, peer: &str, prompt: &str, result: Result<&str, &str>, ms: u64) {
        let turn = match self.open_turn(peer, |turn| turn.answer.is_none() && turn.heard.is_some())
        {
            Some(turn) => turn,
            None => {
                // No transcript before it: a typed question.
                let turn = self.open(peer, "");
                turn.heard = Some(clip(prompt));
                turn
            }
        };
        turn.llm_ms = Some(ms);
        match result {
            Ok(answer) => turn.answer = Some(clip(answer)),
            Err(error) => turn.error = Some(clip(error)),
        }
    }

    /// `/tts` answered (or failed).
    pub fn spoke(&mut self, peer: &str, result: Result<usize, &str>, ms: u64) {
        let Some(turn) =
            self.open_turn(peer, |turn| turn.answer.is_some() && turn.tts_ms.is_none())
        else {
            return;
        };
        turn.tts_ms = Some(ms);
        match result {
            Ok(bytes) => turn.speech_bytes = Some(bytes),
            Err(error) => turn.error = Some(clip(error)),
        }
    }

    /// The newest `limit` turns, newest first.
    pub fn recent(&self, limit: usize) -> Vec<Turn> {
        self.turns.iter().rev().take(limit).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_requests_from_one_frame_make_one_turn() {
        let mut history = History::default();
        history.heard(
            "10.0.0.5",
            "key",
            Some("Сколько будет 2 плюс 2?"),
            None,
            3_400,
        );
        history.answered("10.0.0.5", "Сколько будет 2 плюс 2?", Ok("Четыре."), 620);
        history.spoke("10.0.0.5", Ok(42_168), 1_229);
        let turns = history.recent(10);
        assert_eq!(turns.len(), 1);
        let turn = &turns[0];
        assert_eq!(turn.trigger, "key");
        assert_eq!(turn.answer.as_deref(), Some("Четыре."));
        assert_eq!(
            (turn.stt_ms, turn.llm_ms, turn.tts_ms),
            (Some(3_400), Some(620), Some(1_229))
        );
        assert_eq!(turn.speech_bytes, Some(42_168));
    }

    #[test]
    fn a_rejection_is_kept_and_does_not_swallow_the_next_turn() {
        let mut history = History::default();
        history.heard(
            "10.0.0.5",
            "voice",
            Some("Мама, дай сок."),
            Some("not addressed"),
            2_000,
        );
        history.heard("10.0.0.5", "voice", Some("Рамка, привет"), None, 2_000);
        history.answered("10.0.0.5", "привет", Ok("Привет!"), 500);
        let turns = history.recent(10);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].answer.as_deref(), Some("Привет!"));
        assert_eq!(turns[1].rejected.as_deref(), Some("not addressed"));
        assert_eq!(turns[1].answer, None);
    }

    #[test]
    fn frames_do_not_mix_and_a_typed_question_is_its_own_turn() {
        let mut history = History::default();
        history.heard(
            "10.0.0.5",
            "voice",
            Some("Рамка, который час?"),
            None,
            1_000,
        );
        history.answered("10.0.0.6", "Скажи одно слово.", Ok("Слово."), 300);
        history.answered("10.0.0.5", "который час?", Ok("Десять."), 400);
        let turns = history.recent(10);
        assert_eq!(turns.len(), 2);
        let typed = turns.iter().find(|turn| turn.peer == "10.0.0.6").unwrap();
        assert_eq!(typed.heard.as_deref(), Some("Скажи одно слово."));
        let spoken = turns.iter().find(|turn| turn.peer == "10.0.0.5").unwrap();
        assert_eq!(spoken.answer.as_deref(), Some("Десять."));
    }

    #[test]
    fn the_buffer_is_bounded() {
        let mut history = History::default();
        for _ in 0..CAPACITY + 5 {
            history.heard("p", "voice", Some("x"), Some("no speech"), 1);
        }
        assert_eq!(history.recent(usize::MAX).len(), CAPACITY);
        assert_eq!(history.recent(1)[0].id, (CAPACITY + 5) as u64);
    }
}
