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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The schema this bridge understands. A newer one means a list whose meaning it
/// cannot guess at, so it is refused rather than read halfway.
pub const SCHEMA: u32 = 1;
/// Largest index accepted. A few kilobytes is the real size; anything much past
/// this is not one of ours.
const MAX_INDEX: usize = 512 * 1024;
/// Largest image accepted: a 3 MB slot. Anything much past that is not one of
/// ours, and a wrong `Content-Length` must not turn into an unbounded read.
pub const MAX_IMAGE: u64 = 8 * 1024 * 1024;
/// How long an image download may take from a CDN on a home connection.
const IMAGE_TIMEOUT: Duration = Duration::from_secs(180);
/// How long the frame is given to come back with the new version before the
/// bridge says it cannot tell any more.
pub const COMEBACK: Duration = Duration::from_secs(180);
/// How often the frame is asked whether it is back.
const COMEBACK_STEP: Duration = Duration::from_secs(3);
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

impl Index {
    /// The release with this version, if the list has one.
    pub fn release(&self, version: &str) -> Option<&Release> {
        self.releases
            .iter()
            .find(|release| release.version == version)
    }
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
    /// Frame id -> the install running, or the last one that ran.
    installs: Mutex<BTreeMap<String, Install>>,
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
            installs: Mutex::new(BTreeMap::new()),
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

