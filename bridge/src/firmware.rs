//! The firmware store: what releases exist, and what each image hashes to.
//!
//! A release is a tag on the repository, and its assets are the images. The list
//! the bridge reads is `firmware-index.json`, one asset on every release, written
//! by `tools/firmware-index.py` in CI: one request answers "which versions exist,
//! what is each one's hash, which commit is it", and it is not an API call, so
//! restarting the bridge a few times costs nothing.
//!
//! - **Latest release**: `releases/latest/download/firmware-index.json`.
//! - **Dev build**: the moving pre-release, `releases/download/firmware-dev/…`,
//!   replaced on every green `main`.
//! - **Mirror**: `firmware_index_url`, for a home where GitHub is not reachable.
//!
//! The index is cached on disk with the time it was fetched, so a bridge that
//! cannot reach GitHub still says what it knew, and how old that is. Nothing here
//! writes to the repository: `firmware_repo` is read-only.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The schema this bridge understands. A newer one means a list whose meaning it
/// cannot guess at, so it is refused rather than read halfway.
pub const SCHEMA: u32 = 1;
/// Largest index accepted. A few kilobytes is the real size; anything much past
/// this is not one of ours.
const MAX_INDEX: usize = 512 * 1024;
/// Connect and whole-request timeouts. The index is a few kilobytes from a CDN;
/// downloading an image is a separate, longer call.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const INDEX_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a frame's own `ota` answer is reused.
pub const OTA_CACHE: Duration = Duration::from_secs(30);

/// One release in the list, as `tools/firmware-index.py` writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub date: String,
    pub image: Asset,
    #[serde(default)]
    pub slot_bytes: u64,
    #[serde(default)]
    pub idf: String,
    #[serde(default)]
    pub rust: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub notes: String,
}

/// The image of one release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
    #[serde(default)]
    pub url: String,
}

/// The whole document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub schema: u32,
    #[serde(default)]
    pub board: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub generated: String,
    #[serde(default)]
    pub releases: Vec<Release>,
}

/// Read an index, refusing one this bridge cannot interpret.
pub fn parse(text: &str) -> Result<Index, String> {
    let index: Index = serde_json::from_str(text)
        .map_err(|error| format!("the index is not the JSON this bridge expects: {error}"))?;
    if index.schema != SCHEMA {
        return Err(format!(
            "the index is schema {}, this bridge reads schema {SCHEMA}",
            index.schema
        ));
    }
    if index.releases.is_empty() {
        return Err("the index lists no releases".to_owned());
    }
    for release in &index.releases {
        if release.image.sha256.len() != 64
            || !release
                .image
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(format!(
                "{} has no usable sha256 ({})",
                release.version, release.image.sha256
            ));
        }
    }
    Ok(index)
}

/// Is this `owner/name`? The value becomes a URL path, so nothing else may pass.
pub fn valid_repo(repo: &str) -> bool {
    repo.split('/').count() == 2
        && repo.len() <= 140
        && repo
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-/".contains(&byte))
}

/// Where the index lives for one repository and channel.
pub fn index_url(repo: &str, channel: &str, mirror: &str) -> Result<String, String> {
    if !mirror.is_empty() {
        if !(mirror.starts_with("http://") || mirror.starts_with("https://")) {
            return Err("firmware_index_url must be an http(s) URL".to_owned());
        }
        return Ok(mirror.to_owned());
    }
    if repo.is_empty() {
        return Err("no firmware_repo is set".to_owned());
    }
    if !valid_repo(repo) {
        return Err(format!("firmware_repo {repo:?} is not owner/name"));
    }
    Ok(match channel {
        "dev" => {
            format!("https://github.com/{repo}/releases/download/firmware-dev/firmware-index.json")
        }
        _ => format!("https://github.com/{repo}/releases/latest/download/firmware-index.json"),
    })
}

/// How to fetch one URL. Injected so the store is testable without a network:
/// the tests hand it a map of URL to bytes.
pub type Fetch<'a> = &'a dyn Fn(&str) -> Result<Vec<u8>, String>;

fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// What the store knows right now.
#[derive(Debug, Default)]
pub struct Known {
    pub index: Option<Index>,
    /// When this bridge last fetched it, in Unix milliseconds.
    pub fetched_ms: Option<u64>,
    /// Why the last fetch failed, if it did: the cached list stays usable.
    pub error: Option<String>,
    /// A fetch is in flight; the UI shows that instead of starting another.
    pub refreshing: bool,
}

/// The release list, its cache, and the frame `ota` answers.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    known: Mutex<Known>,
    /// Frame id -> (when read, what the frame said).
    ota: Mutex<BTreeMap<String, (Instant, serde_json::Value)>>,
}

