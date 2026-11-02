//! Plugins: programs on the bridge host that produce something to show.
//!
//! A plugin is a directory with a `plugin.toml` manifest and an executable in any
//! language. The bridge runs it with one JSON document on stdin and reads one JSON
//! document from stdout: a card, text, an image, or "nothing new". The bridge then
//! renders that for each target frame. See `research/bridge-ui-design.md`
//! section 5 for the contract; `bridge/plugins/README.md` for authors.
//!
//! Trust: installing a plugin is installing a program, so plugins appear only by
//! someone putting files on this host. The web UI can list, configure, preview
//! and run them, never add or edit one. Parameters reach a plugin as JSON on
//! stdin, never as arguments or through a shell. The environment is cleared down
//! to a few variables; every run gets its own scratch directory and a timeout.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::render::{self, Card, Fit, Fonts};

/// The protocol version in `api` on both sides.
pub const API: u64 = 1;
/// Longest a manifest may let a plugin run.
const MAX_TIMEOUT_S: u64 = 120;
/// Largest stdout accepted: a base64 photo fits, a runaway loop does not.
const MAX_STDOUT: u64 = 24 * 1024 * 1024;
/// Stderr kept for the run log.
const MAX_STDERR: u64 = 16 * 1024;
/// Longest text parameter.
const MAX_PARAM: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamKind {
    String,
    Text,
    Number,
    Bool,
    Choice,
    Secret,
    Location,
    Url,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Param {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ParamKind,
    pub title: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub choices: Vec<String>,
    #[serde(default)]
    pub note: String,
    /// Not shown in the web form: filled by the assistant (`text_param`).
    #[serde(default)]
    pub hidden: bool,
}

/// What the voice assistant may call the plugin for (B6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assistant {
    /// The main phrase, e.g. "показать погоду".
    pub intent: String,
    pub description: String,
    /// Other ways people say it ("покажи погоду", "какая погода").
    #[serde(default)]
    pub phrases: Vec<String>,
    /// A text parameter that receives the whole question ("нарисуй горы
    /// ночью" -> `prompt`).
    #[serde(default)]
    pub text_param: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_version")]
    pub version: String,
    /// The program, relative to the plugin's directory.
    pub run: String,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    /// What it may return: `card`, `text`, `image`, `none`.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// A suggestion for new schedules, e.g. "3h".
    #[serde(default)]
    pub default_every: Option<String>,
    #[serde(default, rename = "param")]
    pub params: Vec<Param>,
    #[serde(default)]
    pub assistant: Option<Assistant>,
}

fn default_version() -> String {
    "1".to_owned()
}

fn default_timeout() -> u64 {
    20
}

#[derive(Debug, Clone)]
pub struct Plugin {
    pub manifest: Manifest,
    pub dir: PathBuf,
}

/// Every plugin under `dirs`, earlier directories winning a name clash (the
/// user's own directory comes first, so it can replace a shipped plugin), and
/// the reasons any directory was skipped.
pub fn discover(dirs: &[PathBuf]) -> (Vec<Plugin>, Vec<String>) {
    let mut plugins: Vec<Plugin> = Vec::new();
    let mut problems = Vec::new();
    let mut names = BTreeSet::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        for path in entries {
            let manifest_path = path.join("plugin.toml");
            if !manifest_path.is_file() {
                continue;
            }
            match load(&path) {
                Ok(plugin) => {
                    if names.insert(plugin.manifest.name.clone()) {
                        plugins.push(plugin);
                    }
                }
                Err(error) => problems.push(format!("{}: {error}", path.display())),
            }
        }
    }
    (plugins, problems)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Read and check one plugin directory.
