//! Bounded command framing. Oversized or invalid lines are discarded in full.
//!
//! Bytes are framed, not characters: control bytes and `DEL` invalidate the line,
//! everything printable passes through untouched. That lets UTF-8 reach `ai ask`
//! from a terminal, where Russian is two bytes per letter and would otherwise be
//! rejected a byte at a time.
/// Longest line accepted. The worst realistic command is
/// `wifi join <32-byte SSID> <63-byte password>`, which is 106 bytes.
const LINE_BYTES: usize = 160;
/// `ai stt` with no argument records this long; three seconds is about one
/// sentence, and the double-beep cue is played before the timer starts.
const DEFAULT_UTTERANCE_MS: u32 = 3_000;

pub struct LineBuffer {
    bytes: [u8; LINE_BYTES],
    len: usize,
    invalid: bool,
}

impl Default for LineBuffer {
    fn default() -> Self {
        Self {
            bytes: [0; LINE_BYTES],
            len: 0,
            invalid: false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Info,
    Buttons,
    LedOn,
    LedOff,
    Sensor,
    Rtc,
    Pmu,
    Sd,
    SdList(String),
    DisplayBars,
    DisplayBmp(String),
    DisplayRaw(String),
    DisplayClear(u8),
    DisplaySleep,
    DisplayStatus,
    AudioStatus,
    AudioRegs,
    AudioLevel(u32),
    AudioRecord(u32),
    AudioTone(u32),
    AudioLoop(u32),
    AudioHex,
    /// Decibels times ten, so the command type stays integer and `Eq`.
    AudioVolume(i16),
    AudioGain(i16),
    AudioPlay(String),
    AudioOff,
    WifiScan,
    /// SSID and password, never echoed back to the host. `None` on both means
    /// "join with the credentials loaded from the SD card".
    WifiJoin {
        ssid: Option<String>,
        password: Option<String>,
    },
    WifiStatus,
    WifiTime(Option<String>),
    WifiOff,
    /// Deep sleep now, with an optional timed wake in seconds; `None` uses
    /// `power.sleep_secs` (0 = buttons only).
    Sleep(Option<u32>),
    /// Boot-time link flag; read once before the command loop, so it only
    /// changes behaviour after the next reset.
    WifiAuto(bool),
    /// Show effective settings, the file path and whether it loaded.
    Config,
    /// Write the settings file, passphrase included.
    ConfigSave,
    /// Show the effective assistant configuration, key redacted.
    AiStatus,
    /// Build every request body from the settings without sending anything.
    AiSelftest,
    /// Capture a turn, send it to the bridge and print the transcript.
    AiStt(u32),
    /// Wait for the KEY button, capture, transcribe, draw the transcript.
    AiListen(u32),
    /// The whole round trip: capture, transcribe, ask, speak, draw both texts.
    AiTalk(u32),
    /// Text in, speech out: the bridge's `/llm` and `/tts`, no microphone.
    AiAsk(String),
    /// Set the bridge URL live, or clear it with `-`.
    AiUrl(String),
    /// Set the bridge token live, or clear it with `-`.
    AiKey(String),
    /// What may start a turn with nobody typing: nothing, the KEY button, or
    /// the microphone.
    AiIdle(IdleMode),
    /// Loudness floor for the voice watcher, in 16-bit sample units.
    AiThreshold(u32),
    /// Start the slideshow, optionally naming a directory and an interval.
    GalleryStart {
        dir: Option<String>,
        seconds: Option<u32>,
    },
    GalleryStop,
    /// Advance once, now, whether or not the interval has elapsed.
    GalleryNext,
    GalleryStatus,
    /// Report whether the upload page is listening.
    WebStatus,
    /// Which app slot runs, what an upload would replace, and how big it may be.
    Ota,
    /// Start the upload server now instead of waiting for a radio event.
    WebStart,
    WebStop,
    /// Set or clear the page password. `None` clears it; the listener picks the
    /// change up when it is restarted.
    WebPassword(Option<String>),
    Unknown,
}

// Protocol color indices shared with `panel`; kept local so this module stays
// compilable as a standalone native test.
const BLACK: u8 = 0;
const WHITE: u8 = 1;
const YELLOW: u8 = 2;
const RED: u8 = 3;
const BLUE: u8 = 5;
const GREEN: u8 = 6;

fn text(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

fn number(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// Parse a decibel value with at most one fractional digit into tenths, so
/// `audio volume 0`, `-6`, `6.5` and `+3` all work without floating point.
fn decibels(bytes: &[u8]) -> Option<i16> {
    let (negative, rest) = match bytes.first()? {
        b'-' => (true, &bytes[1..]),
        b'+' => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let (whole, fraction) = match rest.iter().position(|&byte| byte == b'.') {
        Some(index) => (&rest[..index], Some(&rest[index + 1..])),
        None => (rest, None),
    };
    let whole = number(whole)?;
    let fraction = match fraction {
        None => 0,
        Some(digits) if digits.len() == 1 => u32::from(digits[0].checked_sub(b'0')?),
        Some(_) => return None,
    };
    if fraction > 9 || whole > 100 {
        return None;
    }
    let tenths = i16::try_from(whole * 10 + fraction).ok()?;
    Some(if negative { -tenths } else { tenths })
}

/// Reject credentials the driver cannot use, so a typo is a clear error rather
/// than a silent 15-second connect timeout. WPA passphrases are 8..=63 bytes;
/// an empty password means an open network.
pub fn credential_error(ssid: &str, password: &str) -> Option<&'static str> {
    if ssid.is_empty() || ssid.len() > 32 {
        Some("ERR wifi ssid must be 1..=32 bytes")
    } else if password.len() > 63 || (1..8).contains(&password.len()) {
        Some("ERR wifi password must be empty or 8..=63 bytes")
    } else {
        None
    }
}

/// Split `ssid password` at the first space. An empty password is allowed for
/// open networks, and an SSID may not contain spaces (the protocol has no
/// quoting), which is stated in `help`.
fn credentials(line: &[u8]) -> Option<(String, String)> {
    let Some(split) = line.iter().position(|&byte| byte == b' ') else {
        // `wifi join <ssid>` with no password selects an open network.
        return text(line).map(|ssid| (ssid, String::new()));
    };
    let ssid = text(&line[..split])?;
    // An empty password is meaningful: it selects an open network.
    let password = if split + 1 == line.len() {
        String::new()
    } else {
        text(&line[split + 1..])?
    };
    if ssid.is_empty() || ssid.contains(' ') {
        return None;
    }
    Some((ssid, password))
}

/// What the idle watcher waits for.
///
/// Defined by the settings module because the same three names are written to
/// `EASELTEST/CONFIG.TXT`, and one enum means the console and the file can never
/// disagree about what is a valid mode.
pub use crate::config::IdleMode;

/// `gallery start [<dir> [<seconds>]]`, where the interval is optional and the
/// directory may contain spaces only if the interval is absent, since that is
/// the one place the two can be told apart.
fn gallery_start(rest: &[u8]) -> Option<Command> {
    let rest = text(rest)?;
    let (dir, seconds) = match rest.rsplit_once(' ') {
        // `gallery start <dir> 120`: the interval is the last word when it is all
        // digits, which leaves the directory free to contain spaces.
        Some((left, right)) if right.bytes().all(|byte| byte.is_ascii_digit()) => {
            let left = left.trim();
            let dir = (!left.is_empty()).then(|| left.to_owned());
            (dir, right.parse().ok())
        }
        // `gallery start 120`: an interval and no directory.
        _ if rest.bytes().all(|byte| byte.is_ascii_digit()) => (None, rest.parse().ok()),
        _ => (Some(rest.to_owned()), None),
    };
    if let Some(dir) = &dir
        && (dir.starts_with('/') || dir.contains('\\') || dir.contains('\0'))
    {
        return None;
    }
    Some(Command::GalleryStart { dir, seconds })
}

fn clear_color(name: &[u8]) -> Option<u8> {
    match name {
        b"black" => Some(BLACK),
        b"white" => Some(WHITE),
        b"yellow" => Some(YELLOW),
        b"red" => Some(RED),
        b"blue" => Some(BLUE),
        b"green" => Some(GREEN),
        _ => None,
    }
}

impl LineBuffer {
    pub fn push(&mut self, byte: u8) -> Option<Command> {
        if matches!(byte, b'\r' | b'\n') {
            let command = if self.invalid {
                Some(Command::Unknown)
            } else {
                match &self.bytes[..self.len] {
                    b"" => None,
                    b"help" => Some(Command::Help),
                    b"info" => Some(Command::Info),
                    b"buttons" => Some(Command::Buttons),
                    b"led on" => Some(Command::LedOn),
                    b"led off" => Some(Command::LedOff),
                    b"sensor" => Some(Command::Sensor),
                    b"rtc" => Some(Command::Rtc),
                    b"pmu" => Some(Command::Pmu),
                    b"sleep" => Some(Command::Sleep(None)),
                    b"sd" => Some(Command::Sd),
                    b"display bars" => Some(Command::DisplayBars),
                    b"display sleep" => Some(Command::DisplaySleep),
                    b"display status" => Some(Command::DisplayStatus),
                    b"audio status" => Some(Command::AudioStatus),
                    b"audio regs" => Some(Command::AudioRegs),
                    b"audio off" => Some(Command::AudioOff),
                    b"audio tone" => Some(Command::AudioTone(500)),
                    b"audio loop" => Some(Command::AudioLoop(500)),
                    b"audio hex" => Some(Command::AudioHex),
                    b"audio volume" => Some(Command::AudioVolume(0)),
                    b"audio gain" => Some(Command::AudioGain(300)),
                    b"wifi scan" => Some(Command::WifiScan),
                    b"wifi status" => Some(Command::WifiStatus),
                    b"wifi time" => Some(Command::WifiTime(None)),
                    b"wifi off" => Some(Command::WifiOff),
                    b"wifi auto on" => Some(Command::WifiAuto(true)),
                    b"wifi auto off" => Some(Command::WifiAuto(false)),
                    b"wifi join" => Some(Command::WifiJoin {
                        ssid: None,
                        password: None,
                    }),
                    b"config" => Some(Command::Config),
                    b"config save" => Some(Command::ConfigSave),
                    b"ai" | b"ai status" => Some(Command::AiStatus),
                    b"ai selftest" => Some(Command::AiSelftest),
                    b"ai stt" => Some(Command::AiStt(DEFAULT_UTTERANCE_MS)),
                    b"ai listen" => Some(Command::AiListen(DEFAULT_UTTERANCE_MS)),
                    b"ai talk" => Some(Command::AiTalk(DEFAULT_UTTERANCE_MS)),
                    // A bare `ai url`/`ai key` is a mistake worth reporting rather
                    // than a request for the current value: `ai status` does that.
                    b"ai url" | b"ai key" | b"ai ask" | b"ai idle" => Some(Command::Unknown),
                    // A bare `sd ls` lists the card root; naming a directory is
                    // optional, and the root is what a gallery needs first.
                    b"sd ls" => Some(Command::SdList(String::new())),
                    b"gallery" | b"gallery status" => Some(Command::GalleryStatus),
                    b"gallery stop" => Some(Command::GalleryStop),
                    b"gallery next" => Some(Command::GalleryNext),
                    b"ota" | b"ota status" => Some(Command::Ota),
                    b"web" | b"web status" => Some(Command::WebStatus),
                    b"web start" => Some(Command::WebStart),
                    b"web stop" => Some(Command::WebStop),
                    // The page's own form can set a password but never clear one
                    // (an empty field means "keep"), so the console owns the way
                    // back out of a forgotten password.
                    b"web password" | b"web password off" => Some(Command::WebPassword(None)),
                    b"gallery start" => Some(Command::GalleryStart {
                        dir: None,
                        seconds: None,
                    }),
                    line => line
                        .strip_prefix(b"ai url ")
                        .and_then(text)
                        .map(Command::AiUrl)
                        .or_else(|| {
                            line.strip_prefix(b"ai key ")
                                .and_then(text)
                                .map(Command::AiKey)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"ai stt ")
                                .and_then(number)
                                .map(Command::AiStt)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"ai listen ")
                                .and_then(number)
                                .map(Command::AiListen)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"ai talk ")
                                .and_then(number)
                                .map(Command::AiTalk)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"ai ask ")
                                .and_then(text)
                                .map(Command::AiAsk)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"ai idle ")
                                .and_then(|name| std::str::from_utf8(name).ok())
                                .and_then(IdleMode::parse)
                                .map(Command::AiIdle)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"sleep ")
                                .and_then(number)
                                .map(|secs| Command::Sleep(Some(secs)))
                        })
                        .or_else(|| {
                            line.strip_prefix(b"ai threshold ")
                                .and_then(number)
                                .map(Command::AiThreshold)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"web password ")
                                .and_then(text)
                                .map(|value| Command::WebPassword(Some(value)))
                        })
                        .or_else(|| line.strip_prefix(b"gallery start ").and_then(gallery_start))
                        .or_else(|| {
                            line.strip_prefix(b"display clear ")
                                .and_then(clear_color)
                                .map(Command::DisplayClear)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"sd ls ")
                                .and_then(text)
                                .map(Command::SdList)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"display bmp ")
                                .and_then(text)
                                .map(Command::DisplayBmp)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"display raw ")
                                .and_then(text)
                                .map(Command::DisplayRaw)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"audio level ")
                                .and_then(number)
                                .map(Command::AudioLevel)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"audio rec ")
                                .and_then(number)
                                .map(Command::AudioRecord)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"audio tone ")
                                .and_then(number)
                                .map(Command::AudioTone)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"audio loop ")
                                .and_then(number)
                                .map(Command::AudioLoop)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"audio volume ")
                                .and_then(decibels)
                                .map(Command::AudioVolume)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"audio gain ")
                                .and_then(decibels)
                                .map(Command::AudioGain)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"audio play ")
                                .and_then(text)
                                .map(Command::AudioPlay)
                        })
                        .or_else(|| {
                            line.strip_prefix(b"wifi join ").and_then(credentials).map(
                                |(ssid, password)| Command::WifiJoin {
                                    ssid: Some(ssid),
                                    password: Some(password),
                                },
                            )
                        })
                        .or_else(|| {
                            line.strip_prefix(b"wifi time ")
                                .and_then(text)
                                .map(|server| Command::WifiTime(Some(server)))
                        })
                        .or(Some(Command::Unknown)),
                }
            };
            self.len = 0;
            self.invalid = false;
            return command;
        }
        // Printable bytes only: refuse C0 controls and DEL, keep everything else.
        if byte < b' ' || byte == 0x7f || self.len == self.bytes.len() {
            self.invalid = true;
        } else if !self.invalid {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(input: &[u8]) -> Vec<Command> {
        let mut buffer = LineBuffer::default();
        input.iter().filter_map(|&byte| buffer.push(byte)).collect()
    }

    #[test]
    fn commands_and_crlf() {
        assert_eq!(
            feed("help\r\ninfo\nbuttons\nled on\nled off\nsensor\nrtc\npmu\nsd\nsd ls 01_SYS~1\ndisplay bars\ndisplay clear red\ndisplay bmp 01_SYS~1/00_init.bmp\ndisplay raw test.el6\ndisplay sleep\ndisplay status\naudio status\naudio regs\naudio level 500\naudio rec 2\naudio tone\naudio tone 250\naudio loop\naudio loop 1000\naudio hex\naudio volume\naudio volume -6\naudio volume +3.5\naudio gain 30\naudio gain 0\naudio play EASELTEST/REC.WAV\naudio off\nwifi scan\nwifi status\nwifi time\nwifi time 192.168.1.1\nwifi off\nwifi auto on\nwifi auto off\nwifi join home secret123\nwifi join\nconfig\nconfig save\nai\nai selftest\nai url http://192.168.50.93:8095\nai key easellet-lan-2026\nai stt 3000\nai listen\nai listen 4000\nai talk\nai talk 6000\nai ask сколько зайцев пляшет вокруг елки\nai idle off\nai idle key\nai idle voice\nai threshold 20\nai threshold 600\nai threshold 4000\n".as_bytes()),
            [
                Command::Help,
                Command::Info,
                Command::Buttons,
                Command::LedOn,
                Command::LedOff,
                Command::Sensor,
                Command::Rtc,
                Command::Pmu,
                Command::Sd,
                Command::SdList("01_SYS~1".to_owned()),
                Command::DisplayBars,
                Command::DisplayClear(RED),
                Command::DisplayBmp("01_SYS~1/00_init.bmp".to_owned()),
                Command::DisplayRaw("test.el6".to_owned()),
                Command::DisplaySleep,
                Command::DisplayStatus,
                Command::AudioStatus,
                Command::AudioRegs,
                Command::AudioLevel(500),
                Command::AudioRecord(2),
                Command::AudioTone(500),
                Command::AudioTone(250),
                Command::AudioLoop(500),
                Command::AudioLoop(1000),
                Command::AudioHex,
                Command::AudioVolume(0),
                Command::AudioVolume(-60),
                Command::AudioVolume(35),
                Command::AudioGain(300),
                Command::AudioGain(0),
                Command::AudioPlay("EASELTEST/REC.WAV".to_owned()),
                Command::AudioOff,
                Command::WifiScan,
                Command::WifiStatus,
                Command::WifiTime(None),
                Command::WifiTime(Some("192.168.1.1".to_owned())),
                Command::WifiOff,
                Command::WifiAuto(true),
                Command::WifiAuto(false),
                Command::WifiJoin {
                    ssid: Some("home".to_owned()),
                    password: Some("secret123".to_owned())
                },
                Command::WifiJoin {
                    ssid: None,
                    password: None
                },
                Command::Config,
                Command::ConfigSave,
                Command::AiStatus,
                Command::AiSelftest,
                Command::AiUrl("http://192.168.50.93:8095".to_owned()),
                Command::AiKey("easellet-lan-2026".to_owned()),
                Command::AiStt(3_000),
                Command::AiListen(3_000),
                Command::AiListen(4_000),
                Command::AiTalk(3_000),
                Command::AiTalk(6_000),
                Command::AiAsk("сколько зайцев пляшет вокруг елки".to_owned()),
                Command::AiIdle(IdleMode::Off),
                Command::AiIdle(IdleMode::Key),
                Command::AiIdle(IdleMode::Voice),
                Command::AiThreshold(20),
                Command::AiThreshold(600),
                Command::AiThreshold(4000),
            ]
        );
    }

    #[test]
    fn gallery_commands_parse_with_and_without_arguments() {
        assert_eq!(feed(b"gallery\n"), [Command::GalleryStatus]);
        assert_eq!(feed(b"gallery status\n"), [Command::GalleryStatus]);
        assert_eq!(feed(b"gallery stop\n"), [Command::GalleryStop]);
        assert_eq!(feed(b"gallery next\n"), [Command::GalleryNext]);
        assert_eq!(feed(b"web\n"), [Command::WebStatus]);
        assert_eq!(feed(b"ota\n"), [Command::Ota]);
        assert_eq!(feed(b"ota status\n"), [Command::Ota]);
        assert_eq!(feed(b"web stop\n"), [Command::WebStop]);
        assert_eq!(
            feed(b"gallery start\n"),
            [Command::GalleryStart {
                dir: None,
                seconds: None
            }]
        );
        assert_eq!(
            feed(b"gallery start EASELTEST\n"),
            [Command::GalleryStart {
                dir: Some("EASELTEST".to_owned()),
                seconds: None
            }]
        );
        // A bare number is the interval, so the usual directory can be resumed
        // by hand without naming it.
        assert_eq!(
            feed(b"gallery start 120\n"),
            [Command::GalleryStart {
                dir: None,
                seconds: Some(120)
            }]
        );
        assert_eq!(
            feed(b"gallery start EASELTEST/GALLERY 600\n"),
            [Command::GalleryStart {
                dir: Some("EASELTEST/GALLERY".to_owned()),
                seconds: Some(600)
            }]
        );
        // Absolute paths and backslashes are refused by the same rule the image
        // loader applies, so neither can escape the card root.
        assert_eq!(feed(b"gallery start /etc 60\n"), [Command::Unknown]);
        assert_eq!(
            feed(b"gallery start ..\\EASELTEST 60\n"),
            [Command::Unknown]
        );
    }

    #[test]
    fn the_page_password_is_set_by_one_line_and_cleared_by_off() {
        // A bare `web password` and `off` both clear the lock: the page form
        // can set a password but never unset it, so this is the only way back.
        assert_eq!(feed(b"web password\n"), [Command::WebPassword(None)]);
        assert_eq!(feed(b"web password off\n"), [Command::WebPassword(None)]);
        assert_eq!(
            feed(b"web password hunter2hunter2\n"),
            [Command::WebPassword(Some("hunter2hunter2".to_owned()))]
        );
    }

    #[test]
    fn the_gallery_bounds_agree_between_the_file_and_the_scheduler() {
        // The settings parser and `gallery.rs` both clamp, and both are compiled
        // without the other in their own native test, so this is the one place
        // that can compare them.
        use crate::config;

        assert_eq!(config::MIN_GALLERY_SECS, crate::gallery::MIN_INTERVAL_SECS);
        assert_eq!(config::MAX_GALLERY_SECS, crate::gallery::MAX_INTERVAL_SECS);
        assert_eq!(
            config::Settings::default().gallery_interval().as_secs(),
            u64::from(crate::gallery::DEFAULT_INTERVAL_SECS)
        );
    }

    #[test]
    fn a_bare_sd_ls_lists_the_card_root() {
        // The empty path is the root: `storage::list` maps `None` onto it.
        assert_eq!(feed(b"sd ls\n"), [Command::SdList(String::new())]);
        assert_eq!(
            feed(b"sd ls EASELTEST\n"),
            [Command::SdList("EASELTEST".to_owned())]
        );
    }

    #[test]
    fn ai_ask_takes_the_rest_of_the_line_verbatim() {
        // No quoting exists, and none is needed: everything after the prefix is
        // the prompt, spaces included.
        assert_eq!(feed(b"ai ask\n"), [Command::Unknown]);
        assert_eq!(
            feed("ai ask привет, как дела?\n".as_bytes()),
            [Command::AiAsk("привет, как дела?".to_owned())]
        );
        // Russian is two UTF-8 bytes per letter, so the line budget has to hold
        // a whole question, not just an ASCII one.
        let long = "ж".repeat(60);
        assert_eq!(
            feed(format!("ai ask {long}\n").as_bytes()),
            [Command::AiAsk(long)]
        );
    }

    #[test]
    fn ai_value_commands_need_a_value() {
        // A bare `ai url` reads like a status request, but silently doing nothing
        // is worse than saying the command is unknown: `ai status` is the status.
        assert_eq!(feed(b"ai url\n"), [Command::Unknown]);
        assert_eq!(feed(b"ai key\n"), [Command::Unknown]);
        // `-` clears, so it has to parse as a value rather than as a flag.
        assert_eq!(feed(b"ai url -\n"), [Command::AiUrl("-".to_owned())]);
        assert_eq!(feed(b"ai key -\n"), [Command::AiKey("-".to_owned())]);
        // A URL carrying a path prefix survives the single-space split.
        assert_eq!(
            feed(b"ai url http://192.168.50.93:8095/v1\n"),
            [Command::AiUrl("http://192.168.50.93:8095/v1".to_owned())]
        );
    }

    #[test]
    fn the_idle_watcher_takes_three_modes_and_nothing_else() {
        assert_eq!(feed(b"ai idle\n"), [Command::Unknown]);
        assert_eq!(feed(b"ai idle off\n"), [Command::AiIdle(IdleMode::Off)]);
        assert_eq!(feed(b"ai idle key\n"), [Command::AiIdle(IdleMode::Key)]);
        assert_eq!(feed(b"ai idle voice\n"), [Command::AiIdle(IdleMode::Voice)]);
        assert_eq!(feed(b"ai threshold 250\n"), [Command::AiThreshold(250)]);
        assert_eq!(feed(b"sleep\n"), [Command::Sleep(None)]);
        assert_eq!(feed(b"sleep 600\n"), [Command::Sleep(Some(600))]);
        assert_eq!(feed(b"sleep soon\n"), [Command::Unknown]);
        // A typo must not fall back to a mode: silence would look like a bug in
        // the watcher rather than in the typing.
        assert_eq!(feed(b"ai idle kye\n"), [Command::Unknown]);
        assert_eq!(feed(b"ai idle voice 2\n"), [Command::Unknown]);
        // Bounds are enforced by the handler, the grammar only needs a number.
        assert_eq!(feed(b"ai threshold\n"), [Command::Unknown]);
        assert_eq!(feed(b"ai threshold 0.5\n"), [Command::Unknown]);
    }

    #[test]
    fn decibel_parser_accepts_signs_and_one_decimal() {
        for (line, expected) in [
            (&b"audio volume 6"[..], Command::AudioVolume(60)),
            (b"audio volume -6", Command::AudioVolume(-60)),
            (b"audio volume +3.5", Command::AudioVolume(35)),
            (b"audio volume -0.5", Command::AudioVolume(-5)),
            (b"audio gain 37.5", Command::AudioGain(375)),
        ] {
            let mut bytes = line.to_vec();
            bytes.push(b'\n');
            assert_eq!(
                feed(&bytes),
                [expected],
                "line {}",
                String::from_utf8_lossy(line)
            );
        }
        for line in [
            &b"audio volume "[..],
            b"audio volume 6.55",
            b"audio volume .5",
            b"audio volume 6.",
            b"audio volume --6",
            b"audio volume 12x",
        ] {
            let mut bytes = line.to_vec();
            bytes.push(b'\n');
            assert_eq!(
                feed(&bytes),
                [Command::Unknown],
                "line {}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn reject_whole_overlong_line_then_recover() {
        let mut bytes = vec![b'x'; LINE_BYTES + 1];
        bytes.extend_from_slice(b"led on\ninfo\n");
        assert_eq!(feed(&bytes), [Command::Unknown, Command::Info]);
    }

    #[test]
    fn credentials_need_an_ssid_and_may_have_an_empty_password() {
        assert_eq!(
            feed(b"wifi join home secret123\n"),
            [Command::WifiJoin {
                ssid: Some("home".to_owned()),
                password: Some("secret123".to_owned())
            }]
        );
        assert_eq!(
            feed(b"wifi join cafe \n"),
            [Command::WifiJoin {
                ssid: Some("cafe".to_owned()),
                password: Some(String::new())
            }],
            "open network"
        );
        assert_eq!(
            feed(b"wifi join cafe\n"),
            [Command::WifiJoin {
                ssid: Some("cafe".to_owned()),
                password: Some(String::new())
            }],
            "open network without a trailing space"
        );
        assert_eq!(
            feed(b"wifi join\n"),
            [Command::WifiJoin {
                ssid: None,
                password: None
            }],
            "no arguments selects the saved credentials"
        );
        assert_eq!(
            feed(b"wifi join  secret\n"),
            [Command::Unknown],
            "empty ssid"
        );
        // Only the SSID is space-delimited; a passphrase may contain spaces.
        assert_eq!(
            feed(b"wifi join cafe pass with spaces\n"),
            [Command::WifiJoin {
                ssid: Some("cafe".to_owned()),
                password: Some("pass with spaces".to_owned())
            }]
        );
    }

    #[test]
    fn credentials_are_validated() {
        assert_eq!(credential_error("home", "secret123"), None);
        assert_eq!(credential_error("home", ""), None, "open network");
        assert!(credential_error("", "secret123").is_some(), "empty ssid");
        assert!(credential_error(&"a".repeat(33), "secret123").is_some());
        assert!(credential_error("home", "short").is_some(), "5 bytes");
        assert!(credential_error("home", &"p".repeat(64)).is_some());
        assert_eq!(credential_error("home", &"p".repeat(63)), None);
    }

    #[test]
    fn longest_credential_line_fits() {
        // 32-byte SSID plus 63-byte password is the worst case the protocol allows.
        let mut line = format!("wifi join {} {}\n", "s".repeat(32), "p".repeat(63)).into_bytes();
        assert_eq!(feed(&line).len(), 1);
        line = format!("wifi join {} {}\n", "s".repeat(32), "p".repeat(64)).into_bytes();
        assert_eq!(
            feed(&line),
            [Command::WifiJoin {
                ssid: Some("s".repeat(32)),
                password: Some("p".repeat(64)),
            }]
        );
    }

    #[test]
    fn reject_invalid_and_unknown_commands() {
        assert_eq!(
            feed(b"led\0 on\nreboot\n\ninfo\n"),
            [Command::Unknown, Command::Unknown, Command::Info]
        );
    }
}
