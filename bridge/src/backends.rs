//! The three backends, behind traits so the protocol can be exercised before any
//! model exists.
//!
//! The stubs are not placeholders to be thrown away: they are how the firmware's
//! request shape gets tested today. `stub` speech recognition reports the duration it
//! received, which proves the audio arrived and was compressed correctly; `stub`
//! synthesis answers with a tone, which proves the device can decode and play what
//! the bridge returns. Swapping in real models changes nothing on the device side.

use crate::ulaw;
use std::io::Write;
use std::process::{Command, Stdio};

/// Speech to text, 16 kHz mono µ-law in, plain text out.
pub trait Stt: Send {
    fn transcribe(&mut self, utterance: &[u8], lang: &str) -> Result<String, String>;

    /// Two-letter language detected by the recogniser, when it can detect one.
    ///
    /// A recogniser with a fixed language returns `None`; auto-detecting Whisper
    /// returns the code it used, which is what lets `/tts` pick a matching voice
    /// instead of trusting the frame's stored `ai.lang`.
    /// Words the recogniser should expect (names, the wake word); a Whisper
    /// prompt. Backends without one ignore it.
    fn set_prompt(&mut self, _prompt: &str) {}

    fn language(&self) -> Option<&str> {
        None
    }
}

/// Language model, plain text prompt in, plain text reply out.
pub trait Llm: Send {
    fn reply(&mut self, prompt: &str, model: &str) -> Result<String, String>;

    /// Extra environment for a command backend (`PF_LLM_SYSTEM`, `PF_LLM_URL`
    /// from the settings); other backends have nothing to configure.
    fn set_env(&mut self, _env: &[(String, String)]) {}
}

/// Text to speech, plain text in, 16 kHz mono µ-law out.
pub trait Tts: Send {
    fn speak(&mut self, text: &str, voice: &str, lang: &str) -> Result<Vec<u8>, String>;
}

/// Rate the whole assistant path runs at, and the rate backends must produce.
pub const ASSISTANT_RATE: u32 = 16_000;

/// Recogniser that only reports what it received.
///
/// Silence and a failing microphone look identical from the device's side, so the
/// report includes the peak level: a transcription of "0.0 s, peak 0" says the audio
/// never arrived, while "2.5 s, peak 9000" says the wire is fine and only the model
/// is missing.
pub struct StubStt;

impl Stt for StubStt {
    fn transcribe(&mut self, utterance: &[u8], lang: &str) -> Result<String, String> {
        let seconds = utterance.len() as f64 / f64::from(ASSISTANT_RATE);
        let peak = ulaw::peak(utterance);
        Ok(format!(
            "stub: получено {seconds:.2} с, пик {peak}, язык {lang}"
        ))
    }
}

/// Model that answers without thinking, for wire tests.
pub struct StubLlm;

impl Llm for StubLlm {
    fn reply(&mut self, prompt: &str, model: &str) -> Result<String, String> {
        let words = prompt.split_whitespace().count();
        Ok(format!(
            "Это ответ заглушки на {words} слов, модель {model}. Проверка связи завершена."
        ))
    }
}

/// Synthesiser that answers with a tone instead of a voice.
///
/// A tone is deliberate: silence would be indistinguishable from a broken playback
/// path, and a spoken stubbed word would need a model anyway.
pub struct StubTts;

