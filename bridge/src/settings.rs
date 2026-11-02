//! The bridge's settings: `bridge.toml`, the environment, the command line.
//!
//! Precedence, lowest first: built-in defaults, the file, `EASEL_*` variables, flags.
//! A value that came from a flag or a variable wins for this run and is shown as
//! read-only in the web UI, because editing it there would be undone at the next
//! restart. Everything else can be changed from the UI and is written back to the
//! file. When the file does not exist yet it is created from the effective
//! settings, so the first `serve-local.sh` run seeds it and later runs need no
//! flags at all.
//!
//! Some values take effect at once ([`Live`]): the wake stems, the token, the
//! model's system prompt and URL, the UI password and extra host names. The
//! listen address and the three backends are read once at start, and the UI says
//! "applies after a restart" for those.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Everything `bridge.toml` may hold. Missing keys keep their defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Listen address. Restart.
    pub addr: String,
    /// Bearer token the frames must send; empty = open.
    pub token: String,
    /// Recogniser: `stub`, `whisper:<size>` or `cmd:<program>`. Restart.
    pub stt: String,
    /// Model: `stub` or `cmd:<program>`. Restart.
    pub llm: String,
    /// Synthesiser: `stub` or `cmd:<program>`. Restart.
    pub tts: String,
    /// Stems a voice-triggered transcript must contain; empty = all addressed.
    pub wake: Vec<String>,
    /// System prompt for the model command (`EASEL_LLM_SYSTEM`); empty = its default.
    pub llm_system: String,
    /// Text the recogniser treats as just heard: the wake word and names it
    /// should expect ("Рамка, покажи погоду в Эспоо"). Whisper only; empty = none.
    pub stt_prompt: String,
    /// Chat-completions URL for the model command (`EASEL_LLM_URL`); empty = its default.
    pub llm_url: String,
    /// Password for the web UI (HTTP Basic, any user name); empty = open.
    pub ui_password: String,
    /// Host names the UI answers to besides IP addresses and local names (`Host`
    /// check against DNS rebinding), e.g. this machine's FQDN.
    pub ui_hosts: Vec<String>,
    /// Frames added by hand, for networks where mDNS does not reach, and frames
    /// whose portal has a password.
    #[serde(rename = "frame")]
    pub frames: Vec<FrameEntry>,
}

/// One `[[frame]]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FrameEntry {
    /// IP address or host name.
    pub host: String,
    /// What the UI calls it; empty = the name it announces.
    pub name: String,
    /// The frame portal's `web.password`, if it has one.
    pub password: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:8080".to_owned(),
            token: String::new(),
            stt: "stub".to_owned(),
            llm: "stub".to_owned(),
            tts: "stub".to_owned(),
            wake: Vec::new(),
            llm_system: String::new(),
            stt_prompt: String::new(),
            llm_url: String::new(),
            ui_password: String::new(),
            ui_hosts: Vec::new(),
            frames: Vec::new(),
        }
    }
}

/// Keys that only take effect at start.
pub const RESTART_KEYS: &[&str] = &["addr", "stt", "llm", "tts"];
/// Keys whose value is never sent to the browser, only whether it is set.
pub const SECRET_KEYS: &[&str] = &["token", "ui_password"];

/// Longest text value accepted from the UI; the system prompt is the long one.
const MAX_VALUE: usize = 4_096;
/// Most entries in a list value.
const MAX_LIST: usize = 32;

/// The values a running bridge reads on every request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Live {
    pub token: Option<String>,
    pub wake: Vec<String>,
    pub llm_env: Vec<(String, String)>,
    pub stt_prompt: String,
    pub ui_password: Option<String>,
    pub ui_hosts: Vec<String>,
}

