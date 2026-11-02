//! Default settings persisted on the SD card.
//!
//! The board has no usable key/value store: NVS is never initialised and the
//! radio is configured with flash storage disabled, so anything kept in RAM dies
//! at the next reset — and the S3's USB-Serial/JTAG resets the chip every time a
//! host opens or closes the port. A plain file on the card is therefore the only
//! place settings survive.
//!
//! This module deliberately contains no I/O and no HAL types, so it compiles and
//! tests standalone like `panel`, `console` and `pcm`. The caller reads and
//! writes the file.
//!
//! The file lives at [`FILE`] with an 8.3 name because FATFS runs with
//! `CONFIG_FATFS_LFN_NONE`, so long names do not exist on this volume.

/// Directory the firmware is allowed to write; shared with the audio test files.
pub const DIR: &str = "EASELTEST";
/// Path below `/sdcard`. 8.3 uppercase: long filenames are compiled out of FATFS.
pub const FILE: &str = "EASELTEST/CONFIG.TXT";
/// Refuse anything larger: a settings file should never need to be big, and a
/// bounded read keeps a corrupt card from exhausting the heap. 4 KB leaves room
/// for the rendered header plus every key at its longest; 1 KB stopped fitting
/// once the power keys arrived, and a frame must always be able to read back
/// the file it wrote.
pub const MAX_BYTES: usize = 4 * 1024;
const MAX_LINES: usize = 64;
/// Longest path the slideshow setting accepts. FAT here is 8.3, so this is
/// generous already; the bound exists so a corrupt line cannot allocate.
const MAX_PATH: usize = 96;
const MAX_SSID: usize = 32;
const MIN_PASSWORD: usize = 8;
const MAX_PASSWORD: usize = 63;
// Duplicated from `audio` so this module stays standalone-compilable; the
// authoritative limits are `audio::MIN_VOLUME_DB..=MAX_VOLUME_DB` and the gain
// range, which these mirror.
const MIN_VOLUME_TENTHS: i16 = -240;
const MAX_VOLUME_TENTHS: i16 = 120;
const MIN_GAIN_TENTHS: i16 = 0;
const MAX_GAIN_TENTHS: i16 = 375;
/// Google API keys are long opaque strings; the bounds only catch obvious
/// mistakes such as an empty value or a pasted "AIza..." fragment.
const MIN_API_KEY: usize = 8;
const MAX_API_KEY: usize = 128;
/// Bounds of `web.password`, the upload page's own login. HTTP Basic over plain
/// HTTP is not encryption, so this is a lock on the front door, not a secret
/// channel; it exists so a frame on a shared network is not open to anyone.
pub const MIN_WEB_PASSWORD: usize = 8;
pub const MAX_WEB_PASSWORD: usize = 64;
const MAX_LANG: usize = 16;
const MAX_VOICE: usize = 48;
const MAX_MODEL: usize = 64;

/// A token is a shared secret, not a provider key, so the bounds only catch obvious
/// mistakes: a value short enough to guess, or a pasted blob past any plausible
/// length. Printable ASCII keeps it safe to put in an HTTP header.
fn key_is_valid(value: &str) -> bool {
    (MIN_API_KEY..=MAX_API_KEY).contains(&value.len())
        && value.bytes().all(|byte| (0x21..=0x7E).contains(&byte))
}

/// A page password that may appear in a settings file and in an HTTP header:
/// printable ASCII, long enough not to be guessed, short enough to retype.
fn web_password(value: &str) -> Option<String> {
    ((MIN_WEB_PASSWORD..=MAX_WEB_PASSWORD).contains(&value.len())
        && value.bytes().all(|byte| (0x21..=0x7E).contains(&byte)))
    .then(|| value.to_owned())
}

/// Defaults for the voice assistant, chosen for Russian because that is what this
/// build is for. Everything here is overridable from the settings file.
pub const DEFAULT_AI_LANG: &str = "ru-RU";

/// Bounds of `ai.threshold`, the voice watcher's gate in 16-bit sample units.
/// The same numbers as the console command, kept here because the file parser
/// must reject a value the console would refuse.
pub const MIN_AI_THRESHOLD: u32 = 20;
pub const MAX_AI_THRESHOLD: u32 = 4_000;

/// Bounds of `gallery.secs`, the slideshow interval. One refresh of a Spectra 6
/// panel takes about 31 s, so anything shorter would only queue pictures it can
/// never show; an hour is more than a picture frame needs.
pub const MIN_GALLERY_SECS: u32 = 120;
pub const MAX_GALLERY_SECS: u32 = 3_600;

/// Bounds of `power.sleep_secs`, the timed wake. A minute keeps a mistyped value
/// from turning the frame into a refresh loop (a refresh is ~30 s and costs more
/// than the sleep saves); a day is the longest one picture should stay up.
/// 0 means "never sleep by itself", which is the default.
pub const MIN_SLEEP_SECS: u32 = 60;
pub const MAX_SLEEP_SECS: u32 = 86_400;

/// Bounds and default of `power.awake_secs`: how long the frame stays up after
/// a person woke it or last did something, before it sleeps again.
pub const MIN_AWAKE_SECS: u32 = 30;
pub const MAX_AWAKE_SECS: u32 = 3_600;
pub const DEFAULT_AWAKE_SECS: u32 = 120;

/// What may start a turn when nobody is typing, persisted as `ai.idle`.
///
/// It lives here rather than in the console because the file parser and the
/// console grammar must agree on exactly three names; the console re-exports
/// this type, so there is one enum and one set of spellings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdleMode {
    /// No watcher: the frame answers only what is typed.
    #[default]
    Off,
    /// A press of KEY (GPIO4) anywhere, at any time, starts a turn.
    Key,
    /// Sustained loudness on the microphones starts a turn.
    Voice,
}

impl IdleMode {
    /// Parse a mode name exactly as the console and the file write it.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "key" => Some(Self::Key),
            "voice" => Some(Self::Voice),
            _ => None,
        }
    }

    /// Canonical spelling for the console and the file.
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Key => "key",
            Self::Voice => "voice",
        }
    }
}

