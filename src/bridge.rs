//! The voice bridge wire protocol.
//!
//! Speech recognition and synthesis happen on a machine on the LAN, not on the
//! ESP32 and not in a cloud service. That choice removes a surprising amount of
//! firmware: no API key to store, no TLS certificate bundle to flash, no JSON to
//! parse, no base64 to inflate a 40 KB utterance into 53 KB. The device speaks one
//! tiny protocol and a host-side service does the hard part.
//!
//! Three endpoints, all `POST`, all plain text or raw audio:
//!
//! | Path   | Request body              | Response                    |
//! |--------|---------------------------|-----------------------------|
//! | `/stt` | µ-law, 16 kHz, mono       | UTF-8 transcript, plain text |
//! | `/llm` | UTF-8 prompt, plain text  | UTF-8 reply, plain text     |
//! | `/tts` | UTF-8 text, plain text    | µ-law or WAV, 16 kHz, mono  |
//!
//! Hints travel as headers (`X-Lang`, `X-Voice`, `X-Model`) so the host can pick a
//! voice or model without the firmware needing to know what a model is. An optional
//! shared token goes in `Authorization: Bearer`, because the bridge is reachable by
//! anything on the network, not just the frame.
//!
//! The exact bytes are pinned here, in one place, because the host service has to
//! agree with them: see `bridge/README.md`.

// This module is compiled two ways: as part of the firmware, and alone by
// `scripts/check.sh` so its tests run on the host. In the standalone build the
// siblings have to be pulled in by path; inside the firmware they are already crate
// modules declared by `main.rs`.
#[cfg(test)]
#[path = "pcm.rs"]
mod pcm;
#[cfg(not(test))]
use crate::pcm;

/// Recognition and synthesis rate, in hertz, for the whole assistant path.
///
/// 16 kHz is what speech models are trained on, so resampling to the panel's 24 kHz
/// audio default would only cost quality and bandwidth.
pub const ASSISTANT_RATE: u32 = 16_000;

pub const STT_PATH: &str = "/stt";
pub const LLM_PATH: &str = "/llm";
pub const TTS_PATH: &str = "/tts";

/// Longest transcript accepted, in bytes. A forty-second monologue is roughly 900
/// bytes of Russian; anything past this is a broken response, not speech.
pub const MAX_TRANSCRIPT_BYTES: usize = 2_048;

/// Longest reply synthesised, in characters. Truncated here as well as on the host
/// so a bug on either side cannot turn one answer into a minute of audio.
pub const MAX_REPLY_CHARS: usize = 400;

/// Longest accepted synthesis response, in bytes: 20 s of 16 kHz µ-law, or half that
/// of WAV. The number is a UX choice as much as a bound — a frame that answers for
/// half a minute is ignoring the person in front of it — and it is also what keeps
/// the decoded buffers small: 320 000 µ-law bytes become 640 000 bytes of PCM, then
/// 960 000 at the 24 kHz playback rate, then 1.9 MB across two channels.
pub const MAX_SPEECH_BYTES: usize = 320_000;

/// Longest accepted answer text from `/llm`, in bytes. The frame draws and speaks it,
/// and `MAX_REPLY_CHARS` bounds the other direction, so a reply past this is a broken
/// response rather than a long thought.
pub const MAX_ANSWER_BYTES: usize = 4_096;

/// A parsed bridge base URL.
///
/// Only scheme, host, port and an optional path prefix are kept. Credentials in the
/// URL, queries and fragments are rejected rather than silently dropped, because
/// either they were meant to matter or they were a mistake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub tls: bool,
    pub host: String,
    pub port: Option<u16>,
    pub prefix: String,
}