impl Settings {
    /// Read `path`; a missing file is the defaults, not an error.
    pub fn load(path: &Path) -> Result<(Self, bool), String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map(|settings| (settings, true))
                .map_err(|error| format!("{}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok((Self::default(), false))
            }
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    /// Write `path` atomically (temporary file, then rename), owner-only because
    /// it holds the token and the UI password.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|error| error.to_string())?;
        let text = format!(
            "# easellet-bridge settings; edited by the web UI as well.\n\
             # Keys: addr, token, stt, llm, tts, wake, llm_system, stt_prompt, llm_url,\n\
             # ui_password, ui_hosts. Flags and EASEL_* variables override this file.\n\n{text}"
        );
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)
                .map_err(|error| format!("{}: {error}", directory.display()))?;
        }
        let temporary = path.with_extension("toml.tmp");
        write_private(&temporary, text.as_bytes())?;
        std::fs::rename(&temporary, path).map_err(|error| format!("{}: {error}", path.display()))
    }

    pub fn live(&self) -> Live {
        let mut llm_env = Vec::new();
        if !self.llm_system.is_empty() {
            llm_env.push(("EASEL_LLM_SYSTEM".to_owned(), self.llm_system.clone()));
        }
        if !self.llm_url.is_empty() {
            llm_env.push(("EASEL_LLM_URL".to_owned(), self.llm_url.clone()));
        }
        Live {
            token: Some(self.token.clone()).filter(|token| !token.is_empty()),
            wake: normalize_wake(&self.wake),
            llm_env,
            stt_prompt: self.stt_prompt.clone(),
            ui_password: Some(self.ui_password.clone()).filter(|password| !password.is_empty()),
            ui_hosts: self
                .ui_hosts
                .iter()
                .map(|host| host.to_ascii_lowercase())
                .collect(),
        }
    }

    /// The settings as the browser sees them: secrets reduced to whether they
    /// are set, and the keys a flag or variable pinned for this run listed.
    pub fn to_json(&self, pinned: &BTreeSet<String>) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or_default();
        if let Some(map) = value.as_object_mut() {
            // Frame passwords become "is set", like the other secrets.
            if let Some(frames) = map
                .get_mut("frame")
                .and_then(serde_json::Value::as_array_mut)
            {
                for frame in frames {
                    let set = frame
                        .get("password")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|password| !password.is_empty());
                    frame["password"] = serde_json::Value::Bool(set);
                }
            }
            for key in SECRET_KEYS {
                let set = map
                    .get(*key)
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|secret| !secret.is_empty());
                map.insert((*key).to_owned(), serde_json::Value::Bool(set));
            }
        }
        serde_json::json!({
            "values": value,
            "pinned": pinned,
            "restart": RESTART_KEYS,
            "secret": SECRET_KEYS,
        })
    }

    /// Apply a change from the UI: a JSON object of keys to new values. A key the
    /// change does not mention is left alone; a secret sent as `""` is left alone
    /// too (the form never had its value), and `"-"` clears it. Pinned keys are
    /// refused. Returns the keys that changed.
    pub fn apply(
        &mut self,
        change: &serde_json::Value,
        pinned: &BTreeSet<String>,
    ) -> Result<Vec<String>, String> {
        let change = change
            .as_object()
            .ok_or_else(|| "settings change must be a JSON object".to_owned())?;
        let mut next = self.clone();
        let mut changed = Vec::new();
        for (key, value) in change {
            if pinned.contains(key) {
                return Err(format!("{key} is set on the command line for this run"));
            }
            let text = |value: &serde_json::Value| -> Result<String, String> {
                let text = value
                    .as_str()
                    .ok_or_else(|| format!("{key} must be text"))?;
                if text.len() > MAX_VALUE || text.contains('\0') {
                    return Err(format!("{key} is too long or contains NUL"));
                }
                Ok(text.to_owned())
            };
            let list = |value: &serde_json::Value| -> Result<Vec<String>, String> {
                let items: Vec<String> = match value {
                    serde_json::Value::Array(items) => {
                        items.iter().map(&text).collect::<Result<_, _>>()?
                    }
                    serde_json::Value::String(joined) => joined
                        .split(',')
                        .map(str::trim)
                        .filter(|item| !item.is_empty())
                        .map(str::to_owned)
                        .collect(),
                    _ => return Err(format!("{key} must be a list")),
                };
                if items.len() > MAX_LIST {
                    return Err(format!("{key} has more than {MAX_LIST} entries"));
                }
                Ok(items)
            };
            match key.as_str() {
                "token" | "ui_password" => {
                    let secret = text(value)?;
                    let field = if key == "token" {
                        &mut next.token
                    } else {
                        &mut next.ui_password
                    };
                    match secret.as_str() {
                        "" => continue,
                        "-" => field.clear(),
                        _ if secret.len() < 8 => {
                            return Err(format!("{key} must be at least 8 characters"));
                        }
                        _ => *field = secret,
                    }
                }
                "addr" => next.addr = valid_addr(&text(value)?)?,
                "stt" => next.stt = valid_backend(&text(value)?, true)?,
                "llm" => next.llm = valid_backend(&text(value)?, false)?,
                "tts" => next.tts = valid_backend(&text(value)?, false)?,
                "wake" => next.wake = list(value)?,
                "llm_system" => next.llm_system = text(value)?,
                "stt_prompt" => next.stt_prompt = text(value)?,
                "llm_url" => {
                    let url = text(value)?;
                    if !(url.is_empty()
                        || url.starts_with("http://")
                        || url.starts_with("https://"))
                    {
                        return Err("llm_url must be an http(s) URL".to_owned());
                    }
                    next.llm_url = url;
                }
                "ui_hosts" => next.ui_hosts = list(value)?,
                // Frames are added and removed through their own requests.
                other => return Err(format!("unknown setting {other}")),
            }
            changed.push(key.clone());
        }
        changed.retain(|key| field_differs(self, &next, key));
        *self = next;
        Ok(changed)
    }
}