impl Store {
    /// A store that has read whatever the cache holds.
    pub fn new(dir: PathBuf) -> Self {
        let known = match read_cache(&dir) {
            Some((index, fetched_ms)) => Known {
                index: Some(index),
                fetched_ms: Some(fetched_ms),
                ..Known::default()
            },
            None => Known::default(),
        };
        Self {
            dir,
            known: Mutex::new(known),
            ota: Mutex::new(BTreeMap::new()),
        }
    }

    /// What the UI shows: the list, when it was fetched, and the last error.
    pub fn to_json(&self) -> serde_json::Value {
        let known = self.lock();
        serde_json::json!({
            "index": known.index,
            "fetched_ms": known.fetched_ms,
            "age_s": known.fetched_ms.map(|fetched| timestamp().saturating_sub(fetched) / 1000),
            "error": known.error,
            "refreshing": known.refreshing,
            "cache_dir": self.dir.display().to_string(),
        })
    }

    /// Fetch the list now, keeping the old one when the fetch fails.
    ///
    /// Two callers must not fetch at once: the second one asks the first, which
    /// is what `refreshing` is for.
    pub fn refresh(&self, url: &str, fetch: Fetch<'_>) -> Result<Index, String> {
        {
            let mut known = self.lock();
            if known.refreshing {
                return Err("a refresh is already running".to_owned());
            }
            known.refreshing = true;
        }
        let result = fetch(url).and_then(|bytes| {
            let text = String::from_utf8(bytes).map_err(|_| "the index is not text".to_owned())?;
            let index = parse(&text)?;
            write_cache(&self.dir, &text)?;
            Ok(index)
        });
        let mut known = self.lock();
        known.refreshing = false;
        match result {
            Ok(index) => {
                known.index = Some(index.clone());
                known.fetched_ms = Some(timestamp());
                known.error = None;
                Ok(index)
            }
            Err(error) => {
                known.error = Some(error.clone());
                Err(error)
            }
        }
    }

    /// Whether a fetch is due: never fetched, or older than `hours`. Zero hours
    /// means "only when a person asks".
    pub fn due(&self, hours: u64) -> bool {
        if hours == 0 {
            return false;
        }
        let known = self.lock();
        if known.refreshing {
            return false;
        }
        match known.fetched_ms {
            None => true,
            Some(fetched) => timestamp().saturating_sub(fetched) >= hours * 3_600_000,
        }
    }

    /// A frame's `ota` answer, read at most once per [`OTA_CACHE`].
    pub fn ota_cached(&self, frame: &str) -> Option<serde_json::Value> {
        let cache = self.ota.lock().ok()?;
        let (at, value) = cache.get(frame)?;
        (at.elapsed() < OTA_CACHE).then(|| value.clone())
    }

