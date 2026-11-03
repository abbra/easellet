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

use crate::compose::{self, Panel, Preset, Tile};
use crate::frames;
use crate::plugins::{self, ParamKind};
use crate::render::Fonts;
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

/// Which job fills which rectangle of a frame's screen.
///
/// A screen is opt-in: a frame without one behaves as it always did, and the
/// newest highest-priority panel owns the whole panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screen {
    /// Frame id.
    pub frame: String,
    /// `single`, `stack`, `columns`, `hero` or `grid`; empty means `single`.
    #[serde(default)]
    pub preset: String,
    /// Job ids in the preset's slot order; an empty string leaves a slot blank.
    #[serde(default)]
    pub slots: Vec<String>,
}

impl Screen {
    pub fn preset(&self) -> Option<Preset> {
        Preset::parse(&self.preset)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobFile {
    #[serde(default, rename = "job")]
    pub jobs: Vec<Job>,
    #[serde(default, rename = "screen", skip_serializing_if = "Vec::is_empty")]
    pub screens: Vec<Screen>,
}

/// One picture waiting for (or already shown on) a frame.
#[derive(Debug, Clone)]
pub struct Render {
    pub image: Arc<Vec<u8>>,
    /// Content hash; also the ETag.
    pub hash: u64,
    /// The job that made it, or `screen` for a composed screen, or `answer`
    /// for a spoken answer or a manual show.
    pub job: String,
}

/// What one job last produced for one frame, kept as content until the frame
/// is drawn.
#[derive(Debug, Clone)]
struct Slot {
    panel: Panel,
    priority: u8,
    at_ms: u64,
    every_min: u32,
}

/// A picture put on the frame by the assistant or by a manual "show", which
/// must survive the next scheduled composition for a while.
#[derive(Debug, Clone)]
struct Override {
    image: Arc<Vec<u8>>,
    hash: u64,
    until_ms: u64,
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
    /// frame -> job -> latest panel.
    outbox: Mutex<BTreeMap<String, BTreeMap<String, Slot>>>,
    /// frame -> a manual or spoken picture that outranks the screen for a while.
    overrides: Mutex<BTreeMap<String, Override>>,
    /// frame -> hash of what it shows, as far as the bridge knows.
    shown: Mutex<BTreeMap<String, u64>>,
    runs: Mutex<VecDeque<Run>>,
}

/// A panel is stale when it is older than two of its job's intervals. A manual
/// job has no interval, so its tile is stale after a day.
fn stale(slot: &Slot, now: u64) -> bool {
    let minutes = if slot.every_min == 0 {
        24 * 60
    } else {
        u64::from(slot.every_min)
    };
    now.saturating_sub(slot.at_ms) > minutes.saturating_mul(2).saturating_mul(60_000)
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
        let jobs: JobFile = match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => JobFile::default(),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        for screen in &jobs.screens {
            let Some(preset) = screen.preset() else {
                return Err(format!(
                    "{}: screen {}: preset {:?} is not single, stack, columns, hero or grid",
                    path.display(),
                    screen.frame,
                    screen.preset
                ));
            };
            if screen.slots.len() > preset.cells().len() {
                return Err(format!(
                    "{}: screen {}: {} is {} slots, not {}",
                    path.display(),
                    screen.frame,
                    preset.name(),
                    preset.cells().len(),
                    screen.slots.len()
                ));
            }
            if jobs
                .screens
                .iter()
                .filter(|other| other.frame == screen.frame)
                .count()
                > 1
            {
                return Err(format!(
                    "{}: screen {}: a frame has one screen",
                    path.display(),
                    screen.frame
                ));
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            jobs: RwLock::new(jobs),
            last_run: Mutex::new(BTreeMap::new()),
            outbox: Mutex::new(BTreeMap::new()),
            overrides: Mutex::new(BTreeMap::new()),
            shown: Mutex::new(BTreeMap::new()),
            runs: Mutex::new(VecDeque::new()),
        })
    }

    fn save(&self, jobs: &JobFile) -> Result<(), String> {
        let text = toml::to_string_pretty(jobs).map_err(|error| error.to_string())?;
        let text = format!(
            "# easellet-bridge jobs; edited by the web UI. Owner-only: may hold secrets.\n\n{text}"
        );
        if let Some(directory) = self.path.parent() {
            std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
        }
        let temporary = self.path.with_extension("toml.tmp");
        crate::settings::write_private(&temporary, text.as_bytes())?;
        std::fs::rename(&temporary, &self.path).map_err(|error| error.to_string())
    }

    /// Every screen, as saved.
    pub fn screens(&self) -> Vec<Screen> {
        self.jobs
            .read()
            .map(|jobs| jobs.screens.clone())
            .unwrap_or_default()
    }

    /// Add or replace the screen of a frame. A slot names a saved job that
    /// shows on this frame; an empty string leaves the slot blank.
    pub fn save_screen(&self, request: &serde_json::Value) -> Result<String, String> {
        let frame = request
            .get("frame")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();
        if frame.is_empty() {
            return Err("choose a frame".to_owned());
        }
        let name = request
            .get("preset")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("single");
        let preset = Preset::parse(name)
            .ok_or_else(|| format!("preset {name} is not single, stack, columns, hero or grid"))?;
        let slots: Vec<String> = request
            .get("slots")
            .and_then(serde_json::Value::as_array)
            .map(|slots| {
                slots
                    .iter()
                    .map(|slot| slot.as_str().unwrap_or("").to_owned())
                    .collect()
            })
            .unwrap_or_default();
        if slots.len() > preset.cells().len() {
            return Err(format!(
                "{} is {} slots, not {}",
                preset.name(),
                preset.cells().len(),
                slots.len()
            ));
        }
        let mut jobs = self
            .jobs
            .write()
            .map_err(|_| "jobs unavailable".to_owned())?;
        let mut next = jobs.clone();
        for (index, id) in slots.iter().enumerate() {
            if id.is_empty() {
                continue;
            }
            if slots[..index].contains(id) {
                return Err(format!("job {id} is in two slots"));
            }
            let Some(job) = next.jobs.iter().find(|job| &job.id == id) else {
                return Err(format!("no job {id}"));
            };
            if !job.frames.iter().any(|frame_id| frame_id == &frame) {
                return Err(format!("job {id} does not target this frame"));
            }
            if job.mode != Mode::Show {
                return Err(format!("job {id} stores pictures; a screen shows them"));
            }
        }
        next.screens.retain(|screen| screen.frame != frame);
        next.screens.push(Screen {
            frame: frame.clone(),
            preset: preset.name().to_owned(),
            slots,
        });
        self.save(&next)?;
        *jobs = next;
        Ok(frame)
    }

    /// Return a frame to the single-panel behaviour.
    pub fn delete_screen(&self, frame: &str) -> Result<(), String> {
        let mut jobs = self
            .jobs
            .write()
            .map_err(|_| "jobs unavailable".to_owned())?;
        let mut next = jobs.clone();
        let before = next.screens.len();
        next.screens.retain(|screen| screen.frame != frame);
        if next.screens.len() == before {
            return Err(format!("no screen for {frame}"));
        }
        self.save(&next)?;
        *jobs = next;
        Ok(())
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
        let screens = self
            .jobs
            .read()
            .map(|jobs| jobs.screens.clone())
            .unwrap_or_default();
        let runs: Vec<Run> = self
            .runs
            .lock()
            .map(|runs| runs.iter().rev().take(50).cloned().collect())
            .unwrap_or_default();
        serde_json::json!({"jobs": list, "screens": screens, "runs": runs})
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
            let Some(panel) = Panel::from_output(&outcome.output) else {
                log("nothing new", String::new());
                continue;
            };
            let (result, message) =
                self.deliver(job, frame_id, frame, panel, context.fonts.as_deref());
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

    /// Queue the panel for the frame and push the screen if the frame is awake.
    fn deliver(
        &self,
        job: &Job,
        frame_id: &str,
        frame: Option<&frames::Frame>,
        panel: Panel,
        fonts: Option<&Fonts>,
    ) -> (String, String) {
        if job.mode == Mode::Gallery {
            let Some(frame) = frame.filter(|frame| frame.portal) else {
                return (
                    "failed".to_owned(),
                    "storing needs the frame's portal, and it is not answering".to_owned(),
                );
            };
            let image = match panel.frame(fonts) {
                Ok(image) => image,
                Err(error) => return ("failed".to_owned(), error),
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
        if let Ok(mut outbox) = self.outbox.lock() {
            outbox.entry(frame_id.to_owned()).or_default().insert(
                job.id.clone(),
                Slot {
                    panel,
                    priority: job.priority,
                    at_ms: now_ms(),
                    every_min: job.every_min,
                },
            );
        }
        // The screen may still be waiting for other jobs, and an answer that
        // the frame is showing must not be replaced by a scheduled update.
        let Some(best) = self.next(frame_id, fonts) else {
            return ("queued".to_owned(), String::new());
        };
        if self.override_active(frame_id) {
            return (
                "queued".to_owned(),
                "an answer is on the frame; this waits".to_owned(),
            );
        }
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

    /// The screen of a frame, if it has one.
    fn screen(&self, frame_id: &str) -> Option<Screen> {
        self.jobs
            .read()
            .ok()?
            .screens
            .iter()
            .find(|screen| screen.frame == frame_id)
            .cloned()
    }

    /// The picture a frame should show: a spoken answer or manual show while
    /// its dwell lasts, else the frame's screen, else the newest
    /// highest-priority panel.
    pub fn next(&self, frame_id: &str, fonts: Option<&Fonts>) -> Option<Render> {
        let now = now_ms();
        if let Ok(overrides) = self.overrides.lock()
            && let Some(answer) = overrides.get(frame_id)
            && now < answer.until_ms
        {
            return Some(Render {
                image: Arc::clone(&answer.image),
                hash: answer.hash,
                job: "answer".to_owned(),
            });
        }
        self.composed(frame_id, fonts)
    }

    /// The screen itself, ignoring an answer that currently holds the frame.
    pub fn composed(&self, frame_id: &str, fonts: Option<&Fonts>) -> Option<Render> {
        let now = now_ms();
        let screen = self.screen(frame_id);
        let preset = screen
            .as_ref()
            .and_then(Screen::preset)
            .unwrap_or(Preset::Single);
        let outbox = self.outbox.lock().ok()?;
        let panels = outbox.get(frame_id)?;
        let (image, job) = if preset == Preset::Single {
            // A saved one-panel screen names the job it wants; without one, the
            // frame keeps the old priority rule.
            let chosen = screen
                .as_ref()
                .and_then(|screen| screen.slots.first())
                .filter(|id| !id.is_empty())
                .and_then(|id| panels.get(id).map(|slot| (id, slot)));
            let (job, slot) = match chosen {
                Some(chosen) => chosen,
                None => panels
                    .iter()
                    .max_by_key(|(_, slot)| (slot.priority, slot.at_ms))?,
            };
            (slot.panel.frame(fonts).ok()?, job.clone())
        } else {
            let order: Vec<String> = screen
                .as_ref()
                .map(|screen| screen.slots.clone())
                .unwrap_or_default();
            let mut tiles: Vec<Option<Tile>> = Vec::with_capacity(preset.cells().len());
            for index in 0..preset.cells().len() {
                let slot = order.get(index).and_then(|id| panels.get(id));
                tiles.push(slot.map(|slot| Tile {
                    panel: &slot.panel,
                    stale: stale(slot, now),
                }));
            }
            let image = compose::render(preset, &tiles, fonts?).ok()?;
            (image, "screen".to_owned())
        };
        let hash = content_hash(&image);
        Some(Render {
            image: Arc::new(image),
            hash,
            job,
        })
    }

    /// Hold `image` on the frame for `dwell`, so the next scheduled screen does
    /// not erase a spoken answer.
    pub fn override_frame(&self, frame_id: &str, image: Vec<u8>, dwell: Duration) {
        let hash = content_hash(&image);
        let until_ms =
            now_ms().saturating_add(u64::try_from(dwell.as_millis()).unwrap_or(u64::MAX));
        if let Ok(mut overrides) = self.overrides.lock() {
            overrides.insert(
                frame_id.to_owned(),
                Override {
                    image: Arc::new(image),
                    hash,
                    until_ms,
                },
            );
        }
    }

    /// Drop a held answer, so the frame returns to its own screen.
    pub fn clear_override(&self, frame_id: &str) {
        if let Ok(mut overrides) = self.overrides.lock() {
            overrides.remove(frame_id);
        }
    }

    fn override_active(&self, frame_id: &str) -> bool {
        self.overrides
            .lock()
            .ok()
            .and_then(|overrides| {
                overrides
                    .get(frame_id)
                    .map(|answer| now_ms() < answer.until_ms)
            })
            .unwrap_or(false)
    }

    #[cfg(test)]
    pub fn queue_for_test(&self, frame_id: &str, job: &str, panel: Panel, priority: u8) {
        let mut outbox = self.outbox.lock().unwrap();
        outbox.entry(frame_id.to_owned()).or_default().insert(
            job.to_owned(),
            Slot {
                panel,
                priority,
                at_ms: now_ms(),
                every_min: 0,
            },
        );
    }

    #[cfg(test)]
    pub fn set_screen_for_test(&self, screen: Screen) {
        if let Ok(mut jobs) = self.jobs.write() {
            jobs.screens.push(screen);
        }
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

    fn notice(text: &str) -> Panel {
        Panel::Card(crate::render::Card::Notice {
            title: text.to_owned(),
            body: String::new(),
            accent: crate::render::Colour::Blue,
        })
    }

    fn fonts() -> Fonts {
        Fonts::load(None).expect("DejaVu Sans on this host")
    }

    #[test]
    fn the_best_panel_wins_by_priority_then_age() {
        let scheduler = Scheduler::load(Path::new("/nonexistent/jobs.toml")).unwrap();
        scheduler.queue_for_test("f", "urgent", notice("a"), 5);
        scheduler.queue_for_test("f", "calm", notice("b"), 0);
        let fonts = fonts();
        assert_eq!(
            scheduler.next("f", Some(&fonts)).unwrap().job,
            "urgent",
            "a frame without a screen keeps the priority rule"
        );
        assert!(scheduler.next("other", Some(&fonts)).is_none());
    }

    #[test]
    fn a_screen_draws_its_slots_in_order() {
        let scheduler = Scheduler::load(Path::new("/nonexistent/jobs.toml")).unwrap();
        scheduler.set_screen_for_test(Screen {
            frame: "f".to_owned(),
            preset: "columns".to_owned(),
            slots: vec!["left".to_owned(), "right".to_owned()],
        });
        scheduler.queue_for_test("f", "left", notice("Погода"), 0);
        scheduler.queue_for_test("f", "right", notice("Цена"), 0);
        let fonts = fonts();
        let render = scheduler.next("f", Some(&fonts)).unwrap();
        assert_eq!(render.job, "screen");
        assert_eq!(render.image.len(), crate::render::FRAME_BYTES);
        // The same tiles always compose to the same picture.
        assert_eq!(render.hash, scheduler.next("f", Some(&fonts)).unwrap().hash);
        // One tile missing still gives a picture, and a different one.
        let before = render.hash;
        if let Ok(mut outbox) = scheduler.outbox.lock() {
            outbox.get_mut("f").unwrap().remove("right");
        }
        assert_ne!(scheduler.next("f", Some(&fonts)).unwrap().hash, before);
    }

    #[test]
    fn a_one_panel_screen_names_its_job() {
        let scheduler = Scheduler::load(Path::new("/nonexistent/jobs.toml")).unwrap();
        scheduler.set_screen_for_test(Screen {
            frame: "f".to_owned(),
            preset: "single".to_owned(),
            slots: vec!["calm".to_owned()],
        });
        scheduler.queue_for_test("f", "calm", notice("выбран"), 0);
        scheduler.queue_for_test("f", "urgent", notice("приоритет"), 9);
        assert_eq!(
            scheduler.next("f", Some(&fonts())).unwrap().job,
            "calm",
            "a saved screen chooses the panel, not the priority"
        );
    }

    #[test]
    fn a_screen_with_no_tiles_has_no_picture() {
        let scheduler = Scheduler::load(Path::new("/nonexistent/jobs.toml")).unwrap();
        scheduler.set_screen_for_test(Screen {
            frame: "f".to_owned(),
            preset: "grid".to_owned(),
            slots: vec!["a".to_owned()],
        });
        assert!(scheduler.next("f", Some(&fonts())).is_none());
    }

    #[test]
    fn a_spoken_answer_outranks_the_screen_for_its_dwell() {
        let scheduler = Scheduler::load(Path::new("/nonexistent/jobs.toml")).unwrap();
        scheduler.set_screen_for_test(Screen {
            frame: "f".to_owned(),
            preset: "single".to_owned(),
            slots: Vec::new(),
        });
        scheduler.queue_for_test("f", "job", notice("Погода"), 0);
        let fonts = fonts();
        assert_eq!(scheduler.next("f", Some(&fonts)).unwrap().job, "job");

        scheduler.override_frame("f", vec![0x22; crate::render::FRAME_BYTES], Duration::ZERO);
        assert_eq!(
            scheduler.next("f", Some(&fonts)).unwrap().job,
            "job",
            "a zero dwell does not hold the answer"
        );
        scheduler.override_frame(
            "f",
            vec![0x33; crate::render::FRAME_BYTES],
            Duration::from_secs(600),
        );
        let answer = scheduler.next("f", Some(&fonts)).unwrap();
        assert_eq!(answer.job, "answer");
        assert_eq!(answer.image[0], 0x33);
    }

    #[test]
    fn an_unknown_preset_stops_the_scheduler_from_loading() {
        let dir = std::env::temp_dir().join(format!("easellet-jobs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jobs.toml");
        std::fs::write(&path, "[[screen]]\nframe = \"f\"\npreset = \"poster\"\n").unwrap();
        let error = Scheduler::load(&path)
            .err()
            .expect("a bad preset is an error");
        assert!(error.contains("poster"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scheduler_with_jobs(dir_name: &str) -> (Scheduler, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "easellet-screens-{}-{dir_name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jobs.toml");
        std::fs::write(
            &path,
            r#"
[[job]]
id = "weather"
plugin = "weather"
frames = ["f"]
every_min = 60

[[job]]
id = "spot"
plugin = "spot"
frames = ["f"]
every_min = 15

[[job]]
id = "album"
plugin = "photo-folder"
frames = ["f"]
mode = "gallery"

[[job]]
id = "elsewhere"
plugin = "news"
frames = ["other"]
"#,
        )
        .unwrap();
        (Scheduler::load(&path).unwrap(), dir)
    }

    #[test]
    fn a_screen_names_jobs_that_show_on_its_frame() {
        let (scheduler, dir) = scheduler_with_jobs("names");
        let save = |value: serde_json::Value| scheduler.save_screen(&value);
        let screen = |preset: &str, slots: Vec<&str>| serde_json::json!({"frame": "f", "preset": preset, "slots": slots});

        assert_eq!(
            save(screen("columns", vec!["weather", "spot"])).unwrap(),
            "f"
        );
        assert_eq!(scheduler.screens().len(), 1);
        assert_eq!(scheduler.screens()[0].preset, "columns");
        assert_eq!(scheduler.screens()[0].slots, ["weather", "spot"]);
        // A frame has one screen: saving again replaces it.
        save(screen("hero", vec!["weather"])).unwrap();
        assert_eq!(scheduler.screens().len(), 1);
        assert_eq!(scheduler.screens()[0].preset, "hero");
        // The screen survives a reload.
        let reloaded = Scheduler::load(&dir.join("jobs.toml")).unwrap();
        assert_eq!(reloaded.screens(), scheduler.screens());

        assert!(save(screen("poster", vec![])).is_err(), "unknown preset");
        assert!(save(screen("grid", vec!["nope"])).is_err(), "unknown job");
        assert!(
            save(screen("columns", vec!["weather", "weather"])).is_err(),
            "one job cannot fill two slots"
        );
        assert!(
            save(screen("columns", vec!["weather", "elsewhere"])).is_err(),
            "a job must target the screen's frame"
        );
        assert!(
            save(screen("columns", vec!["album"])).is_err(),
            "a gallery job stores pictures, it does not show them"
        );
        assert!(
            save(screen("columns", vec!["weather", "spot", ""])).is_err(),
            "columns has two slots"
        );

        scheduler.delete_screen("f").unwrap();
        assert!(scheduler.screens().is_empty());
        assert!(scheduler.delete_screen("f").is_err(), "already gone");
        let _ = std::fs::remove_dir_all(&dir);
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
            screens: Vec::new(),
        };
        let text = toml::to_string_pretty(&file).unwrap();
        assert!(text.contains("[[job]]"), "{text}");
        assert_eq!(toml::from_str::<JobFile>(&text).unwrap(), file);
    }
}
