//! Jobs: a plugin, its saved parameters, the frames it is for, and how often.
//!
//! The scheduler runs due jobs, renders each result for each target frame, and
//! keeps the latest picture per frame and job in an outbox. Delivery is whichever
//! way the frame can take it (`research/bridge-ui-design.md` section 3.3):
//!
//! - **push** to a frame that is awake (OpenDisplay `DirectWrite`, or the portal
//!   for `gallery` jobs);
//! - **pull** by a sleeping frame on its timed wake: `GET /frames/<id>/next`
//!   with the ETag of what it shows, answered 304 when nothing changed.
//!
//! A picture identical to the one a frame already shows is never sent again: the
//! panel takes ~30 s and real charge to refresh, and an unchanged CI board
//! should cost nothing.
//!
//! Jobs live in `jobs.toml` next to `bridge.toml`, owner-only, because a job's
//! parameters may hold secrets (an API token for the CI plugin). Secret values
//! are never sent back to the browser.

use std::collections::{BTreeMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::frames;
use crate::plugins::{self, ParamKind};
use crate::ui::Context;

/// Longest interval a job may have: a week.
const MAX_EVERY_MIN: u32 = 7 * 24 * 60;
/// Shortest automatic interval: a refresh is ~30 s and costs charge.
const MIN_EVERY_MIN: u32 = 5;
/// Runs kept for the log.
const RUN_LOG: usize = 200;
/// Most jobs.
const MAX_JOBS: usize = 64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Replace what the frame shows; nothing is stored on the frame.
    #[default]
    Show,
    /// Store the picture in the frame's slideshow directory.
    Gallery,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub id: String,
    pub plugin: String,
    /// What the UI calls it; empty = the plugin's title.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    /// Frame ids.
    pub frames: Vec<String>,
    /// Minutes between runs; 0 = only when asked.
    #[serde(default)]
    pub every_min: u32,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// When several jobs target one frame, the higher priority's picture wins.
    #[serde(default)]
    pub priority: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobFile {
    #[serde(default, rename = "job")]
    pub jobs: Vec<Job>,
}

/// One picture waiting for (or already shown on) a frame.
#[derive(Debug, Clone)]
pub struct Render {
    pub image: Arc<Vec<u8>>,
    /// Content hash; also the ETag.
    pub hash: u64,
    pub priority: u8,
    pub at_ms: u64,
    pub job: String,
}

/// One entry of the run log.
#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub at_ms: u64,
    pub job: String,
    pub plugin: String,
    pub frame: String,
    /// `shown`, `queued`, `stored`, `unchanged`, `nothing new`, `failed`.
    pub result: String,
    pub message: String,
    pub ms: u64,
}