    pub fn remember_ota(&self, frame: &str, value: serde_json::Value) {
        if let Ok(mut cache) = self.ota.lock() {
            cache.insert(frame.to_owned(), (Instant::now(), value));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Known> {
        // A panic elsewhere must not make the release list unreadable.
        self.known
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Where the default cache lives: `$XDG_CACHE_HOME/easellet-bridge/firmware`.
pub fn default_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("easellet-bridge").join("firmware")
}

fn cache_file(dir: &Path) -> PathBuf {
    dir.join("index.json")
}

fn read_cache(dir: &Path) -> Option<(Index, u64)> {
    let text = std::fs::read_to_string(cache_file(dir)).ok()?;
    let index = parse(&text).ok()?;
    let fetched_ms = std::fs::metadata(cache_file(dir))
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    Some((index, fetched_ms))
}

fn write_cache(dir: &Path, text: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let temporary = dir.join("index.json.tmp");
    std::fs::write(&temporary, text)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, cache_file(dir))
        .map_err(|error| format!("{}: {error}", cache_file(dir).display()))
}

/// Fetch a URL over the network.
pub fn http_fetch(url: &str, token: &str) -> Result<Vec<u8>, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout(INDEX_TIMEOUT)
        .build();
    let mut request = agent
        .request("GET", url)
        .set("User-Agent", "easellet-bridge");
    if !token.is_empty() {
        request = request.set("Authorization", &format!("token {token}"));
    }
    let response = match request.call() {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(error) => return Err(format!("{url}: {error}")),
    };
    let status = response.status();
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_INDEX as u64)
        .read_to_end(&mut body)
        .map_err(|error| format!("{url}: {error}"))?;
    if status != 200 {
        let text = String::from_utf8_lossy(&body);
        let line = text.lines().next().unwrap_or("").trim().to_owned();
        return Err(format!(
            "{url}: HTTP {status}{}{line}",
            if line.is_empty() { "" } else { ": " }
        ));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{
        "schema": 1,
        "board": "easellet-spectra6-800x480",
        "channel": "stable",
        "generated": "2026-11-10T12:04:11Z",
        "releases": [
            {
                "version": "0.1.0",
                "tag": "v0.1.0",
                "date": "2026-11-03T09:00:00Z",
                "image": {
                    "name": "easellet-0.1.0.bin",
                    "bytes": 1170416,
                    "sha256": "f8836cea5ae071a962495829374ef003ced1d471103674a3a2bae940a76a8c83",
                    "url": "https://example.org/easellet-0.1.0.bin"
                },
                "slot_bytes": 3145728,
                "idf": "v5.5.5",
                "rust": "1.97.0.0",
                "commit": "72af2dd",
                "notes": "https://example.org/v0.1.0"
            }
        ]
    }"#;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("easellet-firmware-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_index_is_read_and_a_version_can_be_found() {
        let index = parse(GOOD).unwrap();
        assert_eq!(index.channel, "stable");
        assert_eq!(index.releases.len(), 1);
        let release = &index.releases[0];
        assert_eq!(release.version, "0.1.0");
        assert_eq!(release.slot_bytes, 3145728);
        assert_eq!(release.image.bytes, 1170416);
        assert_eq!(release.commit, "72af2dd");
    }

    #[test]
    fn an_index_this_bridge_cannot_read_is_refused() {
        let newer = GOOD.replace("\"schema\": 1", "\"schema\": 2");
        assert!(parse(&newer).unwrap_err().contains("schema 2"));
        let empty = r#"{"schema": 1, "releases": []}"#;
        assert!(parse(empty).unwrap_err().contains("no releases"));
        let bad_hash = GOOD.replace("f8836cea", "zzzz6cea");
        assert!(parse(&bad_hash).unwrap_err().contains("sha256"));
        assert!(parse("not json").is_err());
    }

    #[test]
    fn the_url_is_the_release_page_the_bridge_reads() {
        assert_eq!(
            index_url("abbra/easellet", "stable", "").unwrap(),
            "https://github.com/abbra/easellet/releases/latest/download/firmware-index.json"
        );
        assert_eq!(
            index_url("abbra/easellet", "dev", "").unwrap(),
            "https://github.com/abbra/easellet/releases/download/firmware-dev/firmware-index.json"
        );
        // A mirror replaces the whole URL, which is how a home without GitHub
        // reaches the same file.
        assert_eq!(
            index_url("", "stable", "http://nas.local/firmware-index.json").unwrap(),
            "http://nas.local/firmware-index.json"
        );
        assert!(
            index_url("", "stable", "")
                .unwrap_err()
                .contains("firmware_repo")
        );
        assert!(index_url("abbra", "stable", "").is_err());
        assert!(index_url("abbra/easellet/extra", "stable", "").is_err());
        assert!(index_url("a/b c", "stable", "").is_err());
        assert!(index_url("", "stable", "ftp://nas/x").is_err());
    }

    #[test]
    fn a_refresh_caches_the_list_and_a_failure_keeps_the_old_one() {
        let dir = scratch("refresh");
        let store = Store::new(dir.clone());
        assert!(store.to_json()["index"].is_null());

        let good = |_: &str| Ok(GOOD.as_bytes().to_vec());
        let index = store
            .refresh("https://example.org/index.json", &good)
            .unwrap();
        assert_eq!(index.releases.len(), 1);
        let json = store.to_json();
        assert!(json["fetched_ms"].as_u64().unwrap() > 0);
        assert!(json["error"].is_null());
        assert!(cache_file(&dir).exists());

        // The same failure that a home without GitHub sees: the list stays.
        let bad = |_: &str| Err("connection refused".to_owned());
        let error = store
            .refresh("https://example.org/index.json", &bad)
            .unwrap_err();
        assert!(error.contains("connection refused"));
        let json = store.to_json();
        assert_eq!(json["index"]["releases"][0]["version"], "0.1.0");
        assert!(
            json["error"]
                .as_str()
                .unwrap()
                .contains("connection refused")
        );

        // A restart reads the cache, so the list survives a restart too.
        let reopened = Store::new(dir.clone());
        let json = reopened.to_json();
        assert_eq!(json["index"]["releases"][0]["version"], "0.1.0");
        assert!(json["fetched_ms"].as_u64().unwrap() > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fetch_is_due_when_it_has_never_happened_or_has_gone_stale() {
        let dir = scratch("due");
        let store = Store::new(dir.clone());
        assert!(store.due(6), "nothing fetched yet is due");
        assert!(!store.due(0), "zero hours means only on demand");
        let good = |_: &str| Ok(GOOD.as_bytes().to_vec());
        store
            .refresh("https://example.org/index.json", &good)
            .unwrap();
        assert!(!store.due(6), "just fetched, not due");
        assert!(!store.due(1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