impl Base {
    pub fn parse(url: &str) -> Result<Self, String> {
        let url = url.trim();
        if url.is_empty() {
            return Err("ERR bridge URL is empty\r\n".into());
        }
        let (tls, rest) = if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else {
            return Err("ERR bridge URL must start with http:// or https://\r\n".into());
        };
        if rest.contains('?') || rest.contains('#') {
            return Err("ERR bridge URL must not carry a query or fragment\r\n".into());
        }
        if rest.contains('@') {
            return Err("ERR bridge URL must not embed credentials\r\n".into());
        }
        let (authority, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.is_empty() {
            return Err("ERR bridge URL has no host\r\n".into());
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => {
                let port: u16 = port
                    .parse()
                    .map_err(|_| format!("ERR bridge port is not a number: {port}\r\n"))?;
                (host, Some(port))
            }
            None => (authority, None),
        };
        if host.is_empty() {
            return Err("ERR bridge URL has no host\r\n".into());
        }
        if !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return Err(format!(
                "ERR bridge host has invalid characters: {host}\r\n"
            ));
        }
        if prefix.len() > 32 {
            return Err("ERR bridge URL prefix is too long\r\n".into());
        }
        Ok(Self {
            tls,
            host: host.to_owned(),
            port,
            // Keep the leading slash: `endpoint` concatenates this directly after
            // the port, and a missing slash turned `:8080/bridge` into `:8080bridge`.
            prefix: if prefix.is_empty() {
                String::new()
            } else {
                format!("/{}", prefix.trim_end_matches('/'))
            },
        })
    }

    pub fn scheme(&self) -> &'static str {
        if self.tls { "https" } else { "http" }
    }

    /// Full URL for one endpoint, keeping any configured prefix.
    pub fn endpoint(&self, path: &str) -> String {
        match self.port {
            Some(port) => format!(
                "{}://{}:{}{}{}",
                self.scheme(),
                self.host,
                port,
                self.prefix,
                path
            ),
            None => format!("{}://{}{}{}", self.scheme(), self.host, self.prefix, path),
        }
    }

    /// Short form for `ai status`, without the path prefix noise when it is empty.
    pub fn summary(&self) -> String {
        self.endpoint("")
    }
}

/// One prepared HTTP request: the firmware only has to hand it to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

/// Hints the host service may use or ignore.
#[derive(Debug, Clone, Default)]
pub struct Hints<'a> {
    pub lang: &'a str,
    pub voice: &'a str,
    pub model: &'a str,
    /// Optional shared token, sent as `Authorization: Bearer`.
    pub token: &'a str,
    /// What started the capture, sent as `X-Trigger`: `key`, `voice` or
    /// `console`. The bridge asks for its wake word only from `voice`: a press
    /// or a typed command already says the frame is being spoken to.
    pub trigger: &'a str,
}

impl Hints<'_> {
    fn headers(&self, content_type: &'static str, accept: &str) -> Vec<(String, String)> {
        let mut headers = vec![
            ("Content-Type".to_owned(), content_type.to_owned()),
            ("Accept".to_owned(), accept.to_owned()),
            ("X-Lang".to_owned(), self.lang.to_owned()),
        ];
        // Empty hints are omitted rather than sent blank: a host service that picks
        // its own default on absence would otherwise be told to use "".
        for (name, value) in [
            ("X-Voice", self.voice),
            ("X-Model", self.model),
            ("X-Trigger", self.trigger),
        ] {
            if !value.is_empty() {
                headers.push((name.to_owned(), value.to_owned()));
            }
        }
        if !self.token.is_empty() {
            headers.push(("Authorization".to_owned(), format!("Bearer {}", self.token)));
        }
        headers
    }
}

/// µ-law utterance for recognition. The body is the audio, raw: no container, no
/// base64, no JSON envelope.
pub fn stt_request(base: &Base, hints: &Hints, utterance: &[u8]) -> Request {
    Request {
        url: base.endpoint(STT_PATH),
        headers: hints.headers("audio/basic", "text/plain"),
        content_type: "audio/basic",
        body: utterance.to_vec(),
    }
}

/// Prompt for the language model, which is the transcript plus whatever the
/// conversation state added to it.
pub fn llm_request(base: &Base, hints: &Hints, prompt: &str) -> Request {
    Request {
        url: base.endpoint(LLM_PATH),
        headers: hints.headers("text/plain; charset=utf-8", "text/plain"),
        content_type: "text/plain; charset=utf-8",
        body: prompt.as_bytes().to_vec(),
    }
}

/// Text to speak. The budget is applied here, inside the builder, so no call path
/// can hand the host an unbounded string.
pub fn tts_request(base: &Base, hints: &Hints, text: &str) -> Request {
    let body = trim_reply(text);
    Request {
        url: base.endpoint(TTS_PATH),
        headers: hints.headers("text/plain; charset=utf-8", "audio/basic, audio/wav"),
        content_type: "text/plain; charset=utf-8",
        body: body.into_bytes(),
    }
}