pub struct Scheduler {
    path: PathBuf,
    jobs: RwLock<JobFile>,
    last_run: Mutex<BTreeMap<String, Instant>>,
    /// frame -> job -> latest picture.
    outbox: Mutex<BTreeMap<String, BTreeMap<String, Render>>>,
    /// frame -> hash of what it shows, as far as the bridge knows.
    shown: Mutex<BTreeMap<String, u64>>,
    runs: Mutex<VecDeque<Run>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// A stable hash of a picture (FNV-1a: no dependency, stable across runs and
/// builds, which `DefaultHasher` does not promise).
pub fn content_hash(image: &[u8]) -> u64 {
    struct Fnv(u64);
    impl Hasher for Fnv {
        fn finish(&self) -> u64 {
            self.0
        }
        fn write(&mut self, bytes: &[u8]) {
            for byte in bytes {
                self.0 ^= u64::from(*byte);
                self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
    let mut hasher = Fnv(0xcbf2_9ce4_8422_2325);
    image.hash(&mut hasher);
    hasher.finish()
}

impl Scheduler {
    pub fn load(path: &Path) -> Result<Self, String> {
        let jobs = match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => JobFile::default(),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        Ok(Self {
            path: path.to_path_buf(),
            jobs: RwLock::new(jobs),
            last_run: Mutex::new(BTreeMap::new()),
            outbox: Mutex::new(BTreeMap::new()),
            shown: Mutex::new(BTreeMap::new()),
            runs: Mutex::new(VecDeque::new()),
        })
    }

    fn save(&self, jobs: &JobFile) -> Result<(), String> {
        let text = toml::to_string_pretty(jobs).map_err(|error| error.to_string())?;
        let text = format!(
            "# photoframe-bridge jobs; edited by the web UI. Owner-only: may hold secrets.\n\n{text}"
        );
        if let Some(directory) = self.path.parent() {
            std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
        }
        let temporary = self.path.with_extension("toml.tmp");
        crate::settings::write_private(&temporary, text.as_bytes())?;
        std::fs::rename(&temporary, &self.path).map_err(|error| error.to_string())
    }

    /// The jobs as the browser sees them: secret parameters become "is set".
    pub fn to_json(&self, plugins: &[plugins::Plugin]) -> serde_json::Value {
        let jobs = self
            .jobs
            .read()
            .map(|jobs| jobs.jobs.clone())
            .unwrap_or_default();
        let list: Vec<serde_json::Value> = jobs
            .iter()
            .map(|job| {
                let mut value = serde_json::to_value(job).unwrap_or_default();
                if let Some(plugin) = plugins
                    .iter()
                    .find(|plugin| plugin.manifest.name == job.plugin)
                {
                    for param in &plugin.manifest.params {
                        if param.kind == ParamKind::Secret
                            && let Some(slot) = value["params"].get_mut(&param.id)
                        {
                            *slot = serde_json::Value::Bool(true);
                        }
                    }
                }
                value
            })
            .collect();
        let runs: Vec<Run> = self
            .runs
            .lock()
            .map(|runs| runs.iter().rev().take(50).cloned().collect())
            .unwrap_or_default();
        serde_json::json!({"jobs": list, "runs": runs})
    }

    /// Add or replace a job from the UI. Parameters are checked against the
    /// plugin's manifest; a secret left empty keeps the stored value.
    pub fn save_job(
        &self,
        request: &serde_json::Value,
        plugins: &[plugins::Plugin],
    ) -> Result<String, String> {
        let text = |key: &str| {
            request
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        let plugin_name = text("plugin");
        let plugin = plugins
            .iter()
            .find(|plugin| plugin.manifest.name == plugin_name)
            .ok_or_else(|| format!("no plugin {plugin_name}"))?;
        let mut jobs = self
            .jobs
            .write()
            .map_err(|_| "jobs unavailable".to_owned())?;
        let id = Some(text("id"))
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| format!("{plugin_name}-{:x}", now_ms()));
        if !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || id.len() > 64
        {
            return Err("job id must be [A-Za-z0-9-]".to_owned());
        }
        let existing = jobs.jobs.iter().find(|job| job.id == id).cloned();
        let mut given = request
            .get("params")
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        for param in &plugin.manifest.params {
            let empty = given.get(&param.id).is_none_or(|value| {
                value.as_str() == Some("") || value.is_boolean() && param.kind == ParamKind::Secret
            });
            if param.kind == ParamKind::Secret
                && empty
                && let Some(old) = existing.as_ref().and_then(|job| job.params.get(&param.id))
            {
                given.insert(param.id.clone(), old.clone());
            }
        }
        let params = plugins::validate(&plugin.manifest, &given)?;
        let frames: Vec<String> = request
            .get("frames")
            .and_then(serde_json::Value::as_array)
            .map(|frames| {
                frames
                    .iter()
                    .filter_map(|frame| frame.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        if frames.is_empty() {
            return Err("choose at least one frame".to_owned());
        }
        let every_min = request
            .get("every_min")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        if every_min != 0 && !(MIN_EVERY_MIN..=MAX_EVERY_MIN).contains(&every_min) {
            return Err(format!(
                "every_min must be 0 or {MIN_EVERY_MIN}..={MAX_EVERY_MIN}"
            ));
        }
        let mode = match text("mode").as_str() {
            "" | "show" => Mode::Show,
            "gallery" => Mode::Gallery,
            other => return Err(format!("mode {other} must be show or gallery")),
        };
        let job = Job {
            id: id.clone(),
            plugin: plugin_name,
            title: text("title").chars().take(64).collect(),
            params,
            frames,
            every_min,
            mode,
            enabled: request
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            priority: request
                .get("priority")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
                .min(9) as u8,
        };
        let mut next = jobs.clone();
        if let Some(old) = next.jobs.iter_mut().find(|old| old.id == id) {
            *old = job;
        } else if next.jobs.len() >= MAX_JOBS {
            return Err(format!("at most {MAX_JOBS} jobs"));
        } else {
            next.jobs.push(job);
        }
        self.save(&next)?;
        *jobs = next;
        // A changed job runs at the next tick.
        if let Ok(mut last) = self.last_run.lock() {
            last.remove(&id);
        }
        Ok(id)
    }

    /// The parameters of a saved job for this plugin and frame, secrets
    /// included, so the assistant runs a plugin the way its owner configured it.
    pub fn params_for(
        &self,
        plugin: &str,
        frame: &str,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        let jobs = self.jobs.read().ok()?;
        jobs.jobs
            .iter()
            .find(|job| job.plugin == plugin && job.frames.iter().any(|id| id == frame))
            .or_else(|| jobs.jobs.iter().find(|job| job.plugin == plugin))
            .map(|job| job.params.clone())
    }

    pub fn delete_job(&self, id: &str) -> Result<(), String> {
        let mut jobs = self
            .jobs
            .write()
            .map_err(|_| "jobs unavailable".to_owned())?;
        let mut next = jobs.clone();
        let before = next.jobs.len();
        next.jobs.retain(|job| job.id != id);
        if next.jobs.len() == before {
            return Err(format!("no job {id}"));
        }
        self.save(&next)?;
        *jobs = next;
        if let Ok(mut outbox) = self.outbox.lock() {
            for renders in outbox.values_mut() {
                renders.remove(id);
            }
        }
        Ok(())
    }

    /// Run every enabled job whose interval has passed.
    pub fn tick(&self, context: &Context) {
        let due: Vec<Job> = {
            let jobs = self
                .jobs
                .read()
                .map(|jobs| jobs.jobs.clone())
                .unwrap_or_default();
            let last = self
                .last_run
                .lock()
                .map(|last| last.clone())
                .unwrap_or_default();
            jobs.into_iter()
                .filter(|job| {
                    job.enabled
                        && job.every_min > 0
                        && last.get(&job.id).is_none_or(|at| {
                            at.elapsed() >= Duration::from_secs(u64::from(job.every_min) * 60)
                        })
                })
                .collect()
        };
        for job in due {
            self.run(&job, context);
        }
    }

    /// Run one job now, by id.
    pub fn run_now(&self, id: &str, context: &Context) -> Result<Vec<Run>, String> {
        let job = self
            .jobs
            .read()
            .ok()
            .and_then(|jobs| jobs.jobs.iter().find(|job| job.id == id).cloned())
            .ok_or_else(|| format!("no job {id}"))?;
        Ok(self.run(&job, context))
    }

    fn run(&self, job: &Job, context: &Context) -> Vec<Run> {
        if let Ok(mut last) = self.last_run.lock() {
            last.insert(job.id.clone(), Instant::now());
        }
        let (found, _) = plugins::discover(&context.plugin_dirs);
        let known = context.frames.list();
        let mut runs = Vec::new();
        for frame_id in &job.frames {
            let started = Instant::now();
            let frame = known.iter().find(|frame| &frame.id == frame_id);
            let mut log = |result: &str, message: String| {
                runs.push(Run {
                    at_ms: now_ms(),
                    job: job.id.clone(),
                    plugin: job.plugin.clone(),
                    frame: frame.map_or_else(|| frame_id.clone(), |frame| frame.name.clone()),
                    result: result.to_owned(),
                    message,
                    ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                });
            };
            let Some(plugin) = found
                .iter()
                .find(|plugin| plugin.manifest.name == job.plugin)
            else {
                log("failed", format!("plugin {} is not installed", job.plugin));
                continue;
            };
            let target = plugins::Target::spectra(
                frame_id,
                frame.map_or(frame_id.as_str(), |frame| frame.name.as_str()),
            );
            let key = (job.plugin.clone(), frame_id.clone());
            let state = context
                .plugin_state
                .lock()
                .ok()
                .and_then(|states| states.get(&key).cloned());
            let outcome = match plugins::run(plugin, &job.params, &target, state.as_ref()) {
                Ok(outcome) => outcome,
                Err(error) => {
                    log("failed", error);
                    continue;
                }
            };
            if let (Some(state), Ok(mut states)) =
                (outcome.state.clone(), context.plugin_state.lock())
            {
                states.insert(key, state);
            }
            let image = match plugins::frame_for(&outcome, context.fonts.as_deref()) {
                Ok(Some(image)) => image,
                Ok(None) => {
                    log("nothing new", String::new());
                    continue;
                }
                Err(error) => {
                    log("failed", error);
                    continue;
                }
            };
            let hash = content_hash(&image);
            let (result, message) = self.deliver(job, frame_id, frame, image, hash);
            log(&result, message);
        }
        if let Ok(mut log) = self.runs.lock() {
            for run in &runs {
                if log.len() == RUN_LOG {
                    log.pop_front();
                }
                log.push_back(run.clone());
            }
        }
        runs
    }

    /// Queue the picture for the frame and push it if the frame is awake.
    fn deliver(
        &self,
        job: &Job,
        frame_id: &str,
        frame: Option<&frames::Frame>,
        image: Vec<u8>,
        hash: u64,
    ) -> (String, String) {
        if job.mode == Mode::Gallery {
            let Some(frame) = frame.filter(|frame| frame.portal) else {
                return (
                    "failed".to_owned(),
                    "storing needs the frame's portal, and it is not answering".to_owned(),
                );
            };
            let password = frame.portal_password().to_owned();
            return match frames::portal(&frame.host, &password, "POST", "/upload", &image) {
                Ok(answer) if answer.status == 200 => (
                    "stored".to_owned(),
                    String::from_utf8_lossy(&answer.body).trim().to_owned(),
                ),
                Ok(answer) => (
                    "failed".to_owned(),
                    String::from_utf8_lossy(&answer.body).trim().to_owned(),
                ),
                Err(error) => ("failed".to_owned(), error),
            };
        }
        let image = Arc::new(image);
        if let Ok(mut outbox) = self.outbox.lock() {
            outbox.entry(frame_id.to_owned()).or_default().insert(
                job.id.clone(),
                Render {
                    image: Arc::clone(&image),
                    hash,
                    priority: job.priority,
                    at_ms: now_ms(),
                    job: job.id.clone(),
                },
            );
        }
        // What the frame should show now: the best queued picture, not
        // necessarily this one (a higher-priority job may own the frame).
        let Some(best) = self.next(frame_id) else {
            return ("queued".to_owned(), String::new());
        };
        if self
            .shown
            .lock()
            .ok()
            .and_then(|shown| shown.get(frame_id).copied())
            == Some(best.hash)
        {
            return (
                "unchanged".to_owned(),
                "the frame already shows this".to_owned(),
            );
        }
        let Some(frame) = frame.filter(|frame| frame.online && frame.panel.is_some()) else {
            return (
                "queued".to_owned(),
                "the frame is asleep; it fetches this on its next wake".to_owned(),
            );
        };
        match frames::display(&frame.host, frame.port, &best.image) {
            Ok(message) => {
                self.mark_shown(frame_id, best.hash);
                ("shown".to_owned(), message)
            }
            Err(error) => (
                "queued".to_owned(),
                format!("push failed ({error}); queued for the frame to fetch"),
            ),
        }
    }

    /// The picture a frame should show: highest priority, then newest.
    pub fn next(&self, frame_id: &str) -> Option<Render> {
        let outbox = self.outbox.lock().ok()?;
        outbox
            .get(frame_id)?
            .values()
            .max_by_key(|render| (render.priority, render.at_ms))
            .cloned()
    }

    #[cfg(test)]
    pub fn queue_for_test(&self, frame_id: &str, image: Vec<u8>, hash: u64) {
        let mut outbox = self.outbox.lock().unwrap();
        outbox.entry(frame_id.to_owned()).or_default().insert(
            "test".to_owned(),
            Render {
                image: Arc::new(image),
                hash,
                priority: 0,
                at_ms: now_ms(),
                job: "test".to_owned(),
            },
        );
    }

    pub fn mark_shown(&self, frame_id: &str, hash: u64) {
        if let Ok(mut shown) = self.shown.lock() {
            shown.insert(frame_id.to_owned(), hash);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_stable_and_content_sensitive() {
        assert_eq!(content_hash(b"abc"), content_hash(b"abc"));
        assert_ne!(content_hash(b"abc"), content_hash(b"abd"));
        // Pinned: an ETag must survive a bridge restart and a rebuild.
        assert_eq!(content_hash(b""), content_hash(&[]));
    }

    #[test]
    fn the_best_render_wins_by_priority_then_age() {
        let scheduler = Scheduler::load(Path::new("/nonexistent/jobs.toml")).unwrap();
        let render = |job: &str, priority, at_ms| Render {
            image: Arc::new(vec![0]),
            hash: 1,
            priority,
            at_ms,
            job: job.to_owned(),
        };
        {
            let mut outbox = scheduler.outbox.lock().unwrap();
            let renders = outbox.entry("f".to_owned()).or_default();
            renders.insert("old-urgent".to_owned(), render("old-urgent", 5, 10));
            renders.insert("new".to_owned(), render("new", 0, 20));
        }
        assert_eq!(scheduler.next("f").unwrap().job, "old-urgent");
        assert!(scheduler.next("other").is_none());
    }

    #[test]
    fn the_job_file_round_trips() {
        let file = JobFile {
            jobs: vec![Job {
                id: "ci-1".to_owned(),
                plugin: "ci".to_owned(),
                title: String::new(),
                params: serde_json::json!({"repo": "x/y", "n": 3})
                    .as_object()
                    .unwrap()
                    .clone(),
                frames: vec!["94a990caed46".to_owned()],
                every_min: 15,
                mode: Mode::Show,
                enabled: true,
                priority: 1,
            }],
        };
        let text = toml::to_string_pretty(&file).unwrap();
        assert!(text.contains("[[job]]"), "{text}");
        assert_eq!(toml::from_str::<JobFile>(&text).unwrap(), file);
    }
}
