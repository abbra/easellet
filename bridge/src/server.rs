//! Request routing, kept free of sockets so the protocol is unit-testable.

use crate::backends::{ASSISTANT_RATE, Llm, Stt, Tts};
use crate::history::History;
use crate::settings::Live;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Longest utterance accepted: 60 s of 16 kHz µ-law.
pub const MAX_UTTERANCE: usize = ASSISTANT_RATE as usize * 60;
/// Longest prompt or synthesis request accepted, in bytes.
pub const MAX_TEXT: usize = 8_192;

/// Letters or digits a transcript needs before it counts as speech.
///
/// The recogniser answers noise with something rather than nothing, and its
/// favourite nothings are a lone full stop or a single letter: a 5 s capture of a
/// room came back as `" ."`, was handed on as a prompt, and the model answered it
/// with "How can I help you?". Two keep a real «Да» and drop the rest. This is a
/// second line of defence behind the device's voice detector, not a substitute
/// for it: it cannot tell a hallucinated sentence from a question.
pub const MIN_TRANSCRIPT_CHARS: usize = 2;

/// Whole transcripts that are the recogniser's repertoire for noise.
///
/// Whisper does not answer noise with an empty string, it answers it in English:
/// a noisy 2 s capture of a question became `"Mm-hmm"`, and the model turned that
/// into "I'm here to help." — spoken aloud, in English, to a child who had just
/// asked something in Russian. The signatures are short, closed-class phrases with
/// no content in them, and none of them is a question, so dropping them costs a
/// real turn only when the whole turn was a grunt. Russian fillers are deliberately
/// absent: «Да» is an answer, and `MIN_TRANSCRIPT_CHARS` keeps it.
///
/// Entries are compared after `noise_key`: lowercase, hyphens read as spaces, every
/// other non-alphanumeric character dropped.
pub const NOISE_TRANSCRIPTS: &[&str] = &[
    "mm hmm",
    "mhm",
    "mm",
    "hmm",
    "hm",
    "uh huh",
    "uhhuh",
    "uh",
    "ah",
    "oh",
    "ooh",
    "ha",
    "thank you",
    "thank you very much",
    "thanks",
    "no",
    "yes",
    "yeah",
    "yep",
    "bye",
    "bye bye",
    "hello",
    "hi",
    "okay",
    "ok",
    "so",
    "well",
    "alright",
    ".",
];

/// Reduce a transcript to the form `NOISE_TRANSCRIPTS` is written in.
///
/// Punctuation is dropped rather than folded, because "Mm-hmm." and "Thank you!"
/// are the same non-answer as their bare forms.
pub fn noise_key(text: &str) -> String {
    let mut key = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_alphanumeric() {
            key.extend(character.to_lowercase());
        } else if character == '-' || character.is_whitespace() {
            key.push(' ');
        }
    }
    key.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Is this whole transcript one of the recogniser's noise answers?
pub fn is_noise_transcript(text: &str) -> bool {
    let key = noise_key(text);
    NOISE_TRANSCRIPTS.contains(&key.as_str())
}

pub struct Bridge {
    pub stt: Box<dyn Stt>,
    pub llm: Box<dyn Llm>,
    pub tts: Box<dyn Tts>,
    /// Shared token, required when set.
    pub token: Option<String>,
    /// Language the last recogniser run detected, used for the next `/tts`.
    pub last_lang: Option<String>,
    /// Stems that must appear in a transcript before it counts as addressed.
    ///
    /// Loudness cannot tell a question aimed at the frame from the same words
    /// said to somebody else: measured here, room conversation and a question
    /// from a metre away both sit at 150..460 RMS, so any gate loose enough to
    /// hear the question also opens on the conversation, and the house gets
    /// answers nobody asked for. A wake stem is the one signal that separates
    /// the two, and it costs no firmware: the detector still opens the
    /// microphone, but a transcript that does not contain the stem is dropped
    /// here and the frame never speaks.
    ///
    /// Empty means "everything is addressed", which is what a bare
    /// `photoframe-bridge` without `PF_WAKE` does. Stems rather than whole
    /// words, because Russian inflects: `рамк` covers «рамка», «рамку», «рамке».
    pub wake: Vec<String>,
    /// When the frame was last called by name on its own, if recently.
    pub addressed_at: Option<Instant>,
    /// Settings the web UI may change while the bridge runs; copied into the
    /// fields above at the start of every request. `None` in tests.
    pub live: Option<Arc<RwLock<Live>>>,
    /// Where turns are recorded for the UI. `None` in tests.
    pub history: Option<Arc<Mutex<History>>>,
    /// The last transcript `/stt` produced, accepted or not, for the history.
    pub last_heard: Option<String>,
    /// Plugins as the assistant's tools: given the question and the frame's
    /// address, the spoken reply when a plugin takes the question. `None` in tests.
    pub tools: Option<Arc<Tool>>,
}