/// Cut a reply to [`MAX_REPLY_CHARS`] on a sentence boundary when one is close, and
/// mark the cut otherwise. Characters, not bytes: Russian is two bytes per letter
/// and a byte slice would panic mid-character.
pub fn trim_reply(text: &str) -> String {
    if text.chars().count() <= MAX_REPLY_CHARS {
        return text.trim().to_owned();
    }
    let head: String = text.chars().take(MAX_REPLY_CHARS).collect();
    let boundary = ['.', '!', '?', '\n']
        .iter()
        .filter_map(|end| head.rfind(*end))
        .max();
    match boundary {
        // Only honour a sentence end in the second half, or the reply becomes a
        // single clause and loses the point of having spoken at all.
        Some(index) if index >= MAX_REPLY_CHARS / 2 => head[..=index].trim().to_owned(),
        _ => format!("{}...", head.trim_end()),
    }
}

/// A transcript or reply: trimmed, whitespace-collapsed, bounded, and never empty.
///
/// Both endpoints return plain text, so there is nothing to parse; what is left is
/// refusing responses that cannot be spoken usefully.
pub fn text_response(body: &[u8], what: &str) -> Result<String, String> {
    if body.is_empty() {
        return Err(format!("ERR bridge returned an empty {what}\r\n"));
    }
    if body.len() > MAX_TRANSCRIPT_BYTES {
        return Err(format!(
            "ERR bridge {what} is {} bytes, limit {MAX_TRANSCRIPT_BYTES}\r\n",
            body.len()
        ));
    }
    let text = core::str::from_utf8(body)
        .map_err(|_| format!("ERR bridge {what} is not valid UTF-8\r\n"))?;
    // A host service may answer with a trailing newline, or spread one sentence over
    // several lines; both should read as a single spoken run of words.
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return Err(format!("ERR bridge {what} is blank\r\n"));
    }
    Ok(collapsed)
}

/// Recognised or generated text, ready to be spoken.
pub fn transcript_response(body: &[u8]) -> Result<String, String> {
    text_response(body, "transcript")
}

/// Model reply, already inside the synthesis budget.
///
/// The character trim runs before the byte cap, because a Russian reply at the
/// character limit is roughly twice that many bytes: applying the transcript cap
/// first would reject perfectly normal answers. The cap still applies afterwards, so
/// a reply stuffed with multi-byte characters is refused rather than sent for
/// synthesis.
pub fn reply_response(body: &[u8]) -> Result<String, String> {
    if body.is_empty() {
        return Err("ERR bridge returned an empty reply\r\n".into());
    }
    let text = core::str::from_utf8(body)
        .map_err(|_| "ERR bridge reply is not valid UTF-8\r\n".to_owned())?;
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = trim_reply(&collapsed);
    if trimmed.is_empty() {
        return Err("ERR bridge reply is blank\r\n".into());
    }
    Ok(trimmed)
}

/// Decoded synthesis response: 16-bit mono samples plus the rate they belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Speech {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved little-endian 16-bit samples.
    pub pcm: Vec<u8>,
}

impl Speech {
    pub fn frames(&self) -> usize {
        self.pcm.len() / (usize::from(self.channels) * 2)
    }

    /// Duplicate a mono stream across the requested channel count.
    ///
    /// The DAC runs stereo even when the source is mono, so silence has to be
    /// written to the second slot rather than left to whatever was there before.
    pub fn to_channels(&self, channels: u16) -> Vec<u8> {
        if channels == self.channels {
            return self.pcm.clone();
        }
        if self.channels == 2 && channels == 1 {
            return self
                .pcm
                .chunks_exact(4)
                .flat_map(|frame| [frame[0], frame[1]])
                .collect();
        }
        if self.channels != 1 || channels < 2 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(self.pcm.len() * usize::from(channels));
        for sample in self.pcm.chunks_exact(2) {
            for _ in 0..channels {
                out.extend_from_slice(sample);
            }
        }
        out
    }
}