    /// The cached image of one release, if it is here and still the right size.
    ///
    /// The hash is checked when the file is written, and again before it is sent;
    /// a file whose size changed is deleted rather than used.
    pub fn cached_image(&self, release: &Release) -> Option<PathBuf> {
        let path = self.dir.join(format!("{}.bin", release.image.sha256));
        let size = std::fs::metadata(&path).ok()?.len();
        if size != release.image.bytes {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        Some(path)
    }

    /// The image of a release: from the cache when it is here, else downloaded.
    pub fn image(&self, release: &Release) -> Result<Vec<u8>, String> {
        if let Some(path) = self.cached_image(release) {
            return std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()));
        }
        let path = download(&release.image.url, release, &self.dir)?;
        std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))
    }

    /// The release list this bridge has, if it has one.
    pub fn index(&self) -> Option<Index> {
        self.lock().index.clone()
    }

    /// The install running on a frame, or the last one that ran.
    pub fn install(&self, frame: &str) -> Option<Install> {
        self.installs
            .lock()
            .map(|installs| installs.get(frame).cloned())
            .unwrap_or_default()
    }

    /// Is an install running on this frame?
    pub fn installing(&self, frame: &str) -> bool {
        self.install(frame).is_some_and(|install| {
            matches!(
                install.phase,
                Phase::Downloading | Phase::Writing | Phase::Restarting
            )
        })
    }

    /// Start an install on a frame the guards have already allowed.
    ///
    /// The work happens on its own thread: writing flash takes four minutes, and
    /// the page polls this same server while it runs.
    pub fn start(
        self: &Arc<Self>,
        frame: &str,
        release: Release,
        frame_io: Box<dyn Frame + Send>,
    ) -> Result<(), String> {
        {
            let mut installs = self
                .installs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(running) = installs.get(frame)
                && matches!(
                    running.phase,
                    Phase::Downloading | Phase::Writing | Phase::Restarting
                )
            {
                return Err(format!(
                    "{} is already being installed on this frame",
                    running.version
                ));
            }
            installs.insert(
                frame.to_owned(),
                Install {
                    frame: frame.to_owned(),
                    version: release.version.clone(),
                    phase: Phase::Downloading,
                    message: String::new(),
                    started_ms: timestamp(),
                },
            );
        }
        let store = Arc::clone(self);
        let frame_id = frame.to_owned();
        std::thread::spawn(move || {
            let mut frame_io = frame_io;
            run(
                &store,
                &frame_id,
                &release,
                &mut *frame_io,
                COMEBACK,
                COMEBACK_STEP,
            );
        });
        Ok(())
    }

    /// Record where an install has got to. An install that was started elsewhere
    /// (`run` is also called directly by the tests) is created here rather than
    /// dropped, so a missing entry can never swallow a failure.
    fn report(&self, frame: &str, version: &str, phase: Phase, message: impl Into<String>) {
        if let Ok(mut installs) = self.installs.lock() {
            let install = installs.entry(frame.to_owned()).or_insert_with(|| Install {
                frame: frame.to_owned(),
                version: version.to_owned(),
                phase,
                message: String::new(),
                started_ms: timestamp(),
            });
            install.phase = phase;
            install.message = message.into();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Known> {
        // A panic elsewhere must not make the release list unreadable.
        self.known
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// What an install is doing, in the order a person sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Downloading,
    Writing,
    Restarting,
    Done,
    Failed,
}

/// One frame's install, running or finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Install {
    pub frame: String,
    pub version: String,
    pub phase: Phase,
    /// What a person is told, in the frame's own words where there are any.
    pub message: String,
    /// Unix milliseconds.
    pub started_ms: u64,
}

/// What the installer needs from one frame, so its tests need no frame.
pub trait Frame {
    /// The frame's own `ota` answer: the running version and the slot size.
    fn ota(&self) -> Result<serde_json::Value, String>;
    /// Write an image and let the frame restart; returns the frame's sentence.
    fn write(&mut self, image: &[u8]) -> Result<String, String>;
}

/// The guards. Everything here is a reason not to write flash, and each one says
/// what a person has to change.
pub fn check(release: &Release, ota: &serde_json::Value) -> Result<(), String> {
    let running = ota
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if running == release.version {
        return Err(format!("{} is already running", release.version));
    }
    // The frame reports the size of the slot it actually has; the index says the
    // slot the image was built for. A frame running another partition table would
    // take the image and then fail to boot it, which is the one outcome worth
    // refusing twice.
    let slot = ota
        .get("max_bytes")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if slot > 0 && release.slot_bytes > slot {
        return Err(format!(
            "the release was built for a {} byte slot, this frame has {slot}; it runs another partition table",
            release.slot_bytes
        ));
    }
    if slot > 0 && release.image.bytes > slot {
        return Err(format!(
            "{} is {} bytes, the frame's slot is {slot}",
            release.image.name, release.image.bytes
        ));
    }
    if release.image.bytes > MAX_IMAGE {
        return Err(format!(
            "{} is {} bytes; this bridge accepts at most {MAX_IMAGE}",
            release.image.name, release.image.bytes
        ));
    }
    Ok(())
}

/// Download, write, wait for the frame to come back.
fn run(
    store: &Store,
    frame: &str,
    release: &Release,
    frame_io: &mut dyn Frame,
    comeback: Duration,
    step: Duration,
) {
    store.report(
        frame,
        &release.version,
        Phase::Downloading,
        format!("downloading {} bytes", release.image.bytes),
    );
    let image = match store.image(release) {
        Ok(image) => image,
        Err(error) => {
            store.report(
                frame,
                &release.version,
                Phase::Failed,
                format!("download failed: {error}"),
            );
            return;
        }
    };
    store.report(frame, &release.version, Phase::Writing, "writing flash");
    let sentence = match frame_io.write(&image) {
        Ok(sentence) => sentence,
        Err(error) => {
            store.report(
                frame,
                &release.version,
                Phase::Failed,
                format!("the frame refused it: {error}"),
            );
            return;
        }
    };
    // A reset in the middle of the answer is the normal ending: the frame restarts
    // as soon as it has written the slot, so the sentence may be all there is.
    let _ = sentence;

    store.report(
        frame,
        &release.version,
        Phase::Restarting,
        "the frame is restarting",
    );
    let deadline = Instant::now() + comeback;
    let mut seen = String::new();
    while Instant::now() < deadline {
        std::thread::sleep(step);
        let Ok(ota) = frame_io.ota() else {
            continue;
        };
        let running = ota
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if running == release.version {
            store.report(
                frame,
                &release.version,
                Phase::Done,
                format!("{} is running", release.version),
            );
            return;
        }
        seen = running;
    }
    // Honest about what the bridge cannot see: the frame may have rolled back, or
    // it may still be booting. Only the console can tell which.
    let why = if seen.is_empty() {
        format!("no answer after {} s", comeback.as_secs())
    } else {
        format!("it reports {seen}, so it rolled back")
    };
    store.report(
        frame,
        &release.version,
        Phase::Failed,
        format!(
            "not back after {} s ({why}); check `ota` on the console",
            comeback.as_secs()
        ),
    );
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

/// Download one image into the cache, checking size and hash before it is kept.
pub fn download(url: &str, release: &Release, dir: &Path) -> Result<PathBuf, String> {
    if release.image.bytes > MAX_IMAGE {
        return Err(format!(
            "{} is {} bytes; this bridge accepts at most {MAX_IMAGE}",
            release.image.name, release.image.bytes
        ));
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout(IMAGE_TIMEOUT)
        .build();
    let response = match agent.request("GET", url).call() {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(error) => return Err(format!("{url}: {error}")),
    };
    let status = response.status();
    if status != 200 {
        return Err(format!("{url}: HTTP {status}"));
    }
    let mut body = Vec::new();
    response
        .into_reader()
        .take(release.image.bytes + 1)
        .read_to_end(&mut body)
        .map_err(|error| format!("{url}: {error}"))?;
    if body.len() as u64 != release.image.bytes {
        return Err(format!(
            "{} is {} bytes, the index says {}",
            release.image.name,
            body.len(),
            release.image.bytes
        ));
    }
    let digest = sha256_hex(&body);
    if digest != release.image.sha256 {
        // Deleted, not written: a file that fails its hash is not a file this
        // bridge ever wants to find in its cache again.
        return Err(format!(
            "{} hashes to {digest}, the index says {}",
            release.image.name, release.image.sha256
        ));
    }
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let path = dir.join(format!("{}.bin", release.image.sha256));
    let temporary = dir.join(format!("{}.bin.tmp", release.image.sha256));
    std::fs::write(&temporary, &body)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, &path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path)
}

/// SHA-256 of some bytes, lowercase hex.
///
/// One hash of one file is the only cryptography this crate needs; the frame
/// verifies the image itself, so an attacker on the LAN is a different question
/// from a corrupted download.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
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
    fn a_hash_is_lowercase_hex_of_the_whole_file() {
        // The empty string's SHA-256 is worth pinning: a hash that silently
        // hashed nothing would still look like 64 hex digits.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// A release whose image is already in the cache, so no test needs a network.
    fn cached_release(dir: &Path, version: &str, bytes: &[u8]) -> Release {
        let mut release = parse(GOOD).unwrap().releases.remove(0);
        release.version = version.to_owned();
        release.image.bytes = bytes.len() as u64;
        release.image.sha256 = sha256_hex(bytes);
        std::fs::write(dir.join(format!("{}.bin", release.image.sha256)), bytes).unwrap();
        release
    }

    fn ota(version: &str) -> serde_json::Value {
        serde_json::json!({
            "version": version, "running": "ota_0", "state": "valid",
            "other": "ota_1", "other_version": version, "max_bytes": 3145728,
        })
    }

    struct Fake {
        /// What `ota` answers, in order; the last one repeats.
        answers: Vec<serde_json::Value>,
        /// What `write` answers.
        write: Result<String, String>,
        written: usize,
    }

    impl Fake {
        fn new(answers: Vec<serde_json::Value>) -> Self {
            Self {
                answers,
                write: Ok("OK firmware 4 bytes written to ota_1; rebooting".to_owned()),
                written: 0,
            }
        }
    }

    impl Frame for Fake {
        fn ota(&self) -> Result<serde_json::Value, String> {
            let index = self.written.min(self.answers.len().saturating_sub(1));
            Ok(self.answers[index].clone())
        }

        fn write(&mut self, image: &[u8]) -> Result<String, String> {
            self.written += 1;
            assert_eq!(image, b"image");
            self.write.clone()
        }
    }

    #[test]
    fn the_guards_refuse_a_frame_that_cannot_take_this_release() {
        let dir = scratch("guards");
        let release = cached_release(&dir, "0.2.0", b"image");
        assert!(check(&release, &ota("0.1.0")).is_ok());
        assert!(
            check(&release, &ota("0.2.0"))
                .unwrap_err()
                .contains("already running")
        );
        // A frame whose slot is smaller than the release was built for runs
        // another partition table, and would take the image then fail to boot it.
        let mut small = ota("0.1.0");
        small["max_bytes"] = serde_json::json!(2097152);
        assert!(
            check(&release, &small)
                .unwrap_err()
                .contains("partition table")
        );
        // An image larger than the slot it is going into: the release was built
        // for this frame's slot, and the image is still bigger than it.
        let mut tiny = ota("0.1.0");
        tiny["max_bytes"] = serde_json::json!(3);
        let mut small_slot = release.clone();
        small_slot.slot_bytes = 3;
        assert!(
            check(&small_slot, &tiny)
                .unwrap_err()
                .contains("the frame's slot is 3")
        );
        // A frame that does not say how large its slot is gets no size check.
        let mut quiet = ota("0.1.0");
        quiet["max_bytes"] = serde_json::json!(0);
        assert!(check(&release, &quiet).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_install_climbs_through_its_phases_to_done() {
        let dir = scratch("install");
        let store = Store::new(dir.clone());
        let release = cached_release(&dir, "0.2.0", b"image");
        let mut frame = Fake::new(vec![ota("0.1.0"), ota("0.2.0")]);
        run(
            &store,
            "f",
            &release,
            &mut frame,
            Duration::from_secs(2),
            Duration::from_millis(5),
        );
        let install = store.install("f").unwrap();
        assert_eq!(install.phase, Phase::Done, "{}", install.message);
        assert_eq!(install.message, "0.2.0 is running");
        assert_eq!(frame.written, 1);
        assert!(!store.installing("f"), "a finished install is not running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rollback_is_reported_with_the_version_the_frame_says() {
        let dir = scratch("rollback");
        let store = Store::new(dir.clone());
        let release = cached_release(&dir, "0.2.0", b"image");
        // It answers "0.1.0" throughout: the bootloader put the old slot back.
        let mut frame = Fake::new(vec![ota("0.1.0")]);
        run(
            &store,
            "f",
            &release,
            &mut frame,
            Duration::from_millis(60),
            Duration::from_millis(10),
        );
        let install = store.install("f").unwrap();
        assert_eq!(install.phase, Phase::Failed);
        assert!(
            install.message.contains("it reports 0.1.0"),
            "{}",
            install.message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_frame_that_refuses_the_image_says_why() {
        let dir = scratch("refused");
        let store = Store::new(dir.clone());
        let release = cached_release(&dir, "0.2.0", b"image");
        let mut frame = Fake::new(vec![ota("0.1.0")]);
        frame.write = Err("HTTP 413 firmware too large".to_owned());
        run(
            &store,
            "f",
            &release,
            &mut frame,
            Duration::from_millis(50),
            Duration::from_millis(5),
        );
        let install = store.install("f").unwrap();
        assert_eq!(install.phase, Phase::Failed);
        assert!(install.message.contains("413 firmware too large"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_image_that_is_not_cached_and_cannot_be_reached_fails_before_the_flash() {
        let dir = scratch("download");
        let store = Store::new(dir.clone());
        let mut release = parse(GOOD).unwrap().releases.remove(0);
        release.image.url = "http://127.0.0.1:1/easellet.bin".to_owned();
        release.image.bytes = 5;
        let mut frame = Fake::new(vec![ota("0.1.0")]);
        run(
            &store,
            "f",
            &release,
            &mut frame,
            Duration::from_millis(50),
            Duration::from_millis(5),
        );
        let install = store.install("f").unwrap();
        assert_eq!(install.phase, Phase::Failed);
        assert!(install.message.starts_with("download failed:"));
        assert_eq!(
            frame.written, 0,
            "nothing is written when nothing was downloaded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_install_on_the_same_frame_is_refused_while_the_first_runs() {
        let dir = scratch("twice");
        let store = Arc::new(Store::new(dir.clone()));
        let release = cached_release(&dir, "0.2.0", b"image");
        // The frame never comes back, so the first install is still running.
        let frame = Box::new(SlowFrame::default());
        store.start("f", release.clone(), frame).unwrap();
        let error = store
            .start("f", release, Box::new(SlowFrame::default()))
            .unwrap_err();
        assert!(error.contains("already being installed"), "{error}");
        // Another frame is a different install and is allowed.
        assert!(
            store
                .start(
                    "g",
                    cached_release(&dir, "0.3.0", b"image"),
                    Box::new(SlowFrame::default())
                )
                .is_ok()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A frame that writes the image and is never heard from again.
    #[derive(Default)]
    struct SlowFrame;

    impl Frame for SlowFrame {
        fn ota(&self) -> Result<serde_json::Value, String> {
            Err("connection refused".to_owned())
        }
        fn write(&mut self, _image: &[u8]) -> Result<String, String> {
            Ok(String::new())
        }
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