pub fn load(dir: &Path) -> Result<Plugin, String> {
    let text =
        std::fs::read_to_string(dir.join("plugin.toml")).map_err(|error| error.to_string())?;
    let manifest: Manifest = toml::from_str(&text).map_err(|error| error.to_string())?;
    if !valid_name(&manifest.name) {
        return Err(format!(
            "name {:?} must be [a-z0-9-], at most 32",
            manifest.name
        ));
    }
    if dir.file_name().and_then(|name| name.to_str()) != Some(manifest.name.as_str()) {
        return Err(format!("name {:?} must match the directory", manifest.name));
    }
    let run = Path::new(&manifest.run);
    if run.is_absolute()
        || run
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("run must be a path inside the plugin directory".to_owned());
    }
    let program = dir.join(run);
    if !is_executable(&program) {
        return Err(format!("{} is not an executable file", program.display()));
    }
    if manifest.timeout_s == 0 || manifest.timeout_s > MAX_TIMEOUT_S {
        return Err(format!("timeout_s must be 1..={MAX_TIMEOUT_S}"));
    }
    let mut ids = BTreeSet::new();
    for param in &manifest.params {
        if !valid_name(&param.id.replace('_', "-")) || !ids.insert(&param.id) {
            return Err(format!("param id {:?} is invalid or repeated", param.id));
        }
        if param.kind == ParamKind::Choice && param.choices.is_empty() {
            return Err(format!("param {} is a choice without choices", param.id));
        }
    }
    Ok(Plugin {
        manifest,
        dir: dir.to_path_buf(),
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Check `values` against the manifest: unknown keys refused, required ones
/// present (or defaulted), each of its declared type. Returns the complete set.
pub fn validate(
    manifest: &Manifest,
    values: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    for key in values.keys() {
        if !manifest.params.iter().any(|param| &param.id == key) {
            return Err(format!("unknown parameter {key}"));
        }
    }
    let mut out = serde_json::Map::new();
    for param in &manifest.params {
        let value = match values.get(&param.id) {
            Some(serde_json::Value::String(text)) if text.is_empty() => None,
            Some(serde_json::Value::Null) | None => None,
            Some(value) => Some(value.clone()),
        }
        .or_else(|| param.default.clone());
        let Some(value) = value else {
            if param.required {
                return Err(format!("{} is required", param.title));
            }
            continue;
        };
        let value = match param.kind {
            ParamKind::Number => match &value {
                serde_json::Value::Number(_) => value,
                serde_json::Value::String(text) => text
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(serde_json::Value::Number)
                    .ok_or_else(|| format!("{} must be a number", param.title))?,
                _ => return Err(format!("{} must be a number", param.title)),
            },
            ParamKind::Bool => match &value {
                serde_json::Value::Bool(_) => value,
                serde_json::Value::String(text) => {
                    serde_json::Value::Bool(matches!(text.as_str(), "true" | "1" | "on"))
                }
                _ => return Err(format!("{} must be yes or no", param.title)),
            },
            _ => {
                let text = value
                    .as_str()
                    .ok_or_else(|| format!("{} must be text", param.title))?;
                if text.len() > MAX_PARAM || text.contains('\0') {
                    return Err(format!("{} is too long", param.title));
                }
                if param.kind == ParamKind::Choice
                    && !param.choices.iter().any(|choice| choice == text)
                {
                    return Err(format!(
                        "{} must be one of {:?}",
                        param.title, param.choices
                    ));
                }
                if param.kind == ParamKind::Url
                    && !(text.starts_with("http://") || text.starts_with("https://"))
                {
                    return Err(format!("{} must be an http(s) URL", param.title));
                }
                value
            }
        };
        out.insert(param.id.clone(), value);
    }
    Ok(out)
}

/// The frame a run is for, as the plugin sees it.
#[derive(Debug, Clone, Serialize)]
pub struct Target {
    pub id: String,
    pub name: String,
    pub width: usize,
    pub height: usize,
    pub orientation: &'static str,
    pub palette: Vec<&'static str>,
}

impl Target {
    /// The Spectra 6 panel; with no frame chosen, previews use this.
    pub fn spectra(id: &str, name: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            width: render::WIDTH,
            height: render::HEIGHT,
            orientation: "landscape",
            palette: vec!["black", "white", "yellow", "red", "blue", "green"],
        }
    }
}

/// What a plugin returned.
#[derive(Debug)]
pub enum Output {
    Card(Card),
    Image {
        bytes: Vec<u8>,
        fit: Fit,
    },
    /// Nothing new: keep what is shown.
    None,
}

#[derive(Debug)]
pub struct Outcome {
    pub output: Output,
    /// State to hand back next run.
    pub state: Option<serde_json::Value>,
    /// When the plugin would like to run again.
    pub next_run_s: Option<u64>,
    /// What the assistant should say when a voice question ran the plugin
    /// ("Не нашёл место «ВСПУ», показываю Хельсинки").
    pub say: Option<String>,
    /// Its stderr, for the run log.
    pub log: String,
    pub ms: u64,
}

/// Run `plugin` once.
pub fn run(
    plugin: &Plugin,
    params: &serde_json::Map<String, serde_json::Value>,
    target: &Target,
    state: Option<&serde_json::Value>,
) -> Result<Outcome, String> {
    let started = Instant::now();
    let scratch = Scratch::new(&plugin.manifest.name)?;
    let input = serde_json::json!({
        "api": API,
        "params": params,
        "frame": target,
        "locale": std::env::var("LANG").ok().and_then(|lang| lang.split('.').next().map(|code| code.replace('_', "-"))).unwrap_or_else(|| "ru-RU".to_owned()),
        "timezone": timezone(),
        "now": now_utc(),
        "scratch": scratch.path,
        "state": state.cloned().unwrap_or(serde_json::Value::Null),
    });
    let program = plugin.dir.join(&plugin.manifest.run);
    let mut command = Command::new(&program);
    command
        .current_dir(&plugin.dir)
        .env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .env("LANG", "C.UTF-8")
        .env("EASEL_PLUGIN_SCRATCH", &scratch.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for variable in [
        "HOME",
        "TZ",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "http_proxy",
        "https_proxy",
        "no_proxy",
    ] {
        if let Some(value) = std::env::var_os(variable) {
            command.env(variable, value);
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("start {}: {error}", program.display()))?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_string();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = (&mut stdout).take(MAX_STDOUT + 1).read_to_end(&mut out);
        out
    });
    let errors = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = (&mut stderr).take(MAX_STDERR).read_to_end(&mut out);
        // Drain the rest so a chatty plugin does not block on a full pipe.
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        out
    });
    let deadline = started + Duration::from_secs(plugin.manifest.timeout_s);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {} s", plugin.manifest.timeout_s));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let _ = writer.join();
    let stdout = reader.join().unwrap_or_default();
    let log = String::from_utf8_lossy(&errors.join().unwrap_or_default()).into_owned();
    if !status.success() {
        let tail: String = log
            .lines()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "exited with {status}{}",
            if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            }
        ));
    }
    if stdout.len() as u64 > MAX_STDOUT {
        return Err("output too large".to_owned());
    }
    let mut outcome = parse_output(&stdout, &scratch.path, &plugin.manifest.outputs)?;
    outcome.log = log;
    outcome.ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(outcome)
}