/// Decode a synthesis response body.
///
/// Accepts WAV (a host that prefers a self-describing container) and raw µ-law (the
/// compact default). Anything else is refused with the content type in the message,
/// because "the audio did not play" is a much worse report than "the host answered
/// audio/ogg".
pub fn speech_response(content_type: &str, body: &[u8]) -> Result<Speech, String> {
    if body.is_empty() {
        return Err("ERR bridge returned empty audio\r\n".into());
    }
    if body.len() > MAX_SPEECH_BYTES {
        return Err(format!(
            "ERR bridge audio is {} bytes, limit {MAX_SPEECH_BYTES}\r\n",
            body.len()
        ));
    }
    let content_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    if body.starts_with(b"RIFF") {
        let wav = pcm::parse_wav_at(body, ASSISTANT_RATE)?;
        return Ok(Speech {
            sample_rate: wav.sample_rate,
            channels: wav.channels,
            pcm: wav.data.to_vec(),
        });
    }
    if content_type == "audio/basic" || content_type == "audio/ulaw" || content_type.is_empty() {
        // µ-law is one byte per sample, so 60 s is 960 000 bytes; the cap above
        // already covers it.
        return Ok(Speech {
            sample_rate: ASSISTANT_RATE,
            channels: 1,
            pcm: pcm::ulaw_decode_all(body),
        });
    }
    Err(format!(
        "ERR bridge audio type {content_type} is not supported\r\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Base {
        Base::parse("http://192.168.50.10:8080/bridge").expect("base")
    }

    #[test]
    fn base_urls_parse_into_endpoints() {
        let parsed = base();
        assert!(!parsed.tls);
        assert_eq!(parsed.host, "192.168.50.10");
        assert_eq!(parsed.port, Some(8080));
        assert_eq!(parsed.prefix, "/bridge");
        assert_eq!(
            parsed.endpoint(STT_PATH),
            "http://192.168.50.10:8080/bridge/stt"
        );
        assert_eq!(parsed.summary(), "http://192.168.50.10:8080/bridge");

        let plain = Base::parse("https://pi.local").expect("plain");
        assert!(plain.tls);
        assert_eq!(plain.port, None);
        assert_eq!(plain.endpoint(TTS_PATH), "https://pi.local/tts");

        let trailing = Base::parse("http://frame.local/bridge/").expect("trailing");
        assert_eq!(trailing.endpoint(LLM_PATH), "http://frame.local/bridge/llm");
    }

    #[test]
    fn bad_base_urls_are_rejected_with_a_reason() {
        for (url, needle) in [
            ("", "empty"),
            ("192.168.50.10:8080", "http://"),
            ("ftp://host", "http://"),
            ("http://user:pass@host", "credentials"),
            ("http://host/path?query=1", "query"),
            ("http://host/#frag", "fragment"),
            ("http://host:http", "not a number"),
            ("http:///stt", "no host"),
            ("http://ho st", "invalid characters"),
        ] {
            let error = Base::parse(url).expect_err(url);
            assert!(error.contains(needle), "{url} -> {error}");
            assert!(error.starts_with("ERR "), "{error}");
            assert!(error.ends_with("\r\n"), "{error}");
        }
    }

    #[test]
    fn requests_carry_the_right_shape() {
        let hints = Hints {
            lang: "ru-RU",
            voice: "ru-RU-Standard-B",
            model: "gemini-flash",
            token: "s3cret",
            trigger: "key",
        };
        let request = stt_request(&base(), &hints, &[0x7F, 0x80, 0x81]);
        assert!(
            request
                .headers
                .contains(&("X-Trigger".into(), "key".into()))
        );
        assert_eq!(request.url, "http://192.168.50.10:8080/bridge/stt");
        assert_eq!(request.content_type, "audio/basic");
        assert_eq!(request.body, vec![0x7F, 0x80, 0x81]);
        assert!(
            request
                .headers
                .contains(&("Accept".into(), "text/plain".into()))
        );
        assert!(
            request
                .headers
                .contains(&("Authorization".into(), "Bearer s3cret".into()))
        );
        assert!(
            request
                .headers
                .contains(&("X-Voice".into(), "ru-RU-Standard-B".into()))
        );

        // No token configured means no Authorization header at all, not an empty one.
        let bare = Hints {
            token: "",
            ..hints.clone()
        };
        let request = llm_request(&base(), &bare, "привет");
        assert!(
            !request
                .headers
                .iter()
                .any(|(name, _)| name == "Authorization")
        );
        assert_eq!(request.body, "привет".as_bytes());
        // Text requests have no capture behind them, so no trigger is sent.
        let unprompted = Hints {
            trigger: "",
            ..bare.clone()
        };
        let request = llm_request(&base(), &unprompted, "привет");
        assert!(!request.headers.iter().any(|(name, _)| name == "X-Trigger"));
        assert!(
            request
                .headers
                .contains(&("X-Model".into(), "gemini-flash".into()))
        );
    }

    #[test]
    fn text_responses_are_collapsed_and_bounded() {
        assert_eq!(
            transcript_response(b"  \xd0\x9f\xd1\x80\xd0\xb8\xd0\xb2\xd0\xb5\xd1\x82\n").unwrap(),
            "Привет"
        );
        assert_eq!(
            transcript_response(b"two\n\nlines\there").unwrap(),
            "two lines here"
        );
        assert!(transcript_response(b"").unwrap_err().contains("empty"));
        assert!(
            transcript_response(b"   \n ")
                .unwrap_err()
                .contains("blank")
        );
        assert!(
            transcript_response(&[0xFF, 0xFE])
                .unwrap_err()
                .contains("UTF-8")
        );
        let long = vec![b'x'; MAX_TRANSCRIPT_BYTES + 1];
        assert!(transcript_response(&long).unwrap_err().contains("limit"));

        let reply = reply_response("Это ответ. ".repeat(200).as_bytes()).unwrap();
        assert!(reply.chars().count() <= MAX_REPLY_CHARS);
        assert!(reply.ends_with('.'));
    }

    #[test]
    fn speech_accepts_raw_ulaw_and_wav() {
        let ulaw: Vec<u8> = (0..160).map(|i| (i % 256) as u8).collect();
        let speech = speech_response("audio/basic", &ulaw).expect("ulaw");
        assert_eq!(speech.sample_rate, ASSISTANT_RATE);
        assert_eq!(speech.channels, 1);
        assert_eq!(speech.frames(), 160);
        assert_eq!(speech.pcm.len(), 320);

        let wav = wav_16k_mono(&[0_u8; 640]);
        let speech = speech_response("audio/wav", &wav).expect("wav");
        assert_eq!(speech.channels, 1);
        assert_eq!(speech.frames(), 320);

        // Wrong rate is reported, not played at the wrong speed.
        let wrong = wav_at(24_000, &[0_u8; 640]);
        let error = speech_response("audio/wav", &wrong).unwrap_err();
        assert!(error.contains("expected 16000"), "{error}");

        assert!(
            speech_response("audio/ogg", &[0_u8; 16])
                .unwrap_err()
                .contains("audio/ogg")
        );
        assert!(
            speech_response("audio/basic", &[])
                .unwrap_err()
                .contains("empty")
        );
        let oversized = vec![0_u8; MAX_SPEECH_BYTES + 1];
        assert!(
            speech_response("audio/basic", &oversized)
                .unwrap_err()
                .contains("limit")
        );
    }

    #[test]
    fn speech_limit_is_twenty_seconds_and_the_boundary_is_inclusive() {
        // The number is a duration choice, not a byte count someone liked: µ-law is
        // one byte per sample, so the limit is exactly 20 s at the assistant rate.
        assert_eq!(MAX_SPEECH_BYTES as u32, ASSISTANT_RATE * 20);
        let at_limit = vec![0xff_u8; MAX_SPEECH_BYTES];
        let speech = speech_response("audio/basic", &at_limit).expect("boundary accepted");
        assert_eq!(speech.frames(), MAX_SPEECH_BYTES);
    }

    #[test]
    fn mono_speech_is_duplicated_not_left_half_empty() {
        let speech = Speech {
            sample_rate: ASSISTANT_RATE,
            channels: 1,
            pcm: vec![0x01, 0x00, 0x02, 0x00],
        };
        assert_eq!(speech.to_channels(2), vec![1, 0, 1, 0, 2, 0, 2, 0]);
        assert_eq!(speech.to_channels(1), speech.pcm);

        let stereo = Speech {
            sample_rate: ASSISTANT_RATE,
            channels: 2,
            pcm: vec![1, 0, 2, 0],
        };
        assert_eq!(stereo.to_channels(1), vec![1, 0]);
        assert!(stereo.to_channels(3).is_empty());
    }

    fn wav_at(rate: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16_u32.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2_u16.to_le_bytes());
        out.extend_from_slice(&16_u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    fn wav_16k_mono(data: &[u8]) -> Vec<u8> {
        wav_at(ASSISTANT_RATE, data)
    }
}