/// Shape check for `ai.url`, deliberately shallow: scheme, host, optional port and
/// optional path prefix, nothing that needs a DNS resolver to reject. `bridge::Base`
/// parses the same string for real; this only keeps a typo out of the config file
/// before it can reach a socket.
pub fn valid_ai_url(url: &str) -> bool {
    if url.len() > MAX_AI_URL || url.len() < 10 {
        return false;
    }
    let rest = match url.strip_prefix("http://") {
        Some(rest) => rest,
        None => match url.strip_prefix("https://") {
            Some(rest) => rest,
            None => return false,
        },
    };
    let (authority, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty()
        || !authority
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
    {
        return false;
    }
    prefix.len() <= 32 && !prefix.contains([' ', '?', '#'])
}
/// Both of these are hints, not credentials or endpoints: the bridge receives them as
/// `X-Voice` and `X-Model` and decides what to do with them. The defaults name the
/// Google-hosted Standard Russian voice and a Gemini model because that is what the
/// bridge's `cmd:` and cloud backends were first pointed at, but a fully local bridge
/// can ignore both, and `ai.voice`/`ai.model` in the settings file override them.
pub const DEFAULT_AI_VOICE: &str = "ru-RU-Standard-B";
pub const DEFAULT_AI_MODEL: &str = "gemini-2.5-flash";

/// Longest bridge base URL accepted, in bytes.
pub const MAX_AI_URL: usize = 96;

/// Settings as read from the card. `None` means "not configured", which is
/// distinct from a configured-but-empty value such as an open network password.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub ssid: Option<String>,
    /// `Some("")` selects an open network; `None` means no password is known.
    pub password: Option<String>,
    pub wifi_auto: bool,
    pub volume_tenths: Option<i16>,
    pub gain_tenths: Option<i16>,
    /// LAN bridge base URL such as `http://192.168.50.10:8080`.
    pub ai_url: Option<String>,
    /// The bridge a sleeping frame asks for its next picture on a timed wake
    /// (`bridge.url`). Opt-in: joining Wi-Fi costs charge on every wake, so
    /// without it a timed wake shows the card's next picture radio-off.
    pub pull_url: Option<String>,
    /// Optional shared token the bridge may require, sent as a bearer token. It is
    /// the device's own secret, not a cloud provider's key.
    pub ai_key: Option<String>,
    /// BCP-47 tag such as `ru-RU`; speaks for both recognition and synthesis.
    pub ai_lang: Option<String>,
    /// TTS voice name such as `ru-RU-Standard-B`.
    pub ai_voice: Option<String>,
    /// Gemini model such as `gemini-2.5-flash`.
    pub ai_model: Option<String>,
    /// Who may start a turn with nobody typing. `None` means the built-in
    /// default, which is off: a frame must not start listening merely because
    /// an old settings file predates this key.
    pub ai_idle: Option<IdleMode>,
    /// Loudness gate for the voice watcher, in 16-bit sample units. `None`
    /// means the built-in default, so an old file keeps the compiled value.
    pub ai_threshold: Option<u16>,
    /// Directory on the card the slideshow reads, relative to its root.
    pub gallery_dir: Option<String>,
    /// Seconds between pictures.
    pub gallery_secs: Option<u16>,
    /// Whether the slideshow starts by itself at boot.
    pub gallery_auto: bool,
    /// Seconds between timed wakes; `None` or 0 means the frame never sleeps by
    /// itself (a PWR click still puts it to sleep, waking only on a button).
    pub sleep_secs: Option<u32>,
    /// Seconds of inactivity before an awake frame sleeps, when sleeping is on.
    pub awake_secs: Option<u32>,
    /// Password for the upload page. `None` means the page is open, which is the
    /// default: a frame on a home LAN has no more visitors than the photos do.
    pub web_password: Option<String>,
    /// Whether an OpenDisplay client may restart the frame. `None` means yes,
    /// as the protocol expects; `opendisplay.reboot=0` refuses it, because the
    /// LAN transport has no authentication.
    pub opendisplay_reboot: Option<bool>,
}