/// See [`Bridge::tools`].
pub type Tool = dyn Fn(&str, &str) -> Option<String> + Send + Sync;

/// How long a bare "Рамка," keeps the door open for the question after it.
///
/// People pause after a name. Without this, «Рамка, — из чего делают торт?»
/// arrives as two captures: the first is the name alone and the model answers a
/// question nobody asked ("Рамка — это…"), the second is the real question with
/// no name in it and is dropped. Ten seconds covers the pause, an "э-э", and a
/// child thinking; it is short enough that the room does not inherit the door.
pub const WAKE_WINDOW: Duration = Duration::from_secs(10);

/// Byte ranges of the whitespace-separated words in `text`.
fn word_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (index, character) in text.char_indices() {
        if character.is_whitespace() {
            if let Some(begin) = start.take() {
                spans.push((begin, index));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(begin) = start {
        spans.push((begin, text.len()));
    }
    spans
}

/// Levenshtein distance, on characters, with the two rows that matter.
fn edits(one: &[char], other: &[char]) -> usize {
    let mut previous: Vec<usize> = (0..=other.len()).collect();
    let mut current = vec![0; other.len() + 1];
    for (row, left) in one.iter().enumerate() {
        current[0] = row + 1;
        for (column, right) in other.iter().enumerate() {
            let substitution = previous[column] + usize::from(left != right);
            current[column + 1] = substitution
                .min(previous[column + 1] + 1)
                .min(current[column] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[other.len()]
}

/// Is `word` one misheard letter away from `stem`?
///
/// The name has to survive the recogniser, not the other way round: in this room
/// Whisper-small heard «Рамка» as «Ранка», and a wake word that must be spelled
/// perfectly simply deafens the frame. One substitution, insertion or deletion is
/// what a recogniser actually does to a short word. The first letter must match,
/// which is what stops «мамка» and «рука» from counting as «рамка», and only the
/// stem's length is compared, because the stem is a prefix of the name.
fn off_by_one(word: &str, stem: &str) -> bool {
    // Soft and hard signs are what a recogniser drops and adds at random, and they
    // cost a whole edit each: «Ранка» and «Ранька» are the same name heard twice.
    let letters = |text: &str| -> Vec<char> {
        text.chars()
            .filter(|character| !matches!(character, 'ь' | 'ъ'))
            .collect()
    };
    let word = letters(word);
    let stem = letters(stem);
    if stem.is_empty() || word.len() < stem.len() || word[0] != stem[0] {
        return false;
    }
    edits(&word[..stem.len()], &stem) <= 1
}

impl Bridge {
    /// Byte range of the first word that carries a wake stem.
    #[must_use]
    pub fn wake_span(&self, text: &str) -> Option<(usize, usize)> {
        word_spans(text).into_iter().find(|(start, end)| {
            let word: String = text[*start..*end]
                .chars()
                .filter(|character: &char| character.is_alphanumeric())
                .collect::<String>()
                .to_lowercase();
            self.wake
                .iter()
                .any(|stem| word.contains(stem.as_str()) || off_by_one(&word, stem))
        })
    }

    /// Did this transcript call the frame by name — or come while it was called?
    #[must_use]
    pub fn is_addressed(&self, text: &str) -> bool {
        if self.wake.is_empty() || self.wake_span(text).is_some() {
            return true;
        }
        self.addressed_at
            .is_some_and(|at| at.elapsed() < WAKE_WINDOW)
    }

    /// The transcript without the wake word, for the model and the panel.
    ///
    /// The name is how the frame is called, not part of the question: leaving it
    /// in asks the model about «Рамка» as often as about the cake. Whole words are
    /// dropped, not stems, so «фоторамка» does not leave a «фото» behind.
    #[must_use]
    pub fn without_wake(&self, text: &str) -> String {
        let Some((start, end)) = self.wake_span(text) else {
            return text.to_owned();
        };
        let rest = format!("{}{}", &text[..start], &text[end..]);
        rest.trim_start_matches(|character: char| {
            character.is_whitespace()
                || character.is_ascii_punctuation()
                || "—–,«»".contains(character)
        })
        .trim()
        .to_owned()
    }
}

/// One incoming request, as the router sees it.
pub struct Incoming<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// Raw `Authorization` header value.
    pub auth: Option<&'a str>,
    pub lang: &'a str,
    pub voice: &'a str,
    pub model: &'a str,
    /// The frame's address, for grouping a turn's requests in the history.
    pub peer: &'a str,
    /// What started the capture: `key`, `voice` or `console` (`X-Trigger`).
    /// Empty from an older frame, which is treated as `voice`.
    pub trigger: &'a str,
    pub body: Vec<u8>,
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// Request bytes consumed, carried here so the access log needs one value.
    pub bytes_in: usize,
    /// Extra headers; `X-Panel: keep` tells the frame a plugin owns the panel.
    pub headers: Vec<(&'static str, String)>,
}

impl Response {
    fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.into().into_bytes(),
            bytes_in: 0,
            headers: Vec::new(),
        }
    }

    fn audio(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: "audio/basic",
            body,
            bytes_in: 0,
            headers: Vec::new(),
        }
    }
}

impl Bridge {
    /// Route one request.
    ///
    /// Every failure is a status code with an explanatory body, because the only
    /// person who will read it is whoever is holding the frame's USB cable.
    pub fn dispatch(&mut self, mut request: Incoming<'_>) -> Response {
        if let Some(live) = &self.live
            && let Ok(live) = live.read()
        {
            self.token = live.token.clone();
            self.wake = live.wake.clone();
            self.llm.set_env(&live.llm_env);
            self.stt.set_prompt(&live.stt_prompt);
        }
        let bytes_in = request.body.len();
        let started = Instant::now();
        // The prompt is needed for the history after the body has been consumed.
        let prompt =
            (request.path == "/llm").then(|| String::from_utf8_lossy(&request.body).into_owned());
        self.last_heard = None;
        let (path, peer, trigger) = (request.path, request.peer, request.trigger);
        let mut response = self.route(&mut request);
        response.bytes_in = bytes_in;
        if response.status != 401 {
            self.record(path, peer, trigger, prompt.as_deref(), &response, started);
        }
        response
    }

    /// Note a finished `/stt`, `/llm` or `/tts` in the UI's history.
    fn record(
        &mut self,
        path: &str,
        peer: &str,
        trigger: &str,
        prompt: Option<&str>,
        response: &Response,
        started: Instant,
    ) {
        let Some(history) = &self.history else {
            return;
        };
        let Ok(mut history) = history.lock() else {
            return;
        };
        let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let body = String::from_utf8_lossy(&response.body);
        let reason = body.trim().strip_prefix("ERR ").unwrap_or(body.trim());
        match path {
            "/stt" if response.status == 200 => {
                history.heard(peer, trigger, self.last_heard.as_deref(), None, ms);
            }
            "/stt" => history.heard(peer, trigger, self.last_heard.as_deref(), Some(reason), ms),
            "/llm" => {
                let result = if response.status == 200 {
                    Ok(body.trim())
                } else {
                    Err(reason)
                };
                history.answered(peer, prompt.unwrap_or(""), result, ms);
            }
            "/tts" => {
                let result = if response.status == 200 {
                    Ok(response.body.len())
                } else {
                    Err(reason)
                };
                history.spoke(peer, result, ms);
            }
            _ => {}
        }
    }

    fn route(&mut self, request: &mut Incoming<'_>) -> Response {
        let method = request.method;
        let path = request.path;
        // `/health` answers before authentication: it is a liveness probe, not data.
        if path == "/health" {
            return Response::text(200, "ok\n");
        }
        if method != "POST" {
            return Response::text(405, "ERR only POST is supported\n");
        }
        if let Some(token) = &self.token {
            let expected = format!("Bearer {token}");
            match request.auth {
                Some(got) if constant_time_eq(got.as_bytes(), expected.as_bytes()) => {}
                _ => return Response::text(401, "ERR bad or missing token\n"),
            }
        }
        let lang = request.lang;
        // A press or a typed command is the frame being spoken to on purpose; only
        // a capture the loudness detector opened has to prove it with the name.
        let deliberate = matches!(request.trigger, "key" | "console");
        let voice = request.voice;
        let model = request.model;
        let body = std::mem::take(&mut request.body);

        match path {
            "/stt" => {
                if body.len() > MAX_UTTERANCE {
                    return Response::text(413, "ERR utterance too long\n");
                }
                if body.is_empty() {
                    return Response::text(400, "ERR empty utterance\n");
                }
                // `PF_BRIDGE_DUMP=<dir>` keeps the exact audio the device sent.
                // Without it there is no way to tell a failing microphone from a
                // failing recogniser: the response only ever says "heard nothing".
                if let Some(directory) = std::env::var_os("PF_BRIDGE_DUMP") {
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|elapsed| elapsed.as_millis())
                        .unwrap_or(0);
                    let path = std::path::Path::new(&directory).join(format!("stt-{stamp}.ulaw"));
                    match std::fs::write(&path, &body) {
                        Ok(()) => {
                            eprintln!("  stt dump: {} bytes -> {}", body.len(), path.display())
                        }
                        Err(error) => eprintln!("  stt dump failed: {error}"),
                    }
                }
                match self.stt.transcribe(&body, lang) {
                    Ok(text) => {
                        let letters = text
                            .chars()
                            .filter(|character| character.is_alphanumeric())
                            .count();
                        if letters < MIN_TRANSCRIPT_CHARS {
                            eprintln!("  stt rejected: {text:?} ({letters} letters) - not speech");
                            return Response::text(500, "ERR stt: no speech\n");
                        }
                        self.last_heard = Some(text.clone());
                        let named = self.wake_span(&text).is_some();
                        let question = self.without_wake(&text);
                        let asked = question
                            .chars()
                            .filter(|character| character.is_alphanumeric())
                            .count();
                        if named && asked < MIN_TRANSCRIPT_CHARS && !self.wake.is_empty() {
                            // The name on its own: the question is coming in the next
                            // capture, so hold the door open and say nothing now.
                            self.addressed_at = Some(Instant::now());
                            eprintln!("  stt: wake word only, listening window open");
                            return Response::text(500, "ERR stt: wake only\n");
                        }
                        if deliberate {
                            // The question may follow up by voice, as after the name.
                            self.addressed_at = Some(Instant::now());
                        } else if !self.is_addressed(&text) {
                            eprintln!("  stt rejected: {text:?} - not addressed to the frame");
                            return Response::text(500, "ERR stt: not addressed\n");
                        }
                        let text = question;
                        if is_noise_transcript(&text) {
                            // The detector heard something, but the recogniser only
                            // found a filler: this is a noisy moment, not a question.
                            eprintln!("  stt rejected: {text:?} - recogniser filler, not speech");
                            return Response::text(500, "ERR stt: no speech\n");
                        }
                        if let Some(code) = self.stt.language() {
                            self.last_lang = Some(code.to_owned());
                        }
                        // The text, not just its length: "the frame answered in
                        // English" is undiagnosable from a character count, and the
                        // recogniser's output is the only evidence of what it heard.
                        eprintln!(
                            "  stt text: {text:?} lang={} letters={letters}",
                            self.last_lang.as_deref().unwrap_or("?")
                        );
                        Response::text(200, text)
                    }
                    Err(error) => Response::text(500, format!("ERR stt: {error}\n")),
                }
            }
            "/llm" => {
                if body.len() > MAX_TEXT {
                    return Response::text(413, "ERR prompt too long\n");
                }
                let prompt = match String::from_utf8(body) {
                    Ok(prompt) if !prompt.trim().is_empty() => prompt,
                    Ok(_) => return Response::text(400, "ERR empty prompt\n"),
                    Err(_) => return Response::text(400, "ERR prompt is not UTF-8\n"),
                };
                // A question a plugin answers ("покажи погоду") goes to that
                // plugin instead of the model; the frame is told to leave its
                // panel alone, because the plugin's picture is on its way.
                // Commands for the frame itself come first: only the frame can
                // act on them, from a fixed list of actions (`commands.rs`).
                if let Some(command) = crate::commands::find(&prompt) {
                    eprintln!(
                        "  frame command: {}",
                        command.action.as_deref().unwrap_or("say")
                    );
                    let mut response = Response::text(200, command.reply);
                    if let Some(action) = command.action {
                        response.headers.push(("X-Action", action));
                        response.headers.push(("X-Panel", "keep".to_owned()));
                    }
                    return response;
                }
                if let Some(tools) = &self.tools
                    && let Some(reply) = tools(&prompt, request.peer)
                {
                    let mut response = Response::text(200, reply);
                    response.headers.push(("X-Panel", "keep".to_owned()));
                    return response;
                }
                match self.llm.reply(&prompt, model) {
                    Ok(text) => Response::text(200, text),
                    Err(error) => Response::text(500, format!("ERR llm: {error}\n")),
                }
            }
            "/tts" => {
                if body.len() > MAX_TEXT {
                    return Response::text(413, "ERR text too long\n");
                }
                let text = match String::from_utf8(body) {
                    Ok(text) if !text.trim().is_empty() => text,
                    Ok(_) => return Response::text(400, "ERR empty text\n"),
                    Err(_) => return Response::text(400, "ERR text is not UTF-8\n"),
                };
                let lang = self.last_lang.as_deref().unwrap_or(lang);
                match self.tts.speak(&text, voice, lang) {
                    Ok(audio) if audio.is_empty() => {
                        Response::text(500, "ERR tts produced no audio\n")
                    }
                    Ok(audio) => Response::audio(audio),
                    Err(error) => Response::text(500, format!("ERR tts: {error}\n")),
                }
            }
            _ => Response::text(404, "ERR unknown path\n"),
        }
    }
}

/// Token comparison that does not return early on the first wrong byte.
///
/// The threat is a curious neighbour with a script, not a nation state, but the cost
/// of doing it properly is one loop.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{StubLlm, StubStt, StubTts};

    fn bridge(token: Option<&str>) -> Bridge {
        Bridge {
            stt: Box::new(StubStt),
            llm: Box::new(StubLlm),
            tts: Box::new(StubTts),
            token: token.map(str::to_owned),
            last_lang: None,
            wake: Vec::new(),
            addressed_at: None,
            live: None,
            history: None,
            last_heard: None,
            tools: None,
        }
    }

    fn send(
        bridge: &mut Bridge,
        method: &str,
        path: &str,
        auth: Option<&str>,
        body: Vec<u8>,
    ) -> Response {
        bridge.dispatch(Incoming {
            method,
            path,
            auth,
            lang: "ru-RU",
            voice: "ru-RU-Standard-B",
            model: "stub",
            peer: "",
            trigger: "",
            body,
        })
    }

    #[test]
    fn health_and_unknown_paths() {
        let mut bridge = bridge(None);
        assert_eq!(
            send(&mut bridge, "GET", "/health", None, Vec::new()).status,
            200
        );
        assert_eq!(
            send(&mut bridge, "GET", "/stt", None, Vec::new()).status,
            405
        );
        assert_eq!(
            send(&mut bridge, "POST", "/nope", None, Vec::new()).status,
            404
        );
    }

    #[test]
    fn token_is_required_only_when_configured() {
        let mut open = bridge(None);
        assert_eq!(
            send(&mut open, "POST", "/stt", None, vec![0xFF; 16]).status,
            200
        );

        let mut guarded = bridge(Some("s3cret"));
        assert_eq!(
            send(&mut guarded, "POST", "/stt", None, vec![0xFF; 16]).status,
            401
        );
        assert_eq!(
            send(
                &mut guarded,
                "POST",
                "/stt",
                Some("Bearer wrong"),
                vec![0xFF; 16]
            )
            .status,
            401
        );
        assert_eq!(
            send(
                &mut guarded,
                "POST",
                "/stt",
                Some("Bearer s3cret"),
                vec![0xFF; 16]
            )
            .status,
            200
        );
    }

    #[test]
    fn each_endpoint_answers_with_the_agreed_content_type() {
        let mut bridge = bridge(None);

        let stt = send(&mut bridge, "POST", "/stt", None, vec![0xFF; 32_000]);
        assert_eq!(stt.status, 200);
        assert!(stt.content_type.starts_with("text/plain"));
        assert_eq!(stt.bytes_in, 32_000);
        let transcript = String::from_utf8(stt.body).expect("utf8");
        // Two seconds of µ-law: the stub reports what it received, so a truncated
        // upload shows up as the wrong duration rather than as silence.
        assert!(transcript.contains("2.00 с"), "{transcript}");
        assert!(transcript.contains("ru-RU"), "{transcript}");

        let llm = send(
            &mut bridge,
            "POST",
            "/llm",
            None,
            "три слова тут".as_bytes().to_vec(),
        );
        assert_eq!(llm.status, 200);
        assert!(String::from_utf8(llm.body).unwrap().contains("3 слов"));

        let tts = send(
            &mut bridge,
            "POST",
            "/tts",
            None,
            "Проверка".as_bytes().to_vec(),
        );
        assert_eq!(tts.status, 200);
        assert_eq!(tts.content_type, "audio/basic");
        assert!(tts.body.len() > 1_000, "{}", tts.body.len());
    }

    #[test]
    fn bad_bodies_are_refused_before_the_backends_see_them() {
        let mut bridge = bridge(None);
        assert_eq!(
            send(&mut bridge, "POST", "/stt", None, Vec::new()).status,
            400
        );
        assert_eq!(
            send(
                &mut bridge,
                "POST",
                "/stt",
                None,
                vec![0xFF; MAX_UTTERANCE + 1]
            )
            .status,
            413
        );
        assert_eq!(
            send(&mut bridge, "POST", "/llm", None, b"   ".to_vec()).status,
            400
        );
        assert_eq!(
            send(&mut bridge, "POST", "/llm", None, vec![0xFF, 0xFE]).status,
            400
        );
        assert_eq!(
            send(&mut bridge, "POST", "/tts", None, vec![b'x'; MAX_TEXT + 1]).status,
            413
        );
        assert_eq!(
            send(&mut bridge, "POST", "/tts", None, Vec::new()).status,
            400
        );
    }

    /// Recogniser that always answers with the same text, to exercise the gate
    /// between `/stt` and `/llm`.
    struct FixedStt(&'static str);

    impl Stt for FixedStt {
        fn transcribe(&mut self, _utterance: &[u8], _lang: &str) -> Result<String, String> {
            Ok(self.0.to_owned())
        }
    }

    #[test]
    fn a_transcript_without_letters_is_refused() {
        // Exactly what a 5 s capture of an empty room produced: Whisper invents a
        // full stop, and the model answered it with "How can I help you?".
        let mut bridge = bridge(None);
        bridge.stt = Box::new(FixedStt(" ."));
        let response = send(&mut bridge, "POST", "/stt", None, vec![0xFF; 16]);
        assert_eq!(response.status, 500);
        assert_eq!(
            String::from_utf8(response.body).unwrap(),
            "ERR stt: no speech\n"
        );
    }

    #[test]
    fn a_two_letter_answer_is_still_speech() {
        // «Да» is a whole turn; the gate exists to drop punctuation, not brevity.
        let mut bridge = bridge(None);
        bridge.stt = Box::new(FixedStt("Да."));
        let response = send(&mut bridge, "POST", "/stt", None, vec![0xFF; 16]);
        assert_eq!(response.status, 200);
        assert_eq!(String::from_utf8(response.body).unwrap(), "Да.");
    }

    #[test]
    fn a_question_a_plugin_takes_skips_the_model_and_keeps_the_panel() {
        let mut bridge = bridge(None);
        bridge.tools = Some(Arc::new(|question: &str, peer: &str| {
            (question.contains("погод") && peer == "10.0.0.5")
                .then(|| "Показываю: Погода.".to_owned())
        }));
        let ask = |bridge: &mut Bridge, text: &str| {
            bridge.dispatch(Incoming {
                method: "POST",
                path: "/llm",
                auth: None,
                lang: "ru-RU",
                voice: "",
                model: "stub",
                peer: "10.0.0.5",
                trigger: "",
                body: text.as_bytes().to_vec(),
            })
        };
        let tool = ask(&mut bridge, "покажи погоду");
        assert_eq!(String::from_utf8(tool.body).unwrap(), "Показываю: Погода.");
        assert!(
            tool.headers
                .iter()
                .any(|(name, value)| *name == "X-Panel" && value == "keep")
        );
        let model = ask(&mut bridge, "сколько будет два плюс два");
        assert!(
            model.headers.is_empty(),
            "the model's answer is painted as usual"
        );
    }

    #[test]
    fn a_button_or_console_turn_needs_no_wake_word() {
        let mut bridge = bridge(None);
        bridge.wake = vec!["рамк".to_owned()];
        bridge.stt = Box::new(FixedStt("Сколько будет два плюс два?"));
        for (trigger, status) in [("voice", 500), ("", 500), ("key", 200), ("console", 200)] {
            bridge.addressed_at = None;
            let response = bridge.dispatch(Incoming {
                method: "POST",
                path: "/stt",
                auth: None,
                lang: "ru-RU",
                voice: "",
                model: "stub",
                peer: "",
                trigger,
                body: vec![0xFF; 16],
            });
            assert_eq!(response.status, status, "trigger={trigger:?}");
        }
        // A button turn opens the same follow-up window as saying the name.
        assert!(bridge.is_addressed("А три плюс три?"));
    }

    #[test]
    fn only_an_addressed_transcript_reaches_the_model() {
        // The point of the stem: room conversation opens the microphone, and the
        // recogniser reads it correctly, but nobody was talking to the frame.
        let mut english = bridge(None);
        english.wake = vec!["frame".to_owned()];
        let mut bridge = bridge(None);
        bridge.wake = vec!["рамк".to_owned(), "frame".to_owned()];
        for overheard in ["Мама, дай мне сок.", "Сколько будет два плюс два?"]
        {
            bridge.stt = Box::new(FixedStt(overheard));
            let response = send(&mut bridge, "POST", "/stt", None, vec![0xFF; 16]);
            assert_eq!(response.status, 500, "{overheard} was answered unasked");
            assert_eq!(
                String::from_utf8(response.body).unwrap(),
                "ERR stt: not addressed\n"
            );
        }
        // Inflections and the English name are both the frame being spoken to.
        for asked in ["Рамка, сколько будет два плюс два?", "Эй, рамку послушай."]
        {
            bridge.stt = Box::new(FixedStt(asked));
            let response = send(&mut bridge, "POST", "/stt", None, vec![0xFF; 16]);
            assert_eq!(response.status, 200, "{asked} was not answered");
        }
        english.stt = Box::new(FixedStt("Frame, what is two plus two?"));
        let response = send(&mut english, "POST", "/stt", None, vec![0xFF; 16]);
        assert_eq!(response.status, 200);
    }

    #[test]
    fn a_bare_wake_word_opens_a_window_instead_of_being_answered() {
        // The turn that made the frame talk about itself: "Рамка" as its own
        // capture, answered as if it were a question.
        let mut bridge = bridge(None);
        bridge.wake = vec!["рамк".to_owned()];
        bridge.stt = Box::new(FixedStt("Рамка"));
        let response = send(&mut bridge, "POST", "/stt", None, vec![0xFF; 16]);
        assert_eq!(response.status, 500);
        assert_eq!(
            String::from_utf8(response.body).unwrap(),
            "ERR stt: wake only\n"
        );
        // The question that follows needs no name: the window is open.
        bridge.stt = Box::new(FixedStt("Из чего делают торт Наполеон?"));
        let response = send(&mut bridge, "POST", "/stt", None, vec![0xFF; 16]);
        assert_eq!(response.status, 200);
    }

    #[test]
    fn the_wake_word_is_not_part_of_the_question() {
        let mut bridge = bridge(None);
        bridge.wake = vec!["рамк".to_owned()];
        bridge.stt = Box::new(FixedStt("Рамка, из чего делают торт Наполеон?"));
        let response = send(&mut bridge, "POST", "/stt", None, vec![0xFF; 16]);
        assert_eq!(response.status, 200);
        assert_eq!(
            String::from_utf8(response.body).unwrap(),
            "из чего делают торт Наполеон?"
        );
        // The whole word goes, not the stem, so nothing is left of «фоторамка».
        assert_eq!(
            bridge.without_wake("Фоторамка расскажи сказку"),
            "расскажи сказку"
        );
        assert_eq!(
            bridge.without_wake("Сколько будет два плюс два?"),
            "Сколько будет два плюс два?"
        );
    }

    #[test]
    fn a_misheard_letter_still_calls_the_frame() {
        // Straight from the log: the name came back as «Ранка» and the frame, which
        // was waiting for «рамк», stayed silent.
        let mut bridge = bridge(None);
        bridge.wake = vec!["рамк".to_owned()];
        for misheard in ["Ранка.", "Ранька", "Ранку."] {
            bridge.stt = Box::new(FixedStt(misheard));
            let response = send(&mut bridge, "POST", "/stt", None, vec![0xFF; 16]);
            assert_eq!(response.status, 500, "{misheard} was answered");
            assert_eq!(
                String::from_utf8(response.body).unwrap(),
                "ERR stt: wake only\n",
                "{misheard}"
            );
        }
        // Words that are merely nearby must not wake it. The window the tests above
        // opened is closed first, or every word would count as "addressed".
        bridge.addressed_at = None;
        for other in ["Мамка.", "Рука.", "Рано ещё.", "Рамка"] {
            assert!(
                !bridge.is_addressed(other) || other == "Рамка",
                "{other} should not be the frame's name"
            );
        }
        // A misheard name in front of a question is answered, and only the name goes.
        assert!(bridge.is_addressed("Эй, ранка, сколько будет два плюс два?"));
        assert_eq!(
            bridge.without_wake("Рамке, сколько будет два плюс два?"),
            "сколько будет два плюс два?"
        );
    }

    #[test]
    fn without_a_wake_stem_every_transcript_is_addressed() {
        let bridge = bridge(None);
        assert!(bridge.is_addressed("Сколько будет два плюс два?"));
        assert!(bridge.is_addressed(""));
    }

    #[test]
    fn a_filler_transcript_is_refused() {
        // Exactly the turn that produced "I'm here to help." in English on a frame
        // whose question was Russian: noise came back as a filler phrase.
        for filler in ["Mm-hmm", "Thank you.", "Uh-huh.", "No.", "Oh"] {
            let mut bridge = bridge(None);
            bridge.stt = Box::new(FixedStt(filler));
            let response = send(&mut bridge, "POST", "/stt", None, vec![0xFF; 16]);
            assert_eq!(response.status, 500, "{filler} was treated as speech");
            assert_eq!(
                String::from_utf8(response.body).unwrap(),
                "ERR stt: no speech\n",
                "{filler}"
            );
        }
    }

    #[test]
    fn a_real_short_question_is_not_filler() {
        // The filler list must not swallow questions: these are short and one of them
        // is a single word, but each carries content the model can answer.
        for question in ["Кто мы?", "Да.", "Hyvää huomenta.", "Salut!"] {
            let mut bridge = bridge(None);
            bridge.stt = Box::new(FixedStt(question));
            let response = send(&mut bridge, "POST", "/stt", None, vec![0xFF; 16]);
            assert_eq!(response.status, 200, "{question} was dropped as filler");
        }
    }

    #[test]
    fn the_filler_forms_are_reduced_to_one_key() {
        assert_eq!(noise_key("Mm-hmm."), "mm hmm");
        assert_eq!(noise_key("Thank you!"), "thank you");
        assert_eq!(noise_key("  Uh—huh  "), "uhhuh");
        assert_eq!(noise_key("Кто мы?"), "кто мы");
        assert!(is_noise_transcript("OK."));
        assert!(!is_noise_transcript("Сколько будет два плюс два?"));
    }

    #[test]
    fn silence_is_reported_as_silence_not_as_speech() {
        let mut bridge = bridge(None);
        let response = send(&mut bridge, "POST", "/stt", None, vec![0xFF; 16_000]);
        let transcript = String::from_utf8(response.body).expect("utf8");
        assert!(transcript.contains("пик 0"), "{transcript}");
    }
}