/// Read a plugin's answer.
fn parse_output(stdout: &[u8], scratch: &Path, allowed: &[String]) -> Result<Outcome, String> {
    let answer: serde_json::Value =
        serde_json::from_slice(stdout).map_err(|error| format!("output is not JSON: {error}"))?;
    if answer.get("api").and_then(serde_json::Value::as_u64) != Some(API) {
        return Err(format!("output must say \"api\": {API}"));
    }
    let output = answer
        .get("output")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "output must have an \"output\" object".to_owned())?;
    let kind = output
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "output.type is missing".to_owned())?;
    if !allowed.is_empty() && !allowed.iter().any(|allowed| allowed == kind) {
        return Err(format!(
            "output type {kind} is not in the manifest's outputs"
        ));
    }
    let text = |key: &str| {
        output
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let output = match kind {
        "card" => {
            let mut card = output.clone();
            card.remove("type");
            Output::Card(
                serde_json::from_value(serde_json::Value::Object(card))
                    .map_err(|error| format!("card: {error}"))?,
            )
        }
        "text" => Output::Card(Card::Notice {
            title: text("title"),
            body: text("text"),
            accent: render::Colour::default(),
        }),
        "image" => {
            let fit: Fit = output
                .get("fit")
                .map(|fit| serde_json::from_value(fit.clone()))
                .transpose()
                .map_err(|error| format!("image.fit: {error}"))?
                .unwrap_or_default();
            let bytes = if let Some(path) = output.get("path").and_then(serde_json::Value::as_str) {
                read_scratch_file(scratch, path)?
            } else if let Some(encoded) = output.get("base64").and_then(serde_json::Value::as_str) {
                crate::ui::base64_decode(encoded.trim())
                    .ok_or_else(|| "image.base64 is not base64".to_owned())?
            } else {
                return Err(
                    "image needs \"path\" (in the scratch directory) or \"base64\"".to_owned(),
                );
            };
            Output::Image { bytes, fit }
        }
        "none" => Output::None,
        other => return Err(format!("unknown output type {other}")),
    };
    Ok(Outcome {
        output,
        state: answer
            .get("state")
            .cloned()
            .filter(|state| !state.is_null()),
        next_run_s: answer.get("next_run_s").and_then(serde_json::Value::as_u64),
        say: answer
            .get("say")
            .and_then(serde_json::Value::as_str)
            .map(|say| say.chars().take(300).collect()),
        log: String::new(),
        ms: 0,
    })
}