impl Settings {
    /// Parse the file text. Errors are complete console replies.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_BYTES {
            return Err(format!(
                "ERR config is {} bytes, limit {MAX_BYTES}\r\n",
                text.len()
            ));
        }
        let mut settings = Self::default();
        let mut lines = 0_usize;
        for (index, raw) in text.lines().enumerate() {
            let number = index + 1;
            lines += 1;
            if lines > MAX_LINES {
                return Err(format!("ERR config has more than {MAX_LINES} lines\r\n"));
            }
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("ERR config line {number}: missing '='\r\n"));
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "wifi.ssid" => {
                    let ssid = value.to_owned();
                    if ssid.is_empty() || ssid.len() > MAX_SSID {
                        return Err(format!(
                            "ERR config line {number}: wifi.ssid must be 1..={MAX_SSID} bytes\r\n"
                        ));
                    }
                    if ssid.chars().any(char::is_control) {
                        return Err(format!(
                            "ERR config line {number}: wifi.ssid has a control character\r\n"
                        ));
                    }
                    settings.ssid = Some(ssid);
                }
                "wifi.password" => {
                    // Empty is meaningful: it selects an open network.
                    if !value.is_empty() && !(MIN_PASSWORD..=MAX_PASSWORD).contains(&value.len()) {
                        return Err(format!(
                            "ERR config line {number}: wifi.password must be empty or {MIN_PASSWORD}..={MAX_PASSWORD} bytes\r\n"
                        ));
                    }
                    settings.password = Some(value.to_owned());
                }
                "wifi.auto" => {
                    settings.wifi_auto = parse_bool(value).ok_or_else(|| {
                        format!("ERR config line {number}: wifi.auto must be 0 or 1\r\n")
                    })?;
                }
                "audio.volume_db" => {
                    let tenths = parse_tenths(value).ok_or_else(|| {
                        format!("ERR config line {number}: audio.volume_db is not a number\r\n")
                    })?;
                    if !(MIN_VOLUME_TENTHS..=MAX_VOLUME_TENTHS).contains(&tenths) {
                        return Err(format!(
                            "ERR config line {number}: audio.volume_db must be {}..={}\r\n",
                            tenths_text(MIN_VOLUME_TENTHS),
                            tenths_text(MAX_VOLUME_TENTHS)
                        ));
                    }
                    settings.volume_tenths = Some(tenths);
                }
                "audio.gain_db" => {
                    let tenths = parse_tenths(value).ok_or_else(|| {
                        format!("ERR config line {number}: audio.gain_db is not a number\r\n")
                    })?;
                    if !(MIN_GAIN_TENTHS..=MAX_GAIN_TENTHS).contains(&tenths) {
                        return Err(format!(
                            "ERR config line {number}: audio.gain_db must be {}..={}\r\n",
                            tenths_text(MIN_GAIN_TENTHS),
                            tenths_text(MAX_GAIN_TENTHS)
                        ));
                    }
                    settings.gain_tenths = Some(tenths);
                }
                "ai.url" => {
                    if !valid_ai_url(value) {
                        return Err(format!(
                            "ERR config line {number}: ai.url must be http://host[:port][/prefix]\r\n"
                        ));
                    }
                    settings.ai_url = Some(value.to_owned());
                }
                "bridge.url" => {
                    if !valid_ai_url(value) {
                        return Err(format!(
                            "ERR config line {number}: bridge.url must be http://host[:port]\r\n"
                        ));
                    }
                    settings.pull_url = Some(value.to_owned());
                }
                "ai.key" => {
                    if !key_is_valid(value) {
                        return Err(format!(
                            "ERR config line {number}: ai.key must be {MIN_API_KEY}..={MAX_API_KEY} printable bytes\r\n"
                        ));
                    }
                    settings.ai_key = Some(value.to_owned());
                }
                "ai.lang" => {
                    if !valid_lang(value) {
                        return Err(format!(
                            "ERR config line {number}: ai.lang must look like ru-RU (1..={MAX_LANG} bytes)\r\n"
                        ));
                    }
                    settings.ai_lang = Some(value.to_owned());
                }
                "ai.voice" => {
                    let valid = !value.is_empty()
                        && value.len() <= MAX_VOICE
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
                    if !valid {
                        return Err(format!(
                            "ERR config line {number}: ai.voice must be 1..={MAX_VOICE} of [A-Za-z0-9-]\r\n"
                        ));
                    }
                    settings.ai_voice = Some(value.to_owned());
                }
                "ai.model" => {
                    let valid = !value.is_empty()
                        && value.len() <= MAX_MODEL
                        && value.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_')
                        });
                    if !valid {
                        return Err(format!(
                            "ERR config line {number}: ai.model must be 1..={MAX_MODEL} of [A-Za-z0-9._-]\r\n"
                        ));
                    }
                    settings.ai_model = Some(value.to_owned());
                }
                "ai.idle" => {
                    settings.ai_idle = Some(IdleMode::parse(value).ok_or_else(|| {
                        format!("ERR config line {number}: ai.idle must be off, key or voice\r\n")
                    })?);
                }
                "ai.threshold" => {
                    let parsed = value
                        .parse::<u32>()
                        .ok()
                        .filter(|level| (MIN_AI_THRESHOLD..=MAX_AI_THRESHOLD).contains(level))
                        .and_then(|level| u16::try_from(level).ok());
                    settings.ai_threshold = Some(parsed.ok_or_else(|| {
                        format!(
                            "ERR config line {number}: ai.threshold must be {MIN_AI_THRESHOLD}..={MAX_AI_THRESHOLD}\r\n"
                        )
                    })?);
                }
                "gallery.dir" => {
                    if value.is_empty()
                        || value.starts_with('/')
                        || value.contains('\\')
                        || value
                            .split('/')
                            .any(|part| part.is_empty() || part == "." || part == "..")
                        || value.len() > MAX_PATH
                    {
                        return Err(format!(
                            "ERR config line {number}: gallery.dir must be a relative path of at most {MAX_PATH} bytes\r\n"
                        ));
                    }
                    settings.gallery_dir = Some(value.to_owned());
                }
                "gallery.secs" => {
                    let parsed = value
                        .parse::<u32>()
                        .ok()
                        .filter(|secs| (MIN_GALLERY_SECS..=MAX_GALLERY_SECS).contains(secs))
                        .and_then(|secs| u16::try_from(secs).ok());
                    settings.gallery_secs = Some(parsed.ok_or_else(|| {
                        format!(
                            "ERR config line {number}: gallery.secs must be {MIN_GALLERY_SECS}..={MAX_GALLERY_SECS}\r\n"
                        )
                    })?);
                }
                "power.sleep_secs" => {
                    let parsed = value.parse::<u32>().ok().filter(|secs| {
                        *secs == 0 || (MIN_SLEEP_SECS..=MAX_SLEEP_SECS).contains(secs)
                    });
                    settings.sleep_secs = Some(parsed.ok_or_else(|| {
                        format!(
                            "ERR config line {number}: power.sleep_secs must be 0 or {MIN_SLEEP_SECS}..={MAX_SLEEP_SECS}\r\n"
                        )
                    })?);
                }
                "power.awake_secs" => {
                    let parsed = value
                        .parse::<u32>()
                        .ok()
                        .filter(|secs| (MIN_AWAKE_SECS..=MAX_AWAKE_SECS).contains(secs));
                    settings.awake_secs = Some(parsed.ok_or_else(|| {
                        format!(
                            "ERR config line {number}: power.awake_secs must be {MIN_AWAKE_SECS}..={MAX_AWAKE_SECS}\r\n"
                        )
                    })?);
                }
                "gallery.auto" => {
                    settings.gallery_auto = value == "1";
                }
                "web.password" => {
                    settings.web_password = Some(web_password(value).ok_or_else(|| {
                        format!(
                            "ERR config line {number}: web.password must be {MIN_WEB_PASSWORD}..={MAX_WEB_PASSWORD} printable bytes\r\n"
                        )
                    })?);
                }
                "opendisplay.reboot" => {
                    settings.opendisplay_reboot = Some(match value {
                        "0" => false,
                        "1" => true,
                        _ => {
                            return Err(format!(
                                "ERR config line {number}: opendisplay.reboot must be 0 or 1\r\n"
                            ));
                        }
                    });
                }
                other => {
                    return Err(format!("ERR config line {number}: unknown key {other}\r\n"));
                }
            }
        }
        Ok(settings)
    }

    /// Canonical file text, including the passphrase when one is known.
    ///
    /// The card sits inside the device, so reading it already requires physical
    /// access to the hardware - at which point the network is reachable anyway.
    /// Hiding the passphrase would therefore buy nothing and would only mean
    /// retyping it after every reboot.
    pub fn render(&self) -> String {
        let mut text = String::from(
            "# easellet settings; one key=value per line, '#' starts a comment.\n\
             # 8.3 name: FATFS is built without long filename support.\n\
             # keys: wifi.auto (1 = join at boot and stay online), wifi.ssid,\n\
             #       wifi.password, audio.volume_db (-24.0..12.0),\n\
             #       audio.gain_db (0.0..37.5), ai.url, ai.key, ai.lang,\n\
             #       ai.voice, ai.model, ai.idle (voice assistant; ai.url is the LAN\n\
             #       bridge and ai.key its optional token; ai.idle=off|key|voice;\n\
             #       ai.threshold is the voice gate in sample units;
             #       gallery.dir, gallery.secs (120..3600), gallery.auto (1 = start
             #       the slideshow at boot); web.password locks the upload page;\n\
             #       opendisplay.reboot=0 stops OpenDisplay clients restarting it;\n\
             #       bridge.url (timed wakes fetch the bridge's picture for this frame);\n\
             #       power.sleep_secs (0 = never, or 60..86400: deep sleep, waking\n\
             #       that often for the next picture), power.awake_secs (30..3600);\n\
             #       the clear, so keep this card physically controlled)\n",
        );
        text.push_str(&format!("wifi.auto={}\n", u8::from(self.wifi_auto)));
        if let Some(ssid) = &self.ssid {
            text.push_str(&format!("wifi.ssid={ssid}\n"));
        }
        if let Some(password) = &self.password {
            text.push_str(&format!("wifi.password={password}\n"));
        }
        if let Some(tenths) = self.volume_tenths {
            text.push_str(&format!("audio.volume_db={}\n", tenths_text(tenths)));
        }
        if let Some(tenths) = self.gain_tenths {
            text.push_str(&format!("audio.gain_db={}\n", tenths_text(tenths)));
        }
        if let Some(url) = &self.ai_url {
            text.push_str(&format!("ai.url={url}\n"));
        }
        if let Some(url) = &self.pull_url {
            text.push_str(&format!("bridge.url={url}\n"));
        }
        if let Some(key) = &self.ai_key {
            text.push_str(&format!("ai.key={key}\n"));
        }
        if let Some(lang) = &self.ai_lang {
            text.push_str(&format!("ai.lang={lang}\n"));
        }
        if let Some(voice) = &self.ai_voice {
            text.push_str(&format!("ai.voice={voice}\n"));
        }
        if let Some(model) = &self.ai_model {
            text.push_str(&format!("ai.model={model}\n"));
        }
        if let Some(idle) = self.ai_idle {
            text.push_str(&format!("ai.idle={}\n", idle.name()));
        }
        if let Some(threshold) = self.ai_threshold {
            text.push_str(&format!("ai.threshold={threshold}\n"));
        }
        if let Some(dir) = &self.gallery_dir {
            text.push_str(&format!("gallery.dir={dir}\n"));
        }
        if let Some(secs) = self.gallery_secs {
            text.push_str(&format!("gallery.secs={secs}\n"));
        }
        if self.gallery_auto {
            text.push_str("gallery.auto=1\n");
        }
        if let Some(password) = &self.web_password {
            text.push_str(&format!("web.password={password}\n"));
        }
        if let Some(secs) = self.sleep_secs {
            text.push_str(&format!("power.sleep_secs={secs}\n"));
        }
        if let Some(secs) = self.awake_secs {
            text.push_str(&format!("power.awake_secs={secs}\n"));
        }
        if let Some(allowed) = self.opendisplay_reboot {
            text.push_str(&format!("opendisplay.reboot={}\n", u8::from(allowed)));
        }
        text
    }

    /// Seconds between timed wakes; 0 means the frame never sleeps by itself.
    pub fn sleep_secs(&self) -> u32 {
        self.sleep_secs.unwrap_or(0)
    }

    /// How long an awake frame waits, with nothing happening, before sleeping.
    pub fn awake_secs(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(self.awake_secs.unwrap_or(DEFAULT_AWAKE_SECS)))
    }

    /// Whether an OpenDisplay client's reboot request is honoured.
    pub fn opendisplay_reboot(&self) -> bool {
        self.opendisplay_reboot.unwrap_or(true)
    }

    /// Optional shared token for the bridge. `None` means the bridge is open, which
    /// is the usual arrangement on a home LAN.
    pub fn bridge_token(&self) -> Option<&str> {
        self.ai_key.as_deref()
    }

    /// Bridge base URL, if one was configured. The assistant has no default: a
    /// wrong default would silently talk to a stranger's machine.
    pub fn bridge_url(&self) -> Option<&str> {
        self.ai_url.as_deref()
    }

    /// Recognition and synthesis language, defaulting to Russian.
    pub fn lang(&self) -> &str {
        self.ai_lang.as_deref().unwrap_or(DEFAULT_AI_LANG)
    }

    /// TTS voice hint passed to the bridge.
    pub fn voice(&self) -> &str {
        self.ai_voice.as_deref().unwrap_or(DEFAULT_AI_VOICE)
    }

    /// Language-model hint passed to the bridge.
    pub fn model(&self) -> &str {
        self.ai_model.as_deref().unwrap_or(DEFAULT_AI_MODEL)
    }

    /// Who may start a turn with nobody typing, defaulting to off.
    pub fn idle_mode(&self) -> IdleMode {
        self.ai_idle.unwrap_or_default()
    }

    /// Voice gate this file asks for, if any. The caller falls back to the
    /// compiled default, which is where the value lives until somebody tunes it.
    pub fn idle_threshold(&self) -> Option<f32> {
        self.ai_threshold.map(f32::from)
    }

    /// Slideshow directory, if the file names one.
    pub fn gallery_dir(&self) -> Option<&str> {
        self.gallery_dir.as_deref()
    }

    /// Slideshow interval, defaulting to five minutes and clamped to the bounds
    /// the file accepts. `gallery.rs` holds the scheduling rules; the numbers are
    /// repeated here so this module stays compilable on its own, and a console
    /// test asserts the two agree.
    pub fn gallery_interval(&self) -> std::time::Duration {
        let secs = self.gallery_secs.map_or(300, u32::from);
        std::time::Duration::from_secs(u64::from(secs.clamp(MIN_GALLERY_SECS, MAX_GALLERY_SECS)))
    }

    /// True when the slideshow should start at boot.
    pub fn gallery_auto(&self) -> bool {
        self.gallery_auto
    }

    /// Password the upload page should require, if the file names one.
    pub fn web_password(&self) -> Option<&str> {
        self.web_password.as_deref()
    }

    /// Validate and set the upload page password. Empty removes it, which is how
    /// the portal's own settings form turns the lock back off.
    ///
    /// Clearing is a separate call because an empty *form field* means "leave it
    /// alone": the portal sends `-` to remove the lock, and there is no other way
    /// back to an open page once a password is on the card.
    pub fn set_web_password(&mut self, value: &str) -> Result<String, String> {
        if value.is_empty() {
            self.web_password = None;
            return Ok("OK web.password cleared; the page is open again\r\n".to_owned());
        }
        let parsed = web_password(value).ok_or_else(|| {
            format!(
                "ERR web.password must be {MIN_WEB_PASSWORD}..={MAX_WEB_PASSWORD} printable bytes\r\n"
            )
        })?;
        self.web_password = Some(parsed);
        Ok(format!(
            "OK web.password=<set:{} chars> (applies when the listener restarts)\r\n",
            value.len()
        ))
    }

    /// Set the bridge URL from a console command, live, without touching the card.
    ///
    /// Validation lives here rather than in the console so a value accepted now
    /// cannot fail on the next load, and vice versa: both paths call the same checks
    /// the file parser uses. `-` removes the setting, which is the only way back to
    /// "no bridge configured" without editing the file.
    pub fn set_ai_url(&mut self, value: &str) -> Result<String, String> {
        if value == "-" {
            self.ai_url = None;
            return Ok("OK ai.url cleared\r\n".to_owned());
        }
        if !valid_ai_url(value) {
            return Err(format!(
                "ERR ai.url must be http://host[:port][/prefix], 10..={MAX_AI_URL} bytes\r\n"
            ));
        }
        self.ai_url = Some(value.to_owned());
        Ok(format!(
            "OK ai.url={value} (live; `config save` writes it to {FILE})\r\n"
        ))
    }

    /// Set or clear the bridge token. The value is never echoed: the reply states the
    /// length only, so a console log cannot leak it.
    pub fn set_ai_key(&mut self, value: &str) -> Result<String, String> {
        if value == "-" {
            self.ai_key = None;
            return Ok("OK ai.key cleared\r\n".to_owned());
        }
        if !key_is_valid(value) {
            return Err(format!(
                "ERR ai.key must be {MIN_API_KEY}..={MAX_API_KEY} printable bytes\r\n"
            ));
        }
        let chars = value.len();
        self.ai_key = Some(value.to_owned());
        Ok(format!(
            "OK ai.key=<set:{chars} chars> (live; `config save` writes it to {FILE})\r\n"
        ))
    }

    /// Credentials usable for a join, if both halves are known.
    pub fn credentials(&self) -> Option<(&str, &str)> {
        match (&self.ssid, &self.password) {
            (Some(ssid), Some(password)) => Some((ssid, password)),
            _ => None,
        }
    }

    /// One-line, password-redacting summary for the console.
    pub fn summary(&self) -> String {
        format!(
            "OK config ssid={} password={} auto={} volume_db={} gain_db={}\r\n",
            self.ssid.as_deref().unwrap_or("<none>"),
            match &self.password {
                None => "<none>",
                Some(password) if password.is_empty() => "<open>",
                Some(_) => "<set>",
            },
            u8::from(self.wifi_auto),
            self.volume_tenths
                .map_or("<default>".to_owned(), tenths_text),
            self.gain_tenths.map_or("<default>".to_owned(), tenths_text),
        ) + &format!(
            "  ai_url={} ai_key={} lang={} voice={} model={} idle={} threshold={}\r\n",
            self.ai_url.as_deref().unwrap_or("<none>"),
            if self.ai_key.is_some() {
                "<set>"
            } else {
                "<none>"
            },
            self.lang(),
            self.voice(),
            self.model(),
            self.idle_mode().name(),
            self.ai_threshold
                .map_or("<default>".to_owned(), |value| value.to_string()),
        ) + &format!(
            "  bridge_url={} sleep_secs={} awake_secs={}\r\n",
            self.pull_url.as_deref().unwrap_or("<none>"),
            self.sleep_secs(),
            self.awake_secs().as_secs(),
        )
    }
}

