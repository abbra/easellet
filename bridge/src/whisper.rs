//! Real recognition through `rwhisper`, behind the `whisper` feature.
//!
//! Candle-based Whisper in pure Rust: no whisper.cpp build, no ffmpeg, no Python.
//! The model is multilingual and auto-detects the language of each utterance. A
//! fixed language is still accepted by the builder, but the frame's household asks
//! in Russian, Finnish, English and French, so detection is the default here.
//!
//! `rwhisper` is vendored in `vendor/rwhisper` rather than fetched: upstream 0.4.1
//! never puts the `<|transcribe|>` token in the decode prompt, so the language token is
//! the only conditioning and the model answers as if asked to transcribe English. Ten
//! seconds of Russian came back as English prose on `small` and as two-word fragments on
//! `tiny`; one token fixed it. See `vendor/rwhisper.patch`.
//!
//! Model size is not a taste question here: `tiny` and `base` could not carry Russian
//! speech even with the patch, so `small` is the smallest size worth documenting. On one
//! core, `small` transcribes a 10 s utterance in about 8 s - the frame's own budget has
//! to allow for that.

use crate::backends::{ASSISTANT_RATE, Stt};
use crate::ulaw;
use futures_util::StreamExt;
use rwhisper::{Whisper, WhisperBuilder, WhisperSource};

/// Turn on the log records the model emits.
///
/// `rwhisper` reports a failed decoding pass with `tracing::error!` and nothing else,
/// so without a subscriber a broken inference is indistinguishable from silence: the
/// stream simply ends with no transcript. Initialised once, and it never overrides an
/// existing subscriber.
fn init_logging() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_target(false)
            .try_init();
    });
}

/// Languages auto-detection may pick, from `PF_STT_LANGS`.
///
/// Comma-separated two-letter codes. The default is the household's own set; an
/// empty value restores Whisper's unrestricted behaviour.
fn supported_languages() -> Vec<String> {
    let spec = std::env::var("PF_STT_LANGS").unwrap_or_else(|_| "ru,en,fi,fr".to_owned());
    spec.split(',')
        .map(str::trim)
        .filter(|code| code.len() == 2)
        .map(str::to_owned)
        .collect()
}

pub struct WhisperStt {
    runtime: tokio::runtime::Runtime,
    model: Whisper,
    label: String,
    last_language: Option<String>,
}

impl WhisperStt {
    /// `name` is one of the model sizes rwhisper ships, downloaded on first use.
    ///
    /// `small` is the documented default; see the module note on `tiny` and `base`.
    pub fn new(name: &str) -> Result<Self, String> {
        let source = match name {
            "tiny" => WhisperSource::QuantizedTiny,
            "base" => WhisperSource::Base,
            "small" => WhisperSource::Small,
            "medium" => WhisperSource::Medium,
            other => {
                return Err(format!(
                    "unknown whisper model {other}: use tiny, base, small or medium"
                ));
            }
        };
        init_logging();
        let runtime =
            tokio::runtime::Runtime::new().map_err(|error| format!("tokio runtime: {error}"))?;
        eprintln!("loading whisper model {name} (first run downloads it)...");
        let model = runtime
            .block_on(async {
                WhisperBuilder::default()
                    .with_source(source)
                    // `None` is auto-detection, now that the vendored decoder
                    // implements it: Russian, Finnish, English and French all come
                    // out of the same model, and the next utterance re-detects.
                    .with_language(None)
                    // Auto-detection alone is not enough in a room with a radio in it:
                    // the detector scores ~100 languages, and on noise the winner is
                    // routinely Korean or Norwegian. The decoder then conditions the
                    // transcription on that choice and produces invented prose in it,
                    // which the answer follows. Naming the four languages this
                    // household speaks removes that class of nonsense;
                    // `PF_STT_LANGS` overrides it for a different house.
                    .with_language_filter(supported_languages())
                    .build()
                    .await
            })
            .map_err(|error| format!("load whisper {name}: {error}"))?;
        eprintln!("whisper model {name} ready");
        Ok(Self {
            runtime,
            model,
            label: name.to_owned(),
            last_language: None,
        })
    }
}

impl Stt for WhisperStt {
    fn set_prompt(&mut self, prompt: &str) {
        self.model.set_prompt(prompt);
    }

    fn language(&self) -> Option<&str> {
        self.last_language.as_deref()
    }

    fn transcribe(&mut self, utterance: &[u8], lang: &str) -> Result<String, String> {
        // The device sends µ-law; the model wants 16-bit samples, and rwhisper
        // resamples to its own 16 kHz internally, so the rate here is already right.
        let samples: Vec<i16> = ulaw::decode_all(utterance);
        if samples.is_empty() {
            return Err("empty utterance".to_owned());
        }
        let seconds = samples.len() as f64 / f64::from(ASSISTANT_RATE);
        // Mono at the assistant rate; `Whisper::transcribe` wants any `rodio::Source`,
        // so the samples are wrapped rather than handed over as an iterator.
        let source = rodio::buffer::SamplesBuffer::new(1, ASSISTANT_RATE, samples);
        // The frame's language ("ru-RU") is a prior, not a choice: the
        // household speaks four languages, but a lone «Рамка» is Russian.
        let hint = lang.split(['-', '_']).next().filter(|code| code.len() == 2);
        self.model
            .set_language_hint(hint.map(str::to_ascii_lowercase).as_deref());
        let mut stream = self.model.transcribe(source);
        // Segments arrive as the decoder emits them, so counting them separates "the
        // decode failed" from "the decode produced blank text": both end with an empty
        // transcript, and only one of them is a bug here.
        let (text, segments) = self.runtime.block_on(async {
            let mut text = String::new();
            let mut segments = 0_usize;
            while let Some(segment) = stream.next().await {
                segments += 1;
                text.push_str(segment.text());
            }
            (text, segments)
        });
        let text = text.trim().to_owned();
        self.last_language = self.model.last_language();
        eprintln!(
            "  stt whisper-{}: {seconds:.2} s, hint={lang} detected={} segments={segments}, {} chars",
            self.label,
            self.last_language.as_deref().unwrap_or("?"),
            text.chars().count()
        );
        if text.is_empty() {
            return Err("whisper heard nothing".to_owned());
        }
        Ok(text)
    }
}