/// A file the plugin wrote into its scratch directory; nothing outside it.
fn read_scratch_file(scratch: &Path, path: &str) -> Result<Vec<u8>, String> {
    let candidate = Path::new(path);
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        scratch.join(candidate)
    };
    let resolved = candidate
        .canonicalize()
        .map_err(|error| format!("image.path: {error}"))?;
    let root = scratch.canonicalize().map_err(|error| error.to_string())?;
    if !resolved.starts_with(&root) {
        return Err("image.path must be inside the run's scratch directory".to_owned());
    }
    let meta = std::fs::metadata(&resolved).map_err(|error| error.to_string())?;
    if !meta.is_file() || meta.len() > MAX_STDOUT {
        return Err("image.path is not a file of a sane size".to_owned());
    }
    std::fs::read(&resolved).map_err(|error| error.to_string())
}

/// The panel's bytes for an outcome, or `None` when there is nothing new.
pub fn frame_for(outcome: &Outcome, fonts: Option<&Fonts>) -> Result<Option<Vec<u8>>, String> {
    match &outcome.output {
        Output::Card(card) => {
            let fonts = fonts.ok_or_else(|| "no font for cards".to_owned())?;
            Ok(Some(render::card_frame(card, fonts)))
        }
        Output::Image { bytes, fit } => render::photo_frame(bytes, *fit).map(Some),
        Output::None => Ok(None),
    }
}

/// A per-run directory, removed afterwards.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Result<Self, String> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "easellet-bridge-{}-{name}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The host's time zone name, if it can be told.
fn timezone() -> String {
    if let Ok(zone) = std::env::var("TZ")
        && !zone.is_empty()
    {
        return zone.trim_start_matches(':').to_owned();
    }
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|link| {
            let link = link.to_string_lossy().into_owned();
            link.split_once("zoneinfo/")
                .map(|(_, zone)| zone.to_owned())
        })
        .unwrap_or_else(|| "UTC".to_owned())
}

