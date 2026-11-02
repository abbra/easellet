//! Recognise one audio file and print what came back.
//!
//! `cargo run --release --features whisper --example transcribe -- <model> <lang> <file>...`
//!
//! `PF_STT_HINT=ru` gives detection the bridge's language head start and
//! `PF_STT_PROMPT="Рамка, …"` the bridge's prompt, so a capture can be replayed
//! with and without them.
//!
//! Exists to test recognition without the frame: the device's own `/stt` path is a
//! µ-law upload, and when it comes back empty it is worth knowing whether the audio,
//! the sample rate or the model is at fault. This reads any file `rodio` understands,
//! shows the sample count and the model's per-segment output, and prints the text.

use std::fs::File;
use std::io::BufReader;

use futures_util::StreamExt;
use rwhisper::{WhisperBuilder, WhisperLanguage, WhisperSource};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "small".to_owned());
    let language = args.next().unwrap_or_else(|| "ru".to_owned());
    // Scratch lives under ~/tmp/frame, not /tmp: a capture that reproduces a
    // misheard word is worth keeping for the next comparison.
    let mut paths: Vec<String> = args.collect();
    if paths.is_empty() {
        paths.push(format!(
            "{}/tmp/frame/stt-wav/speech.wav",
            std::env::var("HOME").unwrap_or_else(|_| ".".to_owned())
        ));
    }

    let source = match name.as_str() {
        "tiny" => WhisperSource::QuantizedTiny,
        "base" => WhisperSource::Base,
        "small" => WhisperSource::Small,
        "medium" => WhisperSource::Medium,
        other => panic!("unknown model {other}: tiny, base, small or medium"),
    };
    let language = match language.as_str() {
        "auto" => None,
        "ru" => Some(WhisperLanguage::Russian),
        "en" => Some(WhisperLanguage::English),
        "fi" => Some(WhisperLanguage::Finnish),
        "fr" => Some(WhisperLanguage::French),
        other => panic!("unknown language {other}: auto, ru, en, fi or fr"),
    };

    let runtime = tokio::runtime::Runtime::new()?;
    let model = runtime.block_on(async {
        WhisperBuilder::default()
            .with_source(source)
            .with_language(language)
            // Same restriction as the bridge, so a test here reproduces what the
            // frame would have heard rather than a friendlier detector.
            .with_language_filter(
                std::env::var("PF_STT_LANGS")
                    .unwrap_or_else(|_| "ru,en,fi,fr".to_owned())
                    .split(',')
                    .map(str::trim)
                    .filter(|code| code.len() == 2)
                    .map(str::to_owned)
                    .collect(),
            )
            .build()
            .await
    })?;
    model.set_language_hint(std::env::var("PF_STT_HINT").ok().as_deref());
    model.set_prompt(&std::env::var("PF_STT_PROMPT").unwrap_or_default());

    for path in paths {
        let decoder = rodio::Decoder::new(BufReader::new(File::open(&path)?))?;
        eprintln!(
            "{path}: {} Hz, {} ch",
            rodio::Source::sample_rate(&decoder),
            rodio::Source::channels(&decoder)
        );
        let mut stream = model.transcribe(decoder);
        let mut segments = 0_usize;
        let text: String = runtime.block_on(async {
            let mut text = String::new();
            while let Some(segment) = stream.next().await {
                segments += 1;
                eprintln!(
                    "segment {segments} at {:.2}s: {:?}",
                    segment.start(),
                    segment.text()
                );
                text.push_str(segment.text());
            }
            text
        });
        eprintln!(
            "segments={segments} chars={} language={:?}",
            text.chars().count(),
            model.last_language()
        );
        println!("{}", text.trim());
    }
    Ok(())
}