impl Tts for StubTts {
    fn speak(&mut self, text: &str, voice: &str, lang: &str) -> Result<Vec<u8>, String> {
        // One tenth of a second per character, capped at three seconds, so a longer
        // reply audibly lasts longer and the timing is visible in the logs.
        let duration = (text.chars().count() as f64 / 10.0).clamp(0.3, 3.0);
        let frames = (duration * f64::from(ASSISTANT_RATE)) as usize;
        let mut pcm = Vec::with_capacity(frames * 2);
        for index in 0..frames {
            let phase = index as f64 / f64::from(ASSISTANT_RATE) * 440.0 * std::f64::consts::TAU;
            let sample = (phase.sin() * 8_000.0) as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        let encoded = ulaw::encode_from_pcm(&pcm);
        eprintln!(
            "  tts stub: voice={voice} lang={lang} chars={} frames={frames} ulaw={}",
            text.chars().count(),
            encoded.len()
        );
        Ok(encoded)
    }
}

/// Synthesiser and model that delegate to an external program through stdin/stdout.
///
/// This is how a real Piper voice or a local llama.cpp server gets wired in without
/// the bridge linking anything: the program reads the text on stdin and writes raw
/// 16-bit mono 16 kHz PCM (TTS) or plain text (model) on stdout. Arguments are split
/// on whitespace and executed directly, never through a shell, so a transcript cannot
/// become a command.
pub struct CommandBackend {
    program: String,
    args: Vec<String>,
    /// Settings-supplied environment, on top of the inherited one.
    env: Vec<(String, String)>,
}

impl CommandBackend {
    /// `spec` is `cmd:<program> [args...]`; the arguments are split on whitespace.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let rest = spec
            .strip_prefix("cmd:")
            .ok_or_else(|| format!("backend {spec}: expected stub or cmd:<program> [args...]"))?;
        let mut parts = rest.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| format!("backend {spec}: no program after cmd:"))?
            .to_owned();
        Ok(Self {
            program,
            args: parts.map(str::to_owned).collect(),
            env: Vec::new(),
        })
    }

    /// Run the program, handing it `env` in addition to the inherited environment.
    ///
    /// The recogniser already receives `LANGS`, and synthesis receives `VOICES`: a
    /// voice is a property of the request, not of the bridge's command line, so the
    /// program has to be told which one this sentence wants. Both are hints — a
    /// backend that has only one voice ignores them, which is why the espeak script
    /// still works unchanged.
    fn run(&self, input: &str, env: &[(&str, &str)]) -> Result<Vec<u8>, String> {
        let mut child = Command::new(&self.program)
            .args(&self.args)
            .envs(
                self.env
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("spawn {}: {error}", self.program))?;
        {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| format!("{}: no stdin", self.program))?;
            stdin
                .write_all(input.as_bytes())
                .map_err(|error| format!("write to {}: {error}", self.program))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| format!("wait for {}: {error}", self.program))?;
        if !output.status.success() {
            return Err(format!("{} exited with {}", self.program, output.status));
        }
        Ok(output.stdout)
    }
}

impl Tts for CommandBackend {
    fn speak(&mut self, text: &str, voice: &str, lang: &str) -> Result<Vec<u8>, String> {
        let pcm = self.run(text, &[("VOICES", voice), ("LANGS", lang)])?;
        if pcm.len() < 2 {
            return Err(format!("{} produced no audio", self.program));
        }
        // The hint is what the frame asked for, not necessarily what spoke: this
        // backend is one fixed program, and the program reports its own choice.
        eprintln!(
            "  tts cmd: voice_hint={voice} lang={lang} pcm={} bytes -> ulaw={}",
            pcm.len(),
            pcm.len() / 2
        );
        Ok(ulaw::encode_from_pcm(&pcm))
    }
}

impl Llm for CommandBackend {
    fn set_env(&mut self, env: &[(String, String)]) {
        env.clone_into(&mut self.env);
    }

    fn reply(&mut self, prompt: &str, model: &str) -> Result<String, String> {
        let stdout = self.run(prompt, &[])?;
        let text = String::from_utf8(stdout)
            .map_err(|_| format!("{} produced non-UTF-8 output", self.program))?;
        let text = text.trim().to_owned();
        if text.is_empty() {
            return Err(format!("{} produced an empty reply", self.program));
        }
        eprintln!(
            "  llm cmd: model_hint={model} reply={} chars",
            text.chars().count()
        );
        Ok(text)
    }
}

impl Stt for CommandBackend {
    fn transcribe(&mut self, utterance: &[u8], lang: &str) -> Result<String, String> {
        // A command recogniser needs samples, not companded bytes, so the µ-law is
        // expanded here: whisper-family models all take 16-bit PCM.
        let pcm = ulaw::decode_to_pcm(utterance);
        let mut child = Command::new(&self.program)
            .args(&self.args)
            .env("LANGS", lang)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("spawn {}: {error}", self.program))?;
        {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| format!("{}: no stdin", self.program))?;
            stdin
                .write_all(&pcm)
                .map_err(|error| format!("write to {}: {error}", self.program))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| format!("wait for {}: {error}", self.program))?;
        if !output.status.success() {
            return Err(format!("{} exited with {}", self.program, output.status));
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|_| format!("{} produced non-UTF-8 output", self.program))?;
        let text = text.trim().to_owned();
        if text.is_empty() {
            return Err(format!("{} heard nothing", self.program));
        }
        Ok(text)
    }
}