/// Now as an RFC 3339 UTC timestamp, without a date library.
fn now_utc() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest / 60 % 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plugin directory under the temp dir, removed on drop.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str, manifest: &str, script: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let root = std::env::temp_dir().join(format!(
                "pf-plugin-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("plugin.toml"), manifest).unwrap();
            std::fs::write(dir.join("run"), script).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir.join("run"), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            Self { root }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const MANIFEST: &str = r#"
name = "echo"
title = "Echo"
run = "run"
timeout_s = 2
outputs = ["card", "none"]

[[param]]
id = "who"
type = "string"
title = "Who"
required = true

[[param]]
id = "colour"
type = "choice"
title = "Colour"
choices = ["red", "blue"]
default = "blue"
"#;

    fn plugin(script: &str) -> (Plugin, Fixture) {
        let fixture = Fixture::new("echo", MANIFEST, script);
        let (mut plugins, problems) = discover(&[fixture.root.clone()]);
        assert!(problems.is_empty(), "{problems:?}");
        (plugins.remove(0), fixture)
    }

    fn params(json: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        json.as_object().unwrap().clone()
    }

    #[test]
    fn a_plugin_reads_its_input_and_returns_a_card() {
        // A shell plugin: it can see its parameters only through stdin.
        let (plugin, _fixture) = plugin(
            "#!/bin/sh\nread -r input\ncase \"$input\" in *'\"who\":\"мир\"'*) who=ok;; *) who=bad;; esac\n\
             printf '{\"api\":1,\"output\":{\"type\":\"card\",\"layout\":\"notice\",\"title\":\"%s\"},\"state\":{\"n\":1},\"next_run_s\":60}' \"$who\"\n",
        );
        let values =
            validate(&plugin.manifest, &params(serde_json::json!({"who": "мир"}))).unwrap();
        assert_eq!(values["colour"], "blue", "the default fills in");
        let outcome = run(&plugin, &values, &Target::spectra("t", "test"), None).unwrap();
        assert!(
            matches!(&outcome.output, Output::Card(Card::Notice { title, .. }) if title == "ok")
        );
        assert_eq!(outcome.state, Some(serde_json::json!({"n": 1})));
        assert_eq!(outcome.next_run_s, Some(60));
    }

    #[test]
    fn parameters_are_checked_against_the_manifest() {
        let (plugin, _fixture) = plugin("#!/bin/sh\nexit 0\n");
        let manifest = &plugin.manifest;
        assert!(
            validate(manifest, &params(serde_json::json!({}))).is_err(),
            "required"
        );
        assert!(
            validate(
                manifest,
                &params(serde_json::json!({"who": "x", "colour": "green"}))
            )
            .is_err()
        );
        assert!(
            validate(
                manifest,
                &params(serde_json::json!({"who": "x", "extra": 1}))
            )
            .is_err()
        );
    }

    #[test]
    fn a_slow_or_failing_or_lying_plugin_is_a_failed_run() {
        let values = params(serde_json::json!({"who": "x", "colour": "red"}));
        let target = Target::spectra("t", "test");
        let (slow, _a) = plugin("#!/bin/sh\nsleep 10\n");
        let started = Instant::now();
        assert!(
            run(&slow, &values, &target, None)
                .unwrap_err()
                .contains("timed out")
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout kills it"
        );
        let (failing, _b) = plugin("#!/bin/sh\necho 'no network' >&2\nexit 3\n");
        assert!(
            run(&failing, &values, &target, None)
                .unwrap_err()
                .contains("no network")
        );
        let (lying, _c) = plugin(
            "#!/bin/sh\necho '{\"api\":1,\"output\":{\"type\":\"image\",\"path\":\"/etc/passwd\"}}'\n",
        );
        assert!(
            run(&lying, &values, &target, None).is_err(),
            "image outside scratch, and not declared"
        );
    }

    #[test]
    fn a_bad_manifest_is_reported_not_loaded() {
        let fixture = Fixture::new("wrong", MANIFEST, "#!/bin/sh\n");
        let (plugins, problems) = discover(&[fixture.root.clone()]);
        assert!(plugins.is_empty());
        assert!(
            problems[0].contains("must match the directory"),
            "{problems:?}"
        );
        let escape = MANIFEST.replace("run = \"run\"", "run = \"../../bin/sh\"");
        let fixture = Fixture::new("echo", &escape, "#!/bin/sh\n");
        let (plugins, problems) = discover(&[fixture.root.clone()]);
        assert!(
            plugins.is_empty() && problems[0].contains("inside"),
            "{problems:?}"
        );
    }

    #[test]
    fn the_clock_is_rfc3339() {
        let now = now_utc();
        assert_eq!(now.len(), 20, "{now}");
        assert!(now.ends_with('Z') && now.as_bytes()[10] == b'T', "{now}");
    }
}