impl Settings {
    /// Add a frame by address, or update the name and password of one already
    /// listed. An empty password keeps the stored one; `-` clears it.
    pub fn add_frame(&mut self, host: &str, name: &str, password: &str) -> Result<(), String> {
        let host = host.trim();
        let valid = !host.is_empty()
            && host.len() <= 253
            && host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-:[]".contains(&byte));
        if !valid {
            return Err(format!(
                "frame address {host:?} must be an IP address or host name"
            ));
        }
        if name.len() > 64 || password.len() > 64 {
            return Err("frame name and password are at most 64 bytes".to_owned());
        }
        let entry = match self.frames.iter_mut().find(|frame| frame.host == host) {
            Some(entry) => entry,
            None => {
                if self.frames.len() >= MAX_LIST {
                    return Err(format!("at most {MAX_LIST} frames can be added by hand"));
                }
                self.frames.push(FrameEntry {
                    host: host.to_owned(),
                    ..FrameEntry::default()
                });
                self.frames.last_mut().expect("just pushed")
            }
        };
        entry.name = name.trim().to_owned();
        match password {
            "" => {}
            "-" => entry.password.clear(),
            _ => entry.password = password.to_owned(),
        }
        Ok(())
    }

    /// Forget a frame added by hand; `false` if it was not listed.
    pub fn remove_frame(&mut self, host: &str) -> bool {
        let before = self.frames.len();
        self.frames.retain(|frame| frame.host != host);
        self.frames.len() != before
    }
}

fn field_differs(before: &Settings, after: &Settings, key: &str) -> bool {
    let before = serde_json::to_value(before).unwrap_or_default();
    let after = serde_json::to_value(after).unwrap_or_default();
    before.get(key) != after.get(key)
}

fn valid_addr(addr: &str) -> Result<String, String> {
    addr.parse::<std::net::SocketAddr>()
        .map(|_| addr.to_owned())
        .map_err(|_| format!("addr {addr:?} must be ip:port"))
}

fn valid_backend(spec: &str, whisper: bool) -> Result<String, String> {
    let ok = spec == "stub"
        || (spec.starts_with("cmd:") && spec.len() > 4)
        || (whisper && spec.starts_with("whisper:") && spec.len() > 8);
    if ok {
        Ok(spec.to_owned())
    } else {
        Err(format!(
            "backend {spec:?} must be stub, cmd:<program>{}",
            if whisper { " or whisper:<size>" } else { "" }
        ))
    }
}