/// A BCP-47-shaped tag such as `ru-RU` or `en-US`, optionally with extra
/// subtags. Deliberately permissive beyond the shape check: the endpoint is the
/// authority on which languages it accepts, and rejecting a valid future tag here
/// would be a nuisance rather than a safety net.
fn valid_lang(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_LANG {
        return false;
    }
    let mut parts = value.split('-');
    let Some(primary) = parts.next() else {
        return false;
    };
    if !(2..=3).contains(&primary.len()) || !primary.bytes().all(|byte| byte.is_ascii_alphabetic())
    {
        return false;
    }
    parts.all(|part| {
        (2..=8).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "yes" => Some(true),
        "0" | "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// Decibels as tenths, matching the console's `audio volume`/`audio gain`
/// number syntax: an optional sign, digits, and at most one decimal.
fn parse_tenths(value: &str) -> Option<i16> {
    let (negative, rest) = match value.as_bytes().first()? {
        b'-' => (true, &value[1..]),
        b'+' => (false, &value[1..]),
        _ => (false, value),
    };
    let (whole, fraction) = match rest.split_once('.') {
        // `.5` and `6.` are rejected: the console grammar needs digits on both
        // sides when a decimal point appears.
        Some((whole, fraction)) => {
            if whole.is_empty() || fraction.len() != 1 {
                return None;
            }
            (whole, Some(fraction))
        }
        None => (rest, None),
    };
    if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let whole: i32 = whole.parse().ok()?;
    let fraction: i32 = match fraction {
        Some(fraction) => fraction.parse().ok()?,
        None => 0,
    };
    let tenths = whole.checked_mul(10)?.checked_add(fraction)?;
    let tenths = if negative { -tenths } else { tenths };
    i16::try_from(tenths).ok()
}

fn tenths_text(tenths: i16) -> String {
    let magnitude = tenths.unsigned_abs();
    let sign = if tenths < 0 { "-" } else { "" };
    format!("{sign}{}.{}", magnitude / 10, magnitude % 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "# defaults for the frame\n\
                        wifi.auto=1\n\
                        wifi.ssid=boids\n\
                        wifi.password=hunter2hunter2\n\
                        audio.volume_db=-12.0\n\
                        audio.gain_db=24.5\n";

    #[test]
    fn parses_a_complete_file() {
        let settings = Settings::parse(FULL).expect("valid file");
        assert_eq!(settings.ssid.as_deref(), Some("boids"));
        assert_eq!(settings.password.as_deref(), Some("hunter2hunter2"));
        assert!(settings.wifi_auto);
        assert_eq!(settings.volume_tenths, Some(-120));
        assert_eq!(settings.gain_tenths, Some(245));
        assert_eq!(
            settings.credentials(),
            Some(("boids", "hunter2hunter2")),
            "both halves known"
        );
    }

    #[test]
    fn empty_and_commented_lines_are_skipped() {
        let settings = Settings::parse("\n   \n# comment\n\taudio.gain_db = 30\n").expect("valid");
        assert_eq!(settings.gain_tenths, Some(300));
        assert_eq!(settings.ssid, None);
        assert_eq!(settings.credentials(), None, "ssid without password");
    }

    #[test]
    fn later_keys_win() {
        let settings = Settings::parse("audio.gain_db=10\naudio.gain_db=20\n").expect("valid");
        assert_eq!(settings.gain_tenths, Some(200));
    }

    #[test]
    fn open_network_password_is_distinct_from_unknown() {
        let open = Settings::parse("wifi.ssid=cafe\nwifi.password=\n").expect("valid");
        assert_eq!(open.password.as_deref(), Some(""));
        assert_eq!(open.credentials(), Some(("cafe", "")), "open network");

        let unknown = Settings::parse("wifi.ssid=cafe\n").expect("valid");
        assert_eq!(unknown.password, None);
        assert_eq!(unknown.credentials(), None);
    }

    #[test]
    fn rejects_malformed_lines() {
        assert!(
            Settings::parse("wifi.ssid\n")
                .unwrap_err()
                .contains("missing '='")
        );
        assert!(
            Settings::parse("wifi.auto=maybe\n")
                .unwrap_err()
                .contains("wifi.auto")
        );
        assert!(
            Settings::parse("nonsense.key=1\n")
                .unwrap_err()
                .contains("unknown key")
        );
        assert!(
            Settings::parse("wifi.ssid=\n")
                .unwrap_err()
                .contains("wifi.ssid")
        );
        assert!(
            Settings::parse(&format!("wifi.ssid={}\n", "s".repeat(33)))
                .unwrap_err()
                .contains("wifi.ssid")
        );
        assert!(
            Settings::parse("wifi.password=short\n")
                .unwrap_err()
                .contains("wifi.password")
        );
        assert!(
            Settings::parse("wifi.ssid=ca\tfe\n")
                .unwrap_err()
                .contains("control character")
        );
    }

    #[test]
    fn enforces_audio_ranges() {
        assert_eq!(
            Settings::parse("audio.volume_db=-24.0\n")
                .expect("lower bound")
                .volume_tenths,
            Some(-240)
        );
        assert_eq!(
            Settings::parse("audio.volume_db=12\n")
                .expect("upper bound")
                .volume_tenths,
            Some(120)
        );
        assert!(
            Settings::parse("audio.volume_db=12.1\n")
                .unwrap_err()
                .contains("audio.volume_db")
        );
        assert!(
            Settings::parse("audio.gain_db=37.6\n")
                .unwrap_err()
                .contains("audio.gain_db")
        );
        assert_eq!(
            Settings::parse("audio.gain_db=37.5\n")
                .expect("upper bound")
                .gain_tenths,
            Some(375)
        );
        assert!(Settings::parse("audio.gain_db=abc\n").is_err());
    }

    #[test]
    fn bounds_the_file() {
        let many = "audio.gain_db=1\n".repeat(MAX_LINES + 1);
        assert!(Settings::parse(&many).unwrap_err().contains("lines"));
        let huge = "#".repeat(MAX_BYTES + 1);
        assert!(Settings::parse(&huge).unwrap_err().contains("limit"));
    }

    #[test]
    fn tenths_round_trip_and_grammar() {
        assert_eq!(parse_tenths("-12.0"), Some(-120));
        assert_eq!(parse_tenths("+3.5"), Some(35));
        assert_eq!(parse_tenths("0"), Some(0));
        assert_eq!(parse_tenths("37.5"), Some(375));
        // Same rejections as the console grammar.
        assert_eq!(parse_tenths(".5"), None);
        assert_eq!(parse_tenths("6."), None);
        assert_eq!(parse_tenths("--6"), None);
        assert_eq!(parse_tenths(""), None);
        assert_eq!(parse_tenths("6.55"), None);
        assert_eq!(tenths_text(-120), "-12.0");
        assert_eq!(tenths_text(-5), "-0.5");
        assert_eq!(tenths_text(375), "37.5");
        assert_eq!(tenths_text(0), "0.0");
    }

    #[test]
    fn render_round_trips() {
        let settings = Settings::parse(FULL).expect("valid");
        let rendered = settings.render();
        assert_eq!(Settings::parse(&rendered).expect("re-parse"), settings);
        assert!(
            rendered.contains("wifi.password=hunter2hunter2"),
            "the passphrase is persisted so a reboot needs no retyping"
        );
    }

    #[test]
    fn render_writes_no_password_line_when_none_is_known() {
        let settings = Settings::parse("wifi.ssid=boids\n").expect("valid");
        // Match the key line, not the header legend that names the key.
        let rendered = settings.render();
        assert!(
            !rendered
                .lines()
                .any(|line| line.starts_with("wifi.password=")),
            "no password line"
        );
        assert_eq!(
            Settings::parse(&settings.render())
                .expect("re-parse")
                .password,
            None,
            "absent, not blanked into an open network"
        );
    }

    #[test]
    fn a_page_password_is_validated_and_round_trips() {
        let mut settings = Settings::default();
        assert_eq!(settings.web_password(), None, "open by default");
        assert!(settings.set_web_password("short").is_err());
        assert!(settings.set_web_password("has space").is_err());
        assert!(settings.set_web_password(&"x".repeat(65)).is_err());
        assert!(settings.set_web_password("easellet-lan").is_ok());
        let rendered = settings.render();
        assert!(
            rendered
                .lines()
                .any(|line| line == "web.password=easellet-lan")
        );
        assert_eq!(
            Settings::parse(&rendered).expect("valid").web_password(),
            Some("easellet-lan")
        );
        // Empty clears it: the portal's own form must be able to unlock the page.
        assert!(settings.set_web_password("").is_ok());
        assert!(
            !settings
                .render()
                .lines()
                .any(|line| line.starts_with("web.password="))
        );
    }

    #[test]
    fn summary_never_shows_the_password() {
        let settings = Settings::parse(FULL).expect("valid");
        let summary = settings.summary();
        assert!(!summary.contains("hunter2hunter2"));
        assert!(summary.contains("password=<set>"));
        assert!(summary.contains("ssid=boids"));
        assert!(summary.contains("volume_db=-12.0"));
        assert!(Settings::default().summary().contains("password=<none>"));
        // The gate survives a reset only if this file names it, so the summary has to
        // print what boot will apply rather than what the console happens to hold.
        let tuned = Settings::parse("ai.threshold=250\n").expect("valid");
        assert!(tuned.summary().contains("threshold=250"));
        assert!(
            Settings::default()
                .summary()
                .contains("threshold=<default>")
        );
    }

    /// A realistic Google API key: 39 characters starting with the usual prefix.
    const API_KEY: &str = "AIzaSyD-abcdefghijklmnopqrstuvwxyz012345";

    /// The assistant block as it would appear on the card.
    const AI: &str = "ai.url=http://192.168.50.10:8080\n\
                      ai.key=AIzaSyD-abcdefghijklmnopqrstuvwxyz012345\n\
                      ai.lang=ru-RU\n\
                      ai.voice=ru-RU-Standard-B\n\
                      ai.model=gemini-2.5-flash\n\
                      ai.idle=key\n\
                      ai.threshold=250\n";

    #[test]
    fn a_file_with_every_key_at_its_longest_reads_back() {
        // Whatever the frame writes it must be able to read at the next boot.
        let url = format!("http://{}", "h".repeat(MAX_AI_URL - "http://".len()));
        let dir = ["ABCDEFGH"; 10].join("/");
        let dir = &dir[..MAX_PATH.min(dir.len())];
        let text = format!(
            "wifi.auto=1\nwifi.ssid={}\nwifi.password={}\naudio.volume_db=-24.0\n\
             audio.gain_db=37.5\nai.url={url}\nai.key={}\nai.lang=ru-RU\nai.voice={}\n\
             ai.model={}\nai.idle=voice\nai.threshold=4000\ngallery.dir={}\n\
             gallery.secs=3600\ngallery.auto=1\nweb.password={}\n\
             power.sleep_secs=86400\npower.awake_secs=3600\nopendisplay.reboot=0\n\
             bridge.url={url}\n",
            "s".repeat(MAX_SSID),
            "p".repeat(MAX_PASSWORD),
            "k".repeat(MAX_API_KEY),
            "v".repeat(MAX_VOICE),
            "m".repeat(MAX_MODEL),
            dir.trim_end_matches('/'),
            "w".repeat(MAX_WEB_PASSWORD),
        );
        let settings = Settings::parse(&text).expect("every key valid");
        let rendered = settings.render();
        assert!(rendered.len() <= MAX_BYTES, "{} bytes", rendered.len());
        Settings::parse(&rendered).expect("the frame reads back its own file");
    }

    #[test]
    fn the_pull_url_is_opt_in_and_validated() {
        assert!(Settings::default().pull_url.is_none());
        let set = Settings::parse("bridge.url=http://192.168.50.93:8095\n").expect("valid");
        assert_eq!(set.pull_url.as_deref(), Some("http://192.168.50.93:8095"));
        assert!(
            set.render()
                .contains("bridge.url=http://192.168.50.93:8095\n")
        );
        assert!(Settings::parse("bridge.url=ftp://x\n").is_err());
    }

    #[test]
    fn power_settings_are_bounded_and_round_trip() {
        let defaults = Settings::default();
        assert_eq!(defaults.sleep_secs(), 0);
        assert_eq!(
            defaults.awake_secs().as_secs(),
            u64::from(DEFAULT_AWAKE_SECS)
        );
        let set = Settings::parse("power.sleep_secs=600\npower.awake_secs=60\n").expect("valid");
        assert_eq!(set.sleep_secs(), 600);
        assert_eq!(set.awake_secs().as_secs(), 60);
        let again = Settings::parse(&set.render()).expect("re-parse");
        assert_eq!(
            (again.sleep_secs(), again.awake_secs()),
            (600, set.awake_secs())
        );
        assert_eq!(
            Settings::parse("power.sleep_secs=0\n")
                .unwrap()
                .sleep_secs(),
            0
        );
        for bad in [
            "power.sleep_secs=59\n",
            "power.sleep_secs=86401\n",
            "power.sleep_secs=soon\n",
            "power.awake_secs=29\n",
            "power.awake_secs=3601\n",
            "power.awake_secs=0\n",
        ] {
            assert!(Settings::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn opendisplay_reboot_defaults_to_allowed_and_round_trips() {
        assert!(Settings::default().opendisplay_reboot());
        let refused = Settings::parse("opendisplay.reboot=0\n").expect("valid");
        assert!(!refused.opendisplay_reboot());
        assert!(refused.render().contains("opendisplay.reboot=0\n"));
        assert!(
            !Settings::parse(&refused.render())
                .expect("re-parse")
                .opendisplay_reboot()
        );
        assert!(Settings::parse("opendisplay.reboot=no\n").is_err());
    }

    #[test]
    fn the_voice_gate_round_trips_and_is_range_checked() {
        let settings = Settings::parse(AI).expect("valid");
        assert_eq!(settings.idle_threshold(), Some(250.0));
        assert!(settings.render().contains("ai.threshold=250\n"));
        // Absent means the compiled default, so an old card keeps its behaviour.
        assert_eq!(Settings::default().idle_threshold(), None);

        for bad in [
            "ai.threshold=0\n",
            "ai.threshold=19\n",
            "ai.threshold=4001\n",
            "ai.threshold=600.5\n",
            "ai.threshold=\n",
            "ai.threshold=loud\n",
        ] {
            assert!(
                Settings::parse(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        assert_eq!(
            Settings::parse("ai.threshold=20\n")
                .expect("valid")
                .idle_threshold(),
            Some(20.0)
        );
        assert_eq!(
            Settings::parse("ai.threshold=4000\n")
                .expect("valid")
                .idle_threshold(),
            Some(4000.0)
        );
    }

    #[test]
    fn assistant_defaults_to_russian() {
        let settings = Settings::default();
        // The whole point of this build: no configuration still speaks Russian
        // rather than falling back to English or the original sketch's Italian.
        assert_eq!(settings.lang(), "ru-RU");
        assert_eq!(settings.voice(), "ru-RU-Standard-B");
        assert_eq!(settings.model(), "gemini-2.5-flash");
        assert_eq!(settings.bridge_url(), None);
        assert_eq!(settings.bridge_token(), None);
        // Silence is the only safe default: an old file must not start listening.
        assert_eq!(settings.idle_mode(), IdleMode::Off);
    }

    #[test]
    fn idle_modes_round_trip_and_are_exactly_three() {
        for mode in [IdleMode::Off, IdleMode::Key, IdleMode::Voice] {
            assert_eq!(IdleMode::parse(mode.name()), Some(mode));
            let text = format!("ai.idle={}\n", mode.name());
            let settings = Settings::parse(&text).expect("valid idle mode");
            assert_eq!(settings.ai_idle, Some(mode));
            assert!(settings.render().contains(text.trim_end()));
        }
        // A mode is remembered as absent when it was never written, so a
        // settings file from before this key existed does not gain one.
        let settings = Settings::parse("wifi.ssid=boids\n").expect("valid");
        assert!(
            !settings
                .render()
                .lines()
                .any(|line| line.starts_with("ai.idle="))
        );
    }

    #[test]
    fn assistant_settings_round_trip() {
        let settings = Settings::parse(AI).expect("valid assistant block");
        assert_eq!(settings.bridge_token(), Some(API_KEY));
        assert_eq!(settings.bridge_url(), Some("http://192.168.50.10:8080"));
        assert_eq!(settings.lang(), "ru-RU");
        assert_eq!(settings.voice(), "ru-RU-Standard-B");
        assert_eq!(settings.model(), "gemini-2.5-flash");
        assert_eq!(settings.idle_mode(), IdleMode::Key);

        // A reload must reproduce every key, or a `config save` would quietly
        // drop the credentials it did not understand.
        let rendered = settings.render();
        for line in [
            "ai.key=",
            "ai.lang=ru-RU",
            "ai.voice=ru-RU-Standard-B",
            "ai.model=gemini-2.5-flash",
            "ai.idle=key",
        ] {
            assert!(rendered.contains(line), "rendered settings lost {line}");
        }
        assert_eq!(Settings::parse(&rendered).expect("reload"), settings);
    }

    #[test]
    fn assistant_keys_are_validated() {
        for bad in [
            "ai.key=short\n",
            "ai.key=AIzaSyD-abcdefghijklmnop qrstuvwxyz0123\n",
            "ai.lang=ru_RU\n",
            "ai.lang=r\n",
            "ai.lang=ru_\n",
            "ai.voice=\n",
            "ai.voice=ru-RU-Wavenet D\n",
            "ai.voice=ru-RU-Wavenet_D\n",
            "ai.model=\n",
            "ai.model=gemini 2.5\n",
            "ai.idle=\n",
            "ai.idle=on\n",
            "ai.idle=Voice\n",
            "ai.idle=voice 1\n",
        ] {
            assert!(
                Settings::parse(bad).is_err(),
                "expected rejection of {bad:?}"
            );
        }
        // Region-less and three-letter primaries are legitimate BCP-47.
        assert!(Settings::parse("ai.lang=ru\n").is_ok());
        assert!(Settings::parse("ai.lang=cmn-Hans-CN\n").is_ok());
    }

    #[test]
    fn bridge_urls_are_validated_by_shape() {
        for good in [
            "http://192.168.50.10:8080",
            "http://pi.local",
            "https://frame.local/bridge",
            "http://host_name:9/b",
        ] {
            assert!(valid_ai_url(good), "{good} should be accepted");
            let text = format!("ai.url={good}\n");
            let settings = Settings::parse(&text).expect(good);
            assert_eq!(settings.bridge_url(), Some(good));
        }
        for bad in [
            "192.168.50.10:8080",
            "ftp://host",
            "http://host/path with space",
            "http://ho st:8080",
            "http://host/path?x=1",
            "",
        ] {
            assert!(!valid_ai_url(bad), "{bad} should be rejected");
            let text = format!("ai.url={bad}\n");
            let error = Settings::parse(&text).expect_err(bad);
            assert!(error.contains("ai.url"), "{bad} -> {error}");
        }
    }

    #[test]
    fn bridge_url_is_saved_even_without_a_token() {
        let settings = Settings::parse("ai.url=http://192.168.50.10:8080\n").expect("parse");
        let rendered = settings.render();
        assert!(rendered.contains("ai.url=http://192.168.50.10:8080"));
        assert!(!rendered.contains("ai.key="));
        // Round trip through the file format keeps the same bridge.
        assert_eq!(
            Settings::parse(&rendered).expect("reparse").bridge_url(),
            settings.bridge_url()
        );
    }

    #[test]
    fn summary_masks_the_api_key() {
        let summary = Settings::parse(AI).expect("valid").summary();
        assert!(!summary.contains(API_KEY), "key must not be printed");
        assert!(summary.contains("ai_key=<set>"));
        assert!(summary.contains("lang=ru-RU"));
        assert!(summary.contains("voice=ru-RU-Standard-B"));
        assert!(Settings::default().summary().contains("ai_key=<none>"));
        assert!(Settings::default().summary().contains("lang=ru-RU"));
    }

    #[test]
    fn summary_shows_the_bridge_url() {
        let summary = Settings::parse(AI).expect("valid").summary();
        assert!(summary.contains("ai_url=http://192.168.50.10:8080"));
        assert!(Settings::default().summary().contains("ai_url=<none>"));
    }

    #[test]
    fn live_bridge_url_matches_what_the_file_parser_accepts() {
        // Setter and parser share `valid_ai_url`, so a value accepted live cannot be
        // rejected on the next boot. Anything the parser refuses, the setter refuses.
        for url in [
            "http://192.168.50.10:8080",
            "http://frame-bridge.lan",
            "http://192.168.50.10:8080/v1",
        ] {
            let mut settings = Settings::default();
            assert!(settings.set_ai_url(url).is_ok(), "{url}");
            assert_eq!(settings.ai_url.as_deref(), Some(url));
            let file = format!("ai.url={url}\n");
            assert_eq!(
                Settings::parse(&file)
                    .expect("parser agrees")
                    .ai_url
                    .as_deref(),
                Some(url)
            );
            assert_eq!(settings.render(), Settings::parse(&file).unwrap().render());
        }
        for url in [
            "",
            "-",
            "ftp://192.168.50.10:8080",
            "192.168.50.10:8080",
            "http://",
            "http://192.168.50.10:8080/a b",
        ] {
            let mut settings = Settings::default();
            if url == "-" {
                // Clearing is the one non-URL the console has to accept.
                settings.ai_url = Some("http://old:8080".to_owned());
                assert!(settings.set_ai_url(url).is_ok());
                assert_eq!(settings.ai_url, None);
                continue;
            }
            assert!(
                settings.set_ai_url(url).is_err(),
                "{url} should be rejected"
            );
            assert_eq!(settings.ai_url, None, "{url} must not be stored");
            assert!(!valid_ai_url(url), "{url} disagrees with the parser");
        }
    }

    #[test]
    fn live_bridge_key_is_bounded_and_never_echoed() {
        let mut settings = Settings::default();
        let reply = settings
            .set_ai_key("easellet-lan-2026")
            .expect("19 printable bytes");
        assert!(!reply.contains("easellet-lan-2026"), "{reply}");
        assert!(reply.contains("19 chars"), "{reply}");
        assert_eq!(settings.bridge_token(), Some("easellet-lan-2026"));
        // The same value has to survive a save/load round trip.
        assert_eq!(
            Settings::parse(&settings.render()).unwrap().bridge_token(),
            Some("easellet-lan-2026")
        );

        for bad in ["", "short", "has space in it", "tab\there"] {
            let mut settings = Settings::default();
            assert!(
                settings.set_ai_key(bad).is_err(),
                "{bad:?} should be rejected"
            );
            assert_eq!(settings.ai_key, None);
        }
        assert!(Settings::default().set_ai_key(&"k".repeat(129)).is_err());

        // `-` is the only way to unset one without editing the card.
        let mut settings = Settings::default();
        settings.set_ai_key("easellet-lan-2026").unwrap();
        assert!(settings.set_ai_key("-").is_ok());
        assert_eq!(settings.ai_key, None);
    }
}