/// Wake stems as the matcher wants them: trimmed, lowercase, no empties.
pub fn normalize_wake(stems: &[String]) -> Vec<String> {
    stems
        .iter()
        .flat_map(|stem| stem.split(','))
        .map(str::trim)
        .filter(|stem| !stem.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The default settings file: `$XDG_CONFIG_HOME/easellet-bridge/bridge.toml`.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("easellet-bridge").join("bridge.toml")
}

#[cfg(unix)]
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(not(unix))]
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pinned(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|key| (*key).to_owned()).collect()
    }

    #[test]
    fn the_file_round_trips_and_unknown_keys_are_refused() {
        let mut settings = Settings::default();
        settings.wake = vec!["рамк".to_owned()];
        settings.llm_system = "Отвечай кратко.".to_owned();
        let text = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
        assert!(toml::from_str::<Settings>("colour = \"red\"\n").is_err());
        // A partial file keeps the defaults for the rest.
        let partial: Settings = toml::from_str("token = \"abcdefgh\"\n").unwrap();
        assert_eq!(partial.stt, "stub");
    }

    #[test]
    fn secrets_never_reach_the_browser() {
        let mut settings = Settings::default();
        settings.token = "easellet-lan".to_owned();
        let json = settings.to_json(&pinned(&[])).to_string();
        assert!(!json.contains("easellet-lan"), "{json}");
        assert!(json.contains("\"token\":true"), "{json}");
        assert!(json.contains("\"ui_password\":false"), "{json}");
    }

    #[test]
    fn a_change_applies_validates_and_respects_pins() {
        let mut settings = Settings::default();
        let changed = settings
            .apply(
                &serde_json::json!({"wake": "Рамк, frame", "llm_system": "Коротко."}),
                &pinned(&[]),
            )
            .unwrap();
        assert_eq!(changed, ["llm_system", "wake"]);
        assert_eq!(settings.live().wake, ["рамк", "frame"]);
        assert_eq!(
            settings.live().llm_env,
            [("EASEL_LLM_SYSTEM".to_owned(), "Коротко.".to_owned())]
        );
        // Pinned by a flag: refused, nothing changes.
        assert!(
            settings
                .apply(&serde_json::json!({"stt": "stub"}), &pinned(&["stt"]))
                .is_err()
        );
        for bad in [
            serde_json::json!({"addr": "nowhere"}),
            serde_json::json!({"stt": "sh -c reboot"}),
            serde_json::json!({"llm_url": "file:///etc/passwd"}),
            serde_json::json!({"token": "short"}),
            serde_json::json!({"colour": "red"}),
        ] {
            let before = settings.clone();
            assert!(settings.apply(&bad, &pinned(&[])).is_err(), "{bad}");
            assert_eq!(settings, before, "a refused change must change nothing");
        }
    }

    #[test]
    fn frames_are_added_kept_secret_and_removed() {
        let mut settings = Settings::default();
        settings
            .add_frame("192.168.50.204", "кухня", "framepass1")
            .unwrap();
        settings.add_frame("192.168.50.204", "кухня", "").unwrap();
        assert_eq!(settings.frames[0].password, "framepass1");
        assert!(settings.add_frame("evil; rm -rf /", "", "").is_err());
        let text = toml::to_string_pretty(&settings).unwrap();
        assert!(text.contains("[[frame]]"), "{text}");
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
        let json = settings.to_json(&BTreeSet::new()).to_string();
        assert!(
            !json.contains("framepass1") && json.contains("\"password\":true"),
            "{json}"
        );
        assert!(settings.remove_frame("192.168.50.204"));
        assert!(settings.frames.is_empty());
    }

    #[test]
    fn an_empty_secret_keeps_and_a_dash_clears() {
        let mut settings = Settings::default();
        settings
            .apply(
                &serde_json::json!({"ui_password": "hunter2hunter2"}),
                &pinned(&[]),
            )
            .unwrap();
        settings
            .apply(&serde_json::json!({"ui_password": ""}), &pinned(&[]))
            .unwrap();
        assert_eq!(settings.ui_password, "hunter2hunter2");
        settings
            .apply(&serde_json::json!({"ui_password": "-"}), &pinned(&[]))
            .unwrap();
        assert!(settings.live().ui_password.is_none());
    }
}
