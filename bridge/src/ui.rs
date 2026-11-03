//! The bridge's web UI: static files and a small JSON API under `/api/`.
//!
//! Same house rules as the frame's own portal (`src/web.rs` in the firmware):
//!
//! - **`Host` must be local**: an IP address, a single-label name, a name under a
//!   local-only suffix, this machine's own name, or one listed in `ui_hosts`. A
//!   page that rebinds its own domain to the bridge still sends that domain, and
//!   gets 403, so it cannot read the API.
//! - **Optional password** (`ui_password`), HTTP Basic, any user name.
//! - **Every POST carries `X-Easellet`**, which a page on another site cannot
//!   add without a CORS preflight the bridge never answers.
//! - **Secrets are write-only**: the settings document says whether they are set,
//!   never what they are.
//!
//! The frame-facing endpoints (`/stt`, `/llm`, `/tts`, `/health`) are not here;
//! they keep their bearer token and their own router.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use crate::frames::{self, Registry};
use crate::history::History;
use crate::jobs::Scheduler;
use crate::opendisplay;
use crate::plugins;
use crate::render::{self, Card, Fonts};
use crate::settings::{Live, RESTART_KEYS, Settings};

const PAGE: &str = include_str!("../web/index.html");
const APP: &str = include_str!("../web/app.js");
/// The frame's own converter, served as is: the firmware gate proves it matches
/// `tools/convert.py`, so pictures prepared here look the same as anywhere else.
const EL6: &str = include_str!("../../tools/el6.mjs");

/// Header every POST must carry; its value is not checked.
pub const POST_HEADER: &str = "X-Easellet";
/// Largest JSON body accepted by the API.
pub const MAX_BODY: usize = 64 * 1024;
/// Largest picture body (`/upload`, `/display`): one frame in the panel's format.
pub const MAX_PICTURE: usize = 512 * 1024;

/// How large a body this path may send.
pub fn body_limit(path: &str) -> usize {
    if path.starts_with("/api/frames/") && (path.ends_with("/upload") || path.ends_with("/display"))
    {
        MAX_PICTURE
    } else {
        MAX_BODY
    }
}
/// Most turns one history request returns.
const MAX_TURNS: usize = 200;

/// The settings and where they came from, shared by the UI and the bridge.
pub struct SettingsState {
    pub settings: Settings,
    /// Keys a flag or variable set for this run; read-only in the UI.
    pub pinned: BTreeSet<String>,
    pub path: PathBuf,
    /// What the bridge reads per request; refreshed on every change.
    pub live: Arc<RwLock<Live>>,
}

/// Everything the UI handler reads.
pub struct Context {
    pub settings: Arc<RwLock<SettingsState>>,
    pub history: Arc<Mutex<History>>,
    pub frames: Arc<Registry>,
    /// Runtime state of the bridge's own OpenDisplay listener.
    pub opendisplay: Arc<Mutex<opendisplay::Status>>,
    pub fonts: Option<Arc<Fonts>>,
    /// Where plugins are looked for, the user's own directory first.
    pub plugin_dirs: Vec<PathBuf>,
    /// What each plugin returned as `state`, by plugin and frame.
    pub plugin_state: Mutex<std::collections::BTreeMap<(String, String), serde_json::Value>>,
    pub scheduler: Arc<Scheduler>,
    /// The release list the frame card offers, and the images it caches.
    pub firmware: Arc<crate::firmware::Store>,
    /// The frames' assistant, for questions typed into the UI; set once the
    /// bridge exists (it holds a tool that holds this context).
    pub bridge: std::sync::OnceLock<Arc<Mutex<crate::server::Bridge>>>,
    pub started: Instant,
    /// This machine's own host names, accepted in `Host`.
    pub hostnames: Vec<String>,
}

/// One request, as far as the UI needs it.
pub struct Request<'a> {
    pub method: &'a str,
    /// Path and query, as sent.
    pub url: &'a str,
    pub host: Option<&'a str>,
    pub authorization: Option<&'a str>,
    /// Whether [`POST_HEADER`] was present.
    pub marked: bool,
    pub body: Vec<u8>,
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub headers: Vec<(&'static str, String)>,
}

impl Response {
    fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
            headers: vec![("Cache-Control", "no-store".to_owned())],
        }
    }

    fn text(status: u16, body: impl Into<String>) -> Self {
        Self::new(status, "text/plain; charset=utf-8", body.into())
    }

    fn json(status: u16, value: &serde_json::Value) -> Self {
        Self::new(status, "application/json; charset=utf-8", value.to_string())
    }
}

/// Is this a UI path rather than one of the frame's endpoints?
pub fn is_ui_path(path: &str) -> bool {
    matches!(path, "/" | "/index.html" | "/app.js" | "/el6.mjs") || path.starts_with("/api/")
}

pub fn handle(request: Request<'_>, context: &Context) -> Response {
    let (path, query) = request.url.split_once('?').unwrap_or((request.url, ""));
    let (password, extra_hosts) = match context.settings.read() {
        Ok(state) => {
            let live = state
                .live
                .read()
                .map(|live| live.clone())
                .unwrap_or_default();
            (live.ui_password, live.ui_hosts)
        }
        Err(_) => return Response::text(500, "ERR settings unavailable\n"),
    };
    if let Some(host) = request.host
        && !local_host(host, &context.hostnames, &extra_hosts)
    {
        return Response::text(403, "ERR use the bridge's address or local name\n");
    }
    if let Some(password) = password
        && request.authorization.and_then(basic_password).as_deref() != Some(password.as_str())
    {
        let mut response = Response::text(401, "ERR the bridge asks for its password\n");
        response.headers.push((
            "WWW-Authenticate",
            "Basic realm=\"easellet-bridge\", charset=\"UTF-8\"".to_owned(),
        ));
        return response;
    }
    if request.method == "POST" && !request.marked {
        return Response::text(403, format!("ERR POST needs the {POST_HEADER} header\n"));
    }
    match (request.method, path) {
        ("GET", "/" | "/index.html") => Response::new(200, "text/html; charset=utf-8", PAGE),
        ("GET", "/app.js") => Response::new(200, "text/javascript; charset=utf-8", APP),
        ("GET", "/el6.mjs") => Response::new(200, "text/javascript; charset=utf-8", EL6),
        ("GET", "/api/status") => status(context),
        ("GET", "/api/settings") => settings(context),
        ("POST", "/api/settings") => save_settings(context, &request.body),
        ("GET", "/api/history") => history(context, query),
        ("POST", "/api/render/preview") => match card_frame(context, &request.body) {
            Ok(frame) => {
                match render::png(&render::unpack(&frame), render::WIDTH, render::HEIGHT) {
                    Ok(png) => Response::new(200, "image/png", png),
                    Err(error) => Response::text(500, format!("ERR png: {error}\n")),
                }
            }
            Err(response) => response,
        },
        ("POST", "/api/assistant/ask") => ask(context, &request.body),
        ("GET", "/api/plugins") => plugin_list(context),
        ("GET", "/api/jobs") => {
            let (found, _) = plugins::discover(&context.plugin_dirs);
            Response::json(200, &context.scheduler.to_json(&found))
        }
        ("POST", "/api/jobs/save") => job_request(&request.body, |request| {
            let (found, _) = plugins::discover(&context.plugin_dirs);
            context
                .scheduler
                .save_job(request, &found)
                .map(|id| serde_json::json!({"id": id}))
        }),
        ("POST", "/api/jobs/delete") => job_request(&request.body, |request| {
            let id = request
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            context
                .scheduler
                .delete_job(id)
                .map(|()| serde_json::json!({"deleted": id}))
        }),
        ("POST", "/api/jobs/run") => job_request(&request.body, |request| {
            let id = request
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            context
                .scheduler
                .run_now(id, context)
                .map(|runs| serde_json::json!({"runs": runs}))
        }),
        ("GET", "/api/screens") => Response::json(
            200,
            &serde_json::json!({"screens": context.scheduler.screens()}),
        ),
        ("POST", "/api/screens/save") => job_request(&request.body, |request| {
            context
                .scheduler
                .save_screen(request)
                .map(|frame| serde_json::json!({"saved": frame}))
        }),
        ("POST", "/api/screens/delete") => job_request(&request.body, |request| {
            let frame = request
                .get("frame")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            context
                .scheduler
                .delete_screen(frame)
                .map(|()| serde_json::json!({"deleted": frame}))
        }),
        ("POST", path) if path.starts_with("/api/plugins/") => {
            plugin_action(context, &path["/api/plugins/".len()..], &request.body)
        }
        ("GET", "/api/firmware") => firmware_status(context),
        ("POST", "/api/firmware/refresh") => firmware_refresh(context),
        ("GET", "/api/opendisplay") => opendisplay_status(context),
        ("POST", "/api/opendisplay") => save_opendisplay(context, &request.body),
        ("GET", "/api/frames") => frame_list(context),
        ("POST", "/api/frames/scan") => {
            rescan(context);
            frame_list(context)
        }
        ("POST", "/api/frames/add") => add_frame(context, &request.body),
        ("POST", "/api/frames/remove") => remove_frame(context, &request.body),
        ("GET", path) if path.starts_with("/api/frames/") && path.ends_with("/screen.png") => {
            screen_png(context, path)
        }
        ("GET", path) if path.starts_with("/api/frames/") && path.ends_with("/ota") => {
            frame_ota(context, path)
        }
        ("POST", path) if path.starts_with("/api/frames/") && path.ends_with("/firmware") => {
            frame_firmware_start(context, path, &request.body)
        }
        ("GET", path) if path.starts_with("/api/frames/") && path.ends_with("/firmware") => {
            frame_firmware_state(context, path)
        }
        (method, path) if path.starts_with("/api/frames/") => frame_action(
            context,
            method,
            &path["/api/frames/".len()..],
            query,
            request.body,
        ),
        ("GET", _) => Response::text(404, "ERR not here\n"),
        _ => Response::text(405, "ERR method not allowed\n"),
    }
}

fn status(context: &Context) -> Response {
    let Ok(state) = context.settings.read() else {
        return Response::text(500, "ERR settings unavailable\n");
    };
    let settings = &state.settings;
    let turns = context
        .history
        .lock()
        .map_or(0, |history| history.recent(usize::MAX).len());
    Response::json(
        200,
        &serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_s": context.started.elapsed().as_secs(),
            "addr": settings.addr,
            "backends": {"stt": settings.stt, "llm": settings.llm, "tts": settings.tts},
            "wake": crate::settings::normalize_wake(&settings.wake),
            "token": !settings.token.is_empty(),
            "ui_password": !settings.ui_password.is_empty(),
            "settings_file": state.path.display().to_string(),
            "turns": turns,
        }),
    )
}

fn settings(context: &Context) -> Response {
    let Ok(state) = context.settings.read() else {
        return Response::text(500, "ERR settings unavailable\n");
    };
    let mut document = state.settings.to_json(&state.pinned);
    document["file"] = serde_json::Value::String(state.path.display().to_string());
    Response::json(200, &document)
}

fn save_settings(context: &Context, body: &[u8]) -> Response {
    if body.len() > MAX_BODY {
        return Response::text(413, "ERR settings change too large\n");
    }
    let change: serde_json::Value = match serde_json::from_slice(body) {
        Ok(change) => change,
        Err(error) => return Response::text(400, format!("ERR settings JSON: {error}\n")),
    };
    let Ok(mut state) = context.settings.write() else {
        return Response::text(500, "ERR settings unavailable\n");
    };
    let mut next = state.settings.clone();
    let changed = match next.apply(&change, &state.pinned) {
        Ok(changed) => changed,
        Err(error) => return Response::text(400, format!("ERR {error}\n")),
    };
    if !changed.is_empty() {
        if let Err(error) = next.save(&state.path) {
            return Response::text(500, format!("ERR saving settings: {error}\n"));
        }
        if let Ok(mut live) = state.live.write() {
            *live = next.live();
        }
        state.settings = next;
    }
    let restart: Vec<&String> = changed
        .iter()
        .filter(|key| RESTART_KEYS.contains(&key.as_str()))
        .collect();
    Response::json(
        200,
        &serde_json::json!({"changed": changed, "restart": restart}),
    )
}

fn history(context: &Context, query: &str) -> Response {
    let limit = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("limit="))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(50)
        .min(MAX_TURNS);
    let Ok(history) = context.history.lock() else {
        return Response::text(500, "ERR history unavailable\n");
    };
    Response::json(200, &serde_json::json!({"turns": history.recent(limit)}))
}

/// A card from a JSON body, drawn and packed for the Spectra panel.
fn card_frame(context: &Context, body: &[u8]) -> Result<Vec<u8>, Response> {
    let Some(fonts) = &context.fonts else {
        return Err(Response::text(
            503,
            "ERR no font for cards (install DejaVu Sans)\n",
        ));
    };
    let card: Card = serde_json::from_slice(body)
        .map_err(|error| Response::text(400, format!("ERR card: {error}\n")))?;
    Ok(render::card_frame(&card, fonts))
}

fn job_request(
    body: &[u8],
    action: impl FnOnce(&serde_json::Value) -> Result<serde_json::Value, String>,
) -> Response {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(request) => match action(&request) {
            Ok(answer) => Response::json(200, &answer),
            Err(error) => Response::text(400, format!("ERR {error}\n")),
        },
        Err(error) => Response::text(400, format!("ERR request JSON: {error}\n")),
    }
}

/// A question typed into the UI: the model's answer, and the same answer
/// spoken (WAV, base64) for the browser to play. It goes through the bridge
/// like a frame's turn, so it shows in the history (as peer `bridge-ui`).
fn ask(context: &Context, body: &[u8]) -> Response {
    let question = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|request| {
            request
                .get("text")
                .and_then(|text| text.as_str().map(str::to_owned))
        })
        .unwrap_or_default();
    if question.trim().is_empty() || question.len() > crate::server::MAX_TEXT {
        return Response::text(400, "ERR ask needs some text\n");
    }
    let Some(bridge) = context.bridge.get() else {
        return Response::text(503, "ERR the assistant is starting\n");
    };
    let Ok(mut bridge) = bridge.lock() else {
        return Response::text(500, "ERR assistant unavailable\n");
    };
    // The UI has passed its own checks; inside the bridge it presents the
    // frames' token, which never leaves this process.
    let bearer = context
        .settings
        .read()
        .ok()
        .and_then(|state| state.live.read().ok().and_then(|live| live.token.clone()))
        .map(|token| format!("Bearer {token}"));
    let call = |bridge: &mut crate::server::Bridge, path: &'static str, body: Vec<u8>| {
        bridge.dispatch(crate::server::Incoming {
            method: "POST",
            path,
            auth: bearer.as_deref(),
            lang: "ru-RU",
            voice: "",
            model: "",
            peer: "bridge-ui",
            trigger: "console",
            body,
        })
    };
    let answer = call(&mut bridge, "/llm", question.into_bytes());
    let spoken = (answer.status == 200).then(|| call(&mut bridge, "/tts", answer.body.clone()));
    let text = String::from_utf8_lossy(&answer.body).trim().to_owned();
    if answer.status != 200 {
        return Response::text(502, format!("{text}\n"));
    }
    let audio = spoken
        .filter(|spoken| spoken.status == 200)
        .map(|spoken| wav(&crate::ulaw::decode_to_pcm(&spoken.body)));
    Response::json(
        200,
        &serde_json::json!({
            "answer": text,
            "audio": audio.map(|wav| format!("data:audio/wav;base64,{}", frames::base64_bytes(&wav))),
        }),
    )
}

/// 16-bit mono PCM at the assistant's rate, as a WAV file.
fn wav(pcm: &[u8]) -> Vec<u8> {
    let rate = crate::backends::ASSISTANT_RATE;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

/// The voice assistant's tools: if a plugin's `[assistant]` phrases match the
/// question and the asking frame is known, run that plugin for that frame in
/// the background and return what the frame should say. The picture is pushed
/// once the frame has finished speaking (its panel is busy until then).
pub fn assistant_tool(context: &Arc<Context>, question: &str, peer: &str) -> Option<String> {
    let (found, _) = plugins::discover(&context.plugin_dirs);
    let intents: Vec<crate::intents::Intent<'_>> = found
        .iter()
        .filter_map(|plugin| {
            let assistant = plugin.manifest.assistant.as_ref()?;
            let mut phrases = vec![assistant.intent.as_str()];
            phrases.extend(assistant.phrases.iter().map(String::as_str));
            Some(crate::intents::Intent {
                plugin: &plugin.manifest.name,
                phrases,
            })
        })
        .collect();
    let (name, _) = crate::intents::find(question, &intents)?;
    let plugin = found
        .iter()
        .find(|plugin| plugin.manifest.name == name)?
        .clone();
    let frame = context
        .frames
        .list()
        .into_iter()
        .find(|frame| frame.host == peer)?;
    let mut given = context
        .scheduler
        .params_for(name, &frame.id)
        .unwrap_or_default();
    if let Some(param) = plugin
        .manifest
        .assistant
        .as_ref()
        .and_then(|assistant| assistant.text_param.clone())
    {
        given.insert(param, serde_json::Value::String(question.to_owned()));
    }
    let values = match plugins::validate(&plugin.manifest, &given) {
        Ok(values) => values,
        Err(_) => {
            return Some(format!(
                "Сначала настройте плагин «{}» на странице моста.",
                plugin.manifest.title
            ));
        }
    };
    let context = Arc::clone(context);
    let title = plugin.manifest.title.clone();
    // What the plugin says it shows («Показываю погоду: Эспоо.»), if it is
    // quick enough to say it in the same answer.
    let (said, heard) = std::sync::mpsc::channel::<String>();
    let title_for_card = title.clone();
    std::thread::spawn(move || {
        let target = plugins::Target::spectra(&frame.id, &frame.name);
        let key = (plugin.manifest.name.clone(), frame.id.clone());
        let state = context
            .plugin_state
            .lock()
            .ok()
            .and_then(|states| states.get(&key).cloned());
        let outcome = match plugins::run(&plugin, &values, &target, state.as_ref()) {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("  assistant tool {}: {error}", plugin.manifest.name);
                return;
            }
        };
        // The plugin's own words win («Не нашёл место …»); otherwise name what
        // the card shows when that adds something.
        let words = match (&outcome.say, &outcome.output) {
            (Some(say), _) => Some(say.clone()),
            (None, plugins::Output::Card(card))
                if !card.title().is_empty() && card.title() != title_for_card =>
            {
                Some(format!("Показываю: {title_for_card}, {}.", card.title()))
            }
            _ => None,
        };
        if let Some(words) = words {
            let _ = said.send(words);
        }
        if let (Some(state), Ok(mut states)) = (outcome.state.clone(), context.plugin_state.lock())
        {
            states.insert(key, state);
        }
        let image = match plugins::frame_for(&outcome, context.fonts.as_deref()) {
            Ok(Some(image)) => image,
            Ok(None) => return,
            Err(error) => {
                eprintln!("  assistant tool {}: {error}", plugin.manifest.name);
                return;
            }
        };
        // The answer stays on the frame for a while: the next scheduled screen
        // must not replace it while someone is still reading.
        context.scheduler.override_frame(
            &frame.id,
            image.clone(),
            std::time::Duration::from_secs(600),
        );
        // The frame is still speaking its answer and refuses a picture until
        // the turn is over; try for a minute.
        for _ in 0..12 {
            match frames::display(&frame.host, frame.port, &image) {
                Ok(message) => {
                    context
                        .scheduler
                        .mark_shown(&frame.id, crate::jobs::content_hash(&image));
                    eprintln!("  assistant tool {}: {message}", plugin.manifest.name);
                    return;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_secs(5)),
            }
        }
        eprintln!(
            "  assistant tool {}: the frame never took the picture",
            plugin.manifest.name
        );
    });
    // A quick plugin (weather: under a second) is waited for, so the answer can
    // say what is coming; a slow one (a generated picture) is not.
    match heard.recv_timeout(TOOL_WAIT) {
        Ok(words) if !words.trim().is_empty() => Some(words),
        _ => Some(format!("Показываю: {title}.")),
    }
}

/// How long the frame's `/llm` waits for a tool to say what it will show.
const TOOL_WAIT: std::time::Duration = std::time::Duration::from_secs(8);

/// Is this the frames' pull endpoint, `/frames/<id>/next`?
pub fn is_pull_path(path: &str) -> bool {
    path.strip_prefix("/frames/")
        .and_then(|rest| rest.strip_suffix("/next"))
        .is_some_and(|id| {
            !id.is_empty()
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// `GET /frames/<id>/next`: a sleeping frame asks for its picture on a timed
/// wake. Guarded by the frames' bearer token, like `/stt`. 200 with the
/// picture and its ETag, 304 when the frame already shows it, 204 when the
/// bridge has nothing for this frame (it then keeps its own slideshow).
pub fn pull(
    context: &Context,
    path: &str,
    authorization: Option<&str>,
    if_none_match: Option<&str>,
) -> Response {
    let token = context
        .settings
        .read()
        .ok()
        .and_then(|state| state.live.read().ok().and_then(|live| live.token.clone()));
    if let Some(token) = token
        && authorization != Some(format!("Bearer {token}").as_str())
    {
        return Response::text(401, "ERR bad or missing token\n");
    }
    let id = path
        .trim_start_matches("/frames/")
        .trim_end_matches("/next")
        .to_ascii_lowercase();
    let Some(render) = context.scheduler.next(&id, context.fonts.as_deref()) else {
        return Response::new(204, "text/plain; charset=utf-8", Vec::new());
    };
    let etag = format!("\"{:016x}\"", render.hash);
    if if_none_match.is_some_and(|given| given.trim() == etag) {
        context.scheduler.mark_shown(&id, render.hash);
        let mut response = Response::new(304, "text/plain; charset=utf-8", Vec::new());
        response.headers.push(("ETag", etag));
        return response;
    }
    // Handing it over counts as shown: the frame paints what it downloaded.
    context.scheduler.mark_shown(&id, render.hash);
    let mut response = Response::new(
        200,
        "application/octet-stream",
        render.image.as_ref().clone(),
    );
    response.headers.push(("ETag", etag));
    response
}

fn plugin_list(context: &Context) -> Response {
    let (found, problems) = plugins::discover(&context.plugin_dirs);
    let list: Vec<serde_json::Value> = found
        .iter()
        .map(|plugin| {
            let mut value = serde_json::to_value(&plugin.manifest).unwrap_or_default();
            value["dir"] = serde_json::Value::String(plugin.dir.display().to_string());
            value
        })
        .collect();
    Response::json(
        200,
        &serde_json::json!({
            "plugins": list,
            "problems": problems,
            "dirs": context.plugin_dirs.iter().map(|dir| dir.display().to_string()).collect::<Vec<_>>(),
        }),
    )
}

/// `/api/plugins/<name>/preview` and `/show`: run once with the form's values.
fn plugin_action(context: &Context, rest: &str, body: &[u8]) -> Response {
    let Some((name, action)) = rest.split_once('/') else {
        return Response::text(404, "ERR no such plugin action\n");
    };
    let (found, _) = plugins::discover(&context.plugin_dirs);
    let Some(plugin) = found
        .into_iter()
        .find(|plugin| plugin.manifest.name == name)
    else {
        return Response::text(404, "ERR no such plugin\n");
    };
    let request: serde_json::Value = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => return Response::text(400, format!("ERR request JSON: {error}\n")),
    };
    let empty = serde_json::Map::new();
    let given = request
        .get("params")
        .and_then(serde_json::Value::as_object)
        .unwrap_or(&empty);
    let values = match plugins::validate(&plugin.manifest, given) {
        Ok(values) => values,
        Err(error) => return Response::text(400, format!("ERR {error}\n")),
    };
    let frame_id = request
        .get("frame")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let frame = context
        .frames
        .list()
        .into_iter()
        .find(|frame| frame.id == frame_id);
    if action == "show" && frame.is_none() {
        return Response::text(400, "ERR choose a frame to show it on\n");
    }
    let target = plugins::Target::spectra(
        frame.as_ref().map_or("preview", |frame| frame.id.as_str()),
        frame
            .as_ref()
            .map_or("preview", |frame| frame.name.as_str()),
    );
    let key = (plugin.manifest.name.clone(), target.id.clone());
    let state = context
        .plugin_state
        .lock()
        .ok()
        .and_then(|states| states.get(&key).cloned());
    let outcome = match plugins::run(&plugin, &values, &target, state.as_ref()) {
        Ok(outcome) => outcome,
        Err(error) => return Response::text(422, format!("ERR {name}: {error}\n")),
    };
    if !outcome.log.trim().is_empty() {
        eprintln!("  plugin {name}: {}", outcome.log.trim());
    }
    if let (Some(state), Ok(mut states)) = (outcome.state.clone(), context.plugin_state.lock()) {
        states.insert(key, state);
    }
    let image = match plugins::frame_for(&outcome, context.fonts.as_deref()) {
        Ok(Some(image)) => image,
        Ok(None) => {
            return Response::text(200, format!("OK {name}: nothing new ({} ms)\n", outcome.ms));
        }
        Err(error) => return Response::text(422, format!("ERR {name}: {error}\n")),
    };
    match action {
        "preview" => match render::png(&render::unpack(&image), render::WIDTH, render::HEIGHT) {
            Ok(png) => {
                let mut response = Response::new(200, "image/png", png);
                response
                    .headers
                    .push(("X-Plugin-Ms", outcome.ms.to_string()));
                response
            }
            Err(error) => Response::text(500, format!("ERR png: {error}\n")),
        },
        "show" => {
            let frame = frame.expect("checked above");
            match frame.panel {
                Some(panel)
                    if usize::from(panel.width) * usize::from(panel.height) / 2 == image.len() =>
                {
                    match frames::display(&frame.host, frame.port, &image) {
                        Ok(message) => {
                            context.scheduler.override_frame(
                                &frame.id,
                                image.clone(),
                                std::time::Duration::from_secs(600),
                            );
                            Response::text(200, format!("{message}\n"))
                        }
                        Err(error) => Response::text(502, format!("ERR {error}\n")),
                    }
                }
                _ => Response::text(409, "ERR the frame's panel is not known or not 800x480\n"),
            }
        }
        _ => Response::text(404, "ERR no such plugin action\n"),
    }
}

fn frame_list(context: &Context) -> Response {
    Response::json(200, &serde_json::json!({"frames": context.frames.list()}))
}

/// The Frames tab's OpenDisplay control: saved settings plus the listener's
/// actual state, which may differ for a second after a change.
fn opendisplay_status(context: &Context) -> Response {
    let Ok(state) = context.settings.read() else {
        return Response::text(500, "ERR settings unavailable\n");
    };
    let settings = &state.settings;
    let running = context
        .opendisplay
        .lock()
        .map(|status| status.clone())
        .unwrap_or_default();
    let frame = context
        .frames
        .list()
        .into_iter()
        .find(|frame| frame.id == settings.opendisplay_frame);
    Response::json(
        200,
        &serde_json::json!({
            "enabled": settings.opendisplay_enabled,
            "frame": settings.opendisplay_frame,
            "frame_name": frame.map(|frame| frame.name),
            "name": settings.opendisplay_name,
            "addr": settings.opendisplay_addr,
            "running": running.running,
            "instance": running.instance,
            "port": running.port,
            "error": running.error,
        }),
    )
}

fn save_opendisplay(context: &Context, body: &[u8]) -> Response {
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Response::text(400, "ERR expected a JSON object\n");
    };
    let enabled = request
        .get("enabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let frame = request
        .get("frame")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim();
    let name = request
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim();
    if enabled && frame.is_empty() {
        return Response::text(400, "ERR choose a frame to expose\n");
    }
    let Ok(mut state) = context.settings.write() else {
        return Response::text(500, "ERR settings unavailable\n");
    };
    let change = serde_json::json!({
        "opendisplay_enabled": enabled,
        "opendisplay_frame": frame,
        "opendisplay_name": name,
    });
    let mut next = state.settings.clone();
    if let Err(error) = next.apply(&change, &state.pinned) {
        return Response::text(400, format!("ERR {error}\n"));
    }
    if let Err(error) = next.save(&state.path) {
        return Response::text(500, format!("ERR saving settings: {error}\n"));
    }
    state.settings = next;
    drop(state);
    opendisplay_status(context)
}

/// Browse and check every frame now (the background refresh does the same each
/// minute).
pub fn rescan(context: &Context) {
    let manual = context
        .settings
        .read()
        .map(|state| state.settings.frames.clone())
        .unwrap_or_default();
    context.frames.refresh(&manual);
}

fn change_frames(
    context: &Context,
    body: &[u8],
    change: impl FnOnce(&mut Settings, &serde_json::Value) -> Result<(), String>,
) -> Response {
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Response::text(400, "ERR expected a JSON object\n");
    };
    {
        let Ok(mut state) = context.settings.write() else {
            return Response::text(500, "ERR settings unavailable\n");
        };
        let mut next = state.settings.clone();
        if let Err(error) = change(&mut next, &request) {
            return Response::text(400, format!("ERR {error}\n"));
        }
        if let Err(error) = next.save(&state.path) {
            return Response::text(500, format!("ERR saving settings: {error}\n"));
        }
        state.settings = next;
    }
    rescan(context);
    frame_list(context)
}

fn add_frame(context: &Context, body: &[u8]) -> Response {
    change_frames(context, body, |settings, request| {
        let field = |key: &str| {
            request
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        };
        settings.add_frame(field("host"), field("name"), field("password"))
    })
}

fn remove_frame(context: &Context, body: &[u8]) -> Response {
    let host = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|request| {
            request
                .get("host")
                .and_then(|host| host.as_str().map(str::to_owned))
        })
        .unwrap_or_default();
    let response = change_frames(context, body, |settings, _| {
        if settings.remove_frame(&host) {
            Ok(())
        } else {
            Err(format!("{host} is not a frame added by hand"))
        }
    });
    context.frames.forget(&host);
    response
}

/// `/api/frames/<id>/screen.png`: the frame's screen as a PNG. It does not need
/// the frame to be awake or even discovered, so a screen can be checked while
/// the panel sleeps.
fn screen_png(context: &Context, path: &str) -> Response {
    let id = path
        .trim_start_matches("/api/frames/")
        .trim_end_matches("/screen.png")
        .trim_end_matches('/');
    match context.scheduler.composed(id, context.fonts.as_deref()) {
        Some(screen) => match render::png(
            &render::unpack(&screen.image),
            render::WIDTH,
            render::HEIGHT,
        ) {
            Ok(png) => {
                let mut response = Response::new(200, "image/png", png);
                response.headers.push(("X-Screen", screen.job.clone()));
                response
            }
            Err(error) => Response::text(500, format!("ERR png: {error}\n")),
        },
        None => Response::new(204, "text/plain; charset=utf-8", Vec::new()),
    }
}

/// The firmware settings, as the browser sees them, plus the release list.
fn firmware_settings(settings: &Settings) -> serde_json::Value {
    serde_json::json!({
        "repo": settings.firmware_repo,
        "channel": settings.firmware_channel,
        "check_h": settings.firmware_check_h,
        "index_url": settings.firmware_index_url,
        "token_set": !settings.firmware_token.is_empty(),
        "enabled": !settings.firmware_repo.is_empty() || !settings.firmware_index_url.is_empty(),
    })
}

fn firmware_status(context: &Context) -> Response {
    let Ok(state) = context.settings.read() else {
        return Response::text(500, "ERR settings unavailable\n");
    };
    let settings = state.settings.clone();
    drop(state);
    let mut document = firmware_settings(&settings);
    let store = context.firmware.to_json();
    document["store"] = store;
    // The list refreshes itself while somebody is looking at it, which is when a
    // stale list is worth fixing; nothing in the bridge starts a fetch at 3 a.m.
    if document["enabled"] == serde_json::Value::Bool(true)
        && context.firmware.due(settings.firmware_check_h)
    {
        let store = Arc::clone(&context.firmware);
        std::thread::spawn(move || {
            if let Ok(url) = crate::firmware::index_url(
                &settings.firmware_repo,
                &settings.firmware_channel,
                &settings.firmware_index_url,
            ) {
                let token = settings.firmware_token.clone();
                let result =
                    store.refresh(&url, &|url: &str| crate::firmware::http_fetch(url, &token));
                if let Err(error) = result {
                    eprintln!("ERR firmware index: {error}");
                }
            }
        });
    }
    Response::json(200, &document)
}

fn firmware_refresh(context: &Context) -> Response {
    let Ok(state) = context.settings.read() else {
        return Response::text(500, "ERR settings unavailable\n");
    };
    let settings = state.settings.clone();
    drop(state);
    let url = match crate::firmware::index_url(
        &settings.firmware_repo,
        &settings.firmware_channel,
        &settings.firmware_index_url,
    ) {
        Ok(url) => url,
        Err(error) => return Response::text(409, format!("ERR {error}\n")),
    };
    let token = settings.firmware_token.clone();
    match context
        .firmware
        .refresh(&url, &|url: &str| crate::firmware::http_fetch(url, &token))
    {
        Ok(index) => Response::json(
            200,
            &serde_json::json!({"fetched": index.releases.len(), "index": index}),
        ),
        Err(error) => Response::text(502, format!("ERR {error}\n")),
    }
}

/// `/api/frames/<id>/ota`: what the frame says about its two slots.
///
/// Proxied rather than read from the browser: the frame has no CORS and may have
/// a password. The answer is cached for [`crate::firmware::OTA_CACHE`], because a
/// page that polls the install state would otherwise ask the frame twice a second.
fn frame_ota(context: &Context, path: &str) -> Response {
    let id = path
        .trim_start_matches("/api/frames/")
        .trim_end_matches("/ota")
        .trim_end_matches('/');
    match read_ota(context, id) {
        Ok(value) => Response::json(200, &value),
        Err(response) => response,
    }
}

/// The frame's own `ota` answer, cached for [`crate::firmware::OTA_CACHE`].
fn read_ota(context: &Context, id: &str) -> Result<serde_json::Value, Response> {
    if let Some(cached) = context.firmware.ota_cached(id) {
        return Ok(cached);
    }
    let Some((host, _port, password)) = context.frames.target(id) else {
        return Err(Response::text(404, "ERR no such frame; scan again\n"));
    };
    let answer = match frames::portal(&host, &password, "GET", "/ota", &[]) {
        Ok(answer) => answer,
        // A frame that is asleep is the common case, so it is named; the reason
        // follows it, because a reboot, a wrong password and a wrong address
        // would otherwise all read as sleep.
        Err(error) => {
            return Err(Response::text(
                502,
                format!("ERR the frame is asleep or has no portal ({error}); it must be awake\n"),
            ));
        }
    };
    if answer.status != 200 {
        let text = String::from_utf8_lossy(&answer.body).trim().to_owned();
        return Err(Response::text(
            answer.status,
            format!("ERR the frame answered {}: {text}\n", answer.status),
        ));
    }
    let value: serde_json::Value = match serde_json::from_slice(&answer.body) {
        Ok(value) => value,
        Err(error) => {
            return Err(Response::text(
                502,
                format!("ERR the frame's ota answer is not JSON: {error}\n"),
            ));
        }
    };
    context.firmware.remember_ota(id, value.clone());
    Ok(value)
}

/// A frame as the installer sees it: the portal, with the timeout an upload needs.
struct PortalFrame {
    host: String,
    password: String,
}

impl crate::firmware::Frame for PortalFrame {
    fn ota(&self) -> Result<serde_json::Value, String> {
        let answer = frames::portal(&self.host, &self.password, "GET", "/ota", &[])?;
        if answer.status != 200 {
            return Err(format!("the frame answered {}", answer.status));
        }
        serde_json::from_slice(&answer.body)
            .map_err(|error| format!("the frame's ota answer is not JSON: {error}"))
    }

    fn write(&mut self, image: &[u8]) -> Result<String, String> {
        let answer = frames::portal_with(
            &self.host,
            &self.password,
            "POST",
            "/ota",
            image,
            frames::PORTAL_TIMEOUT_OTA,
        );
        match answer {
            Ok(answer) if answer.status == 200 => {
                Ok(String::from_utf8_lossy(&answer.body).trim().to_owned())
            }
            // A status other than 200 is the frame refusing the image, and its
            // sentence says why. A *transport* error after the body was sent is
            // the normal ending: the frame restarts as soon as it has written
            // the slot. Nothing is known yet, so the installer asks the frame.
            Ok(answer) => Err(String::from_utf8_lossy(&answer.body).trim().to_owned()),
            Err(_) => Ok(String::new()),
        }
    }
}

/// `POST /api/frames/<id>/firmware`: write one release to one frame.
fn frame_firmware_start(context: &Context, path: &str, body: &[u8]) -> Response {
    if body.len() > MAX_BODY {
        return Response::text(413, "ERR request too large\n");
    }
    let id = path
        .trim_start_matches("/api/frames/")
        .trim_end_matches("/firmware")
        .trim_end_matches('/');
    let request: serde_json::Value = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => return Response::text(400, format!("ERR request JSON: {error}\n")),
    };
    let version = request
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    if version.is_empty() {
        return Response::text(400, "ERR name the version to install\n");
    }
    // Writing flash is not something a page may do by accident: the button asks
    // twice, and the request carries the answer to the second question.
    if request.get("confirm") != Some(&serde_json::Value::Bool(true)) {
        return Response::text(400, "ERR installing firmware needs confirm: true\n");
    }
    let Some((host, _port, password)) = context.frames.target(id) else {
        return Response::text(404, "ERR no such frame; scan again\n");
    };
    let Some(index) = context.firmware.index() else {
        return Response::text(409, "ERR no release list yet; refresh it first\n");
    };
    let Some(release) = index.release(&version).cloned() else {
        return Response::text(
            404,
            format!("ERR the release list has no {version}; refresh it\n"),
        );
    };
    if context.firmware.installing(id) {
        return Response::text(
            409,
            format!("ERR {version} is already being installed on this frame\n"),
        );
    }
    let ota = match read_ota(context, id) {
        Ok(ota) => ota,
        // The read already says why (it could not connect, or the frame refused
        // `GET /ota`); flattening it to one sentence hid a reboot in progress.
        Err(reason) => return reason,
    };
    if let Err(error) = crate::firmware::check(&release, &ota) {
        return Response::text(409, format!("ERR {error}\n"));
    }
    let frame_io = Box::new(PortalFrame { host, password });
    match context.firmware.start(id, release, frame_io) {
        Ok(()) => Response::json(202, &serde_json::json!({"started": version})),
        Err(error) => Response::text(409, format!("ERR {error}\n")),
    }
}

/// `GET /api/frames/<id>/firmware`: what the install is doing.
fn frame_firmware_state(context: &Context, path: &str) -> Response {
    let id = path
        .trim_start_matches("/api/frames/")
        .trim_end_matches("/firmware")
        .trim_end_matches('/');
    match context.firmware.install(id) {
        Some(install) => Response::json(200, &serde_json::to_value(install).unwrap_or_default()),
        None => Response::json(200, &serde_json::json!({"phase": "idle"})),
    }
}

/// `/api/frames/<id>/<action>`: the portal proxy and OpenDisplay delivery.
fn frame_action(
    context: &Context,
    method: &str,
    rest: &str,
    query: &str,
    body: Vec<u8>,
) -> Response {
    let Some((id, action)) = rest.split_once('/') else {
        return Response::text(404, "ERR no such frame action\n");
    };
    let Some((host, port, password)) = context.frames.target(id) else {
        return Response::text(404, "ERR no such frame; scan again\n");
    };
    // The picture's path is passed on, so it may only hold what the frame's
    // own names do; anything else is refused rather than escaped.
    let path = match query.split('&').find_map(|pair| pair.strip_prefix("path=")) {
        Some(path)
            if path
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"%._-~/+".contains(&byte)) =>
        {
            Some(path)
        }
        Some(_) => return Response::text(400, "ERR path holds characters a frame name cannot\n"),
        None => None,
    };
    let forward = |method: &str, path: &str, body: &[u8]| -> Response {
        match frames::portal(&host, &password, method, path, body) {
            Ok(answer) => {
                let content_type = match answer.content_type.as_str() {
                    "application/json" => "application/json; charset=utf-8",
                    "application/octet-stream" => "application/octet-stream",
                    _ => "text/plain; charset=utf-8",
                };
                Response::new(answer.status, content_type, answer.body)
            }
            Err(error) => Response::text(502, format!("ERR {error}\n")),
        }
    };
    match (method, action) {
        ("GET", "gallery") => forward("GET", "/gallery", &[]),
        ("GET", "sensors") => forward("GET", "/sensors", &[]),
        ("POST", "screen") => {
            let panel = context
                .frames
                .list()
                .into_iter()
                .find(|frame| frame.id == id)
                .and_then(|frame| frame.panel);
            match panel {
                Some(panel)
                    if usize::from(panel.width) * usize::from(panel.height) / 2
                        == render::FRAME_BYTES =>
                {
                    let Some(screen) = context.scheduler.composed(id, context.fonts.as_deref())
                    else {
                        return Response::text(409, "ERR the screen has nothing to show yet\n");
                    };
                    match frames::display(&host, port, &screen.image) {
                        Ok(message) => {
                            context.scheduler.clear_override(id);
                            context.scheduler.mark_shown(id, screen.hash);
                            Response::text(200, format!("{message}\n"))
                        }
                        Err(error) => Response::text(502, format!("ERR {error}\n")),
                    }
                }
                _ => Response::text(409, "ERR the frame's panel is not known or not 800x480\n"),
            }
        }
        ("GET", "queued") => match context.scheduler.next(id, context.fonts.as_deref()) {
            Some(queued) => match render::png(
                &render::unpack(&queued.image),
                render::WIDTH,
                render::HEIGHT,
            ) {
                Ok(png) => {
                    let mut response = Response::new(200, "image/png", png);
                    response.headers.push(("X-Job", queued.job.clone()));
                    response
                }
                Err(error) => Response::text(500, format!("ERR png: {error}\n")),
            },
            None => Response::new(204, "text/plain; charset=utf-8", Vec::new()),
        },
        ("GET", "image") => match path {
            Some(path) => forward("GET", &format!("/image?path={path}"), &[]),
            None => Response::text(400, "ERR image needs ?path=\n"),
        },
        ("POST", "show") => match path {
            Some(path) => forward("POST", &format!("/show?path={path}"), &[]),
            None => Response::text(400, "ERR show needs ?path=\n"),
        },
        ("POST", "delete") => match path {
            Some(path) => forward("POST", &format!("/delete?path={path}"), &[]),
            None => Response::text(400, "ERR delete needs ?path=\n"),
        },
        ("POST", "upload") => forward("POST", "/upload", &body),
        ("POST", "card") => {
            let frame = match card_frame(context, &body) {
                Ok(frame) => frame,
                Err(response) => return response,
            };
            let panel = context
                .frames
                .list()
                .into_iter()
                .find(|frame| frame.id == id)
                .and_then(|frame| frame.panel);
            match panel {
                Some(panel)
                    if usize::from(panel.width) * usize::from(panel.height) / 2 == frame.len() =>
                {
                    match frames::display(&host, port, &frame) {
                        Ok(message) => {
                            context.scheduler.override_frame(
                                id,
                                frame.clone(),
                                std::time::Duration::from_secs(600),
                            );
                            Response::text(200, format!("{message}\n"))
                        }
                        Err(error) => Response::text(502, format!("ERR {error}\n")),
                    }
                }
                Some(_) => {
                    Response::text(409, "ERR cards are drawn for 800x480 panels only so far\n")
                }
                None => Response::text(409, "ERR the frame's panel is not known yet; scan again\n"),
            }
        }
        ("POST", "display") => {
            let expected = context
                .frames
                .list()
                .into_iter()
                .find(|frame| frame.id == id)
                .and_then(|frame| frame.panel)
                .map(|panel| usize::from(panel.width) * usize::from(panel.height) / 2);
            match expected {
                Some(expected) if body.len() != expected => Response::text(
                    400,
                    format!("ERR the panel takes {expected} bytes, got {}\n", body.len()),
                ),
                None => Response::text(409, "ERR the frame's panel is not known yet; scan again\n"),
                Some(_) => match frames::display(&host, port, &body) {
                    Ok(message) => {
                        context.scheduler.override_frame(
                            id,
                            body.clone(),
                            std::time::Duration::from_secs(600),
                        );
                        Response::text(200, format!("{message}\n"))
                    }
                    Err(error) => Response::text(502, format!("ERR {error}\n")),
                },
            }
        }
        _ => Response::text(404, "ERR no such frame action\n"),
    }
}

/// `Host` names the bridge the way a person on the LAN would.
pub fn local_host(host: &str, own: &[String], extra: &[String]) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        // [IPv6]:port
        rest.split_once(']').map_or(rest, |(address, _)| address)
    } else {
        match host.rsplit_once(':') {
            Some((name, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => name,
            _ => host,
        }
    };
    let name = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    if name.parse::<std::net::IpAddr>().is_ok() || !name.contains('.') {
        return true;
    }
    own.iter()
        .chain(extra)
        .any(|allowed| allowed.eq_ignore_ascii_case(&name))
        || [".local", ".lan", ".home", ".home.arpa", ".internal"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// This machine's names: the kernel's host name, and its short form.
pub fn own_hostnames() -> Vec<String> {
    let full = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let mut names = vec![full.clone()];
    if let Some((short, _)) = full.split_once('.') {
        names.push(short.to_owned());
    }
    names.retain(|name| !name.is_empty());
    names
}

/// Password from a `Basic` credential, whatever the user name.
fn basic_password(header: &str) -> Option<String> {
    let (scheme, encoded) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let text = String::from_utf8(base64_decode(encoded.trim())?).ok()?;
    let (_user, password) = text.split_once(':')?;
    Some(password.to_owned())
}

pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        Some(match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let mut decoded = Vec::with_capacity(text.len() * 3 / 4);
    let (mut accumulator, mut bits) = (0_u32, 0_u32);
    for &byte in text.as_bytes() {
        if byte == b'=' {
            break;
        }
        accumulator = (accumulator << 6) | value(byte)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    Some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(settings: Settings) -> (Context, tempdir::Dir) {
        let dir = tempdir::Dir::new();
        let live = Arc::new(RwLock::new(settings.live()));
        let state = SettingsState {
            settings,
            pinned: BTreeSet::new(),
            path: dir.path.join("bridge.toml"),
            live,
        };
        (
            Context {
                settings: Arc::new(RwLock::new(state)),
                history: Arc::new(Mutex::new(History::default())),
                frames: Arc::new(Registry::default()),
                opendisplay: Arc::new(Mutex::new(opendisplay::Status::default())),
                fonts: Fonts::load(None).ok().map(Arc::new),
                plugin_dirs: Vec::new(),
                plugin_state: Mutex::new(std::collections::BTreeMap::new()),
                scheduler: Arc::new(Scheduler::load(&dir.path.join("jobs.toml")).unwrap()),
                firmware: Arc::new(crate::firmware::Store::new(dir.path.join("firmware"))),
                bridge: std::sync::OnceLock::new(),
                started: Instant::now(),
                hostnames: vec!["honka.vda.li".to_owned()],
            },
            dir,
        )
    }

    /// A scratch directory under the target dir, removed on drop.
    mod tempdir {
        pub struct Dir {
            pub path: std::path::PathBuf,
        }
        impl Dir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicU32, Ordering};
                static NEXT: AtomicU32 = AtomicU32::new(0);
                let path = std::env::temp_dir().join(format!(
                    "easellet-bridge-ui-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self { path }
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }

    fn get<'a>(url: &'a str, host: &'a str) -> Request<'a> {
        Request {
            method: "GET",
            url,
            host: Some(host),
            authorization: None,
            marked: false,
            body: Vec::new(),
        }
    }

    #[test]
    fn the_firmware_route_says_what_it_knows_and_what_it_cannot() {
        let (context, _dir) = context(Settings::default());
        let answer = handle(get("/api/firmware", "192.168.50.93"), &context);
        assert_eq!(answer.status, 200);
        let document: serde_json::Value = serde_json::from_slice(&answer.body).unwrap();
        assert_eq!(document["enabled"], false);
        assert_eq!(document["channel"], "stable");
        assert_eq!(document["check_h"], 6);
        assert!(document["store"]["index"].is_null());
        assert_eq!(document["token_set"], false);

        // With no repository there is nothing to fetch, and the answer says so
        // instead of failing somewhere deeper.
        let mut request = get("/api/firmware/refresh", "192.168.50.93");
        request.method = "POST";
        request.marked = true;
        let answer = handle(request, &context);
        assert_eq!(answer.status, 409);
        assert!(String::from_utf8_lossy(&answer.body).contains("firmware_repo"));

        // A frame the registry does not know has no ota to proxy.
        let answer = handle(
            get("/api/frames/94a990caed46/ota", "192.168.50.93"),
            &context,
        );
        assert_eq!(answer.status, 404);
    }

    #[test]
    fn only_local_names_are_served() {
        let own = vec!["honka.vda.li".to_owned()];
        for host in [
            "192.168.50.93:8095",
            "[::1]:8095",
            "honka",
            "honka.vda.li:8095",
            "bridge.local",
        ] {
            assert!(local_host(host, &own, &[]), "{host}");
        }
        for host in ["evil.example", "honka.vda.li.evil.example", ""] {
            assert!(!local_host(host, &own, &[]), "{host}");
        }
        assert!(local_host(
            "frames.example.org",
            &own,
            &["frames.example.org".to_owned()]
        ));
        let (context, _dir) = context(Settings::default());
        assert_eq!(
            handle(get("/api/status", "evil.example"), &context).status,
            403
        );
        assert_eq!(
            handle(get("/api/status", "192.168.50.93"), &context).status,
            200
        );
    }

    #[test]
    fn a_post_without_the_header_is_refused() {
        let (context, _dir) = context(Settings::default());
        let body = br#"{"wake": "frame"}"#.to_vec();
        let mut request = Request {
            method: "POST",
            url: "/api/settings",
            host: Some("192.168.50.93"),
            authorization: None,
            marked: false,
            body: body.clone(),
        };
        assert_eq!(handle(request, &context).status, 403);
        request = Request {
            method: "POST",
            url: "/api/settings",
            host: Some("192.168.50.93"),
            authorization: None,
            marked: true,
            body,
        };
        let response = handle(request, &context);
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        // Applied live and saved.
        let state = context.settings.read().unwrap();
        assert_eq!(state.live.read().unwrap().wake, ["frame"]);
        assert!(
            std::fs::read_to_string(&state.path)
                .unwrap()
                .contains("frame")
        );
    }

    #[test]
    fn an_opendisplay_choice_is_saved_and_reported() {
        let (context, _dir) = context(Settings::default());
        let body = br#"{"enabled":true,"frame":"94a990caed46","name":"Kitchen"}"#.to_vec();
        let request = Request {
            method: "POST",
            url: "/api/opendisplay",
            host: Some("192.168.50.93"),
            authorization: None,
            marked: true,
            body,
        };
        let response = handle(request, &context);
        assert_eq!(response.status, 200);
        let document: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(document["enabled"], true);
        assert_eq!(document["frame"], "94a990caed46");
        assert_eq!(document["name"], "Kitchen");
        assert_eq!(document["running"], false);
        let state = context.settings.read().unwrap();
        assert!(state.settings.opendisplay_enabled);
        assert_eq!(state.settings.opendisplay_frame, "94a990caed46");
        assert_eq!(state.settings.opendisplay_name, "Kitchen");
        assert!(
            std::fs::read_to_string(&state.path)
                .unwrap()
                .contains("opendisplay_enabled = true")
        );
    }

    #[test]
    fn enabling_opendisplay_without_a_frame_is_refused() {
        let (context, _dir) = context(Settings::default());
        let request = Request {
            method: "POST",
            url: "/api/opendisplay",
            host: Some("192.168.50.93"),
            authorization: None,
            marked: true,
            body: br#"{"enabled":true,"frame":"","name":"Easellet"}"#.to_vec(),
        };
        let response = handle(request, &context);
        assert_eq!(response.status, 400);
        assert!(
            !context
                .settings
                .read()
                .unwrap()
                .settings
                .opendisplay_enabled
        );
    }

    #[test]
    fn the_password_guards_everything_and_secrets_stay_hidden() {
        let mut settings = Settings::default();
        settings.ui_password = "hunter2hunter2".to_owned();
        settings.token = "easellet-lan".to_owned();
        let (context, _dir) = context(settings);
        let response = handle(get("/api/settings", "192.168.50.93"), &context);
        assert_eq!(response.status, 401);
        assert!(
            response
                .headers
                .iter()
                .any(|(name, _)| *name == "WWW-Authenticate")
        );
        // btoa("x:hunter2hunter2")
        let request = Request {
            authorization: Some("Basic eDpodW50ZXIyaHVudGVyMg=="),
            ..get("/api/settings", "192.168.50.93")
        };
        let response = handle(request, &context);
        assert_eq!(response.status, 200);
        let body = String::from_utf8(response.body).unwrap();
        assert!(
            !body.contains("hunter2") && !body.contains("easellet-lan"),
            "{body}"
        );
    }

    #[test]
    fn frame_actions_need_a_known_frame_and_a_clean_path() {
        let (context, _dir) = context(Settings::default());
        let response = handle(
            get("/api/frames/0011223344/gallery", "192.168.50.93"),
            &context,
        );
        assert_eq!(response.status, 404);
        assert_eq!(body_limit("/api/frames/x/upload"), MAX_PICTURE);
        assert_eq!(body_limit("/api/settings"), MAX_BODY);
        let listed = handle(get("/api/frames", "192.168.50.93"), &context);
        assert_eq!(listed.status, 200);
        assert_eq!(String::from_utf8(listed.body).unwrap(), r#"{"frames":[]}"#);
    }

    #[test]
    fn a_card_previews_as_png() {
        let (context, _dir) = context(Settings::default());
        let request = Request {
            method: "POST",
            url: "/api/render/preview",
            host: Some("192.168.50.93"),
            authorization: None,
            marked: true,
            body: r#"{"layout":"notice","title":"Привет","body":"Это рамка."}"#
                .as_bytes()
                .to_vec(),
        };
        let response = handle(request, &context);
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "image/png");
        let bad = Request {
            method: "POST",
            url: "/api/render/preview",
            host: Some("192.168.50.93"),
            authorization: None,
            marked: true,
            body: br#"{"layout":"poster"}"#.to_vec(),
        };
        assert_eq!(handle(bad, &context).status, 400);
    }

    #[test]
    fn the_pull_endpoint_wants_the_token_and_answers_etags() {
        let mut settings = Settings::default();
        settings.token = "easellet-lan".to_owned();
        let (context, _dir) = context(settings);
        assert!(is_pull_path("/frames/94a990caed46/next"));
        assert!(!is_pull_path("/frames/../next") && !is_pull_path("/frames//next"));
        let path = "/frames/94a990caed46/next";
        assert_eq!(pull(&context, path, None, None).status, 401);
        let auth = Some("Bearer easellet-lan");
        assert_eq!(
            pull(&context, path, auth, None).status,
            204,
            "nothing queued"
        );
        {
            let card: crate::render::Card =
                serde_json::from_str(r#"{"layout":"notice","title":"test"}"#).unwrap();
            context.scheduler.queue_for_test(
                "94a990caed46",
                "test",
                crate::compose::Panel::Card(card),
                0,
            );
        }
        let first = pull(&context, path, auth, None);
        assert_eq!(first.status, 200);
        assert_eq!(first.body.len(), render::FRAME_BYTES);
        let etag = first
            .headers
            .iter()
            .find(|(name, _)| *name == "ETag")
            .unwrap()
            .1
            .clone();
        assert_eq!(pull(&context, path, auth, Some(&etag)).status, 304);
    }

    #[test]
    fn a_screen_preview_does_not_need_an_awake_frame() {
        let (context, _dir) = context(Settings::default());
        let path = "/api/frames/94a990caed46/screen.png";
        let empty = handle(get(path, "192.168.50.93"), &context);
        assert_eq!(empty.status, 204, "no tiles yet");

        let card: crate::render::Card =
            serde_json::from_str(r#"{"layout":"notice","title":"test"}"#).unwrap();
        context.scheduler.queue_for_test(
            "94a990caed46",
            "test",
            crate::compose::Panel::Card(card),
            0,
        );
        let rendered = handle(get(path, "192.168.50.93"), &context);
        assert_eq!(rendered.status, 200, "the frame is not in the registry");
        assert_eq!(&rendered.body[1..4], b"PNG");
    }

    #[test]
    fn screens_are_listed_and_deleted_from_the_ui() {
        let (context, _dir) = context(Settings::default());
        fn post<'a>(url: &'a str, body: &'a str) -> Request<'a> {
            Request {
                method: "POST",
                url,
                host: Some("192.168.50.93"),
                authorization: None,
                marked: true,
                body: body.as_bytes().to_vec(),
            }
        }

        let empty = handle(get("/api/screens", "192.168.50.93"), &context);
        assert_eq!(empty.status, 200);
        let document: serde_json::Value = serde_json::from_slice(&empty.body).unwrap();
        assert_eq!(document["screens"], serde_json::json!([]));

        let refused = handle(post("/api/screens/save", r#"{"frame":""}"#), &context);
        assert_eq!(refused.status, 400);
        assert!(String::from_utf8_lossy(&refused.body).contains("frame"));
        let missing = handle(post("/api/screens/delete", r#"{"frame":"f"}"#), &context);
        assert_eq!(missing.status, 400);

        context.scheduler.set_screen_for_test(crate::jobs::Screen {
            frame: "f".to_owned(),
            preset: "single".to_owned(),
            slots: Vec::new(),
        });
        let listed = handle(get("/api/screens", "192.168.50.93"), &context);
        let document: serde_json::Value = serde_json::from_slice(&listed.body).unwrap();
        assert_eq!(document["screens"][0]["frame"], "f");

        let deleted = handle(post("/api/screens/delete", r#"{"frame":"f"}"#), &context);
        assert_eq!(deleted.status, 200);
        let empty = handle(get("/api/screens", "192.168.50.93"), &context);
        let document: serde_json::Value = serde_json::from_slice(&empty.body).unwrap();
        assert_eq!(document["screens"], serde_json::json!([]));
    }

    #[test]
    fn installing_firmware_asks_twice_and_needs_a_list_and_a_frame() {
        let (context, _dir) = context(Settings::default());
        let ask = |body: &'static str| Request {
            method: "POST",
            url: "/api/frames/f/firmware",
            host: Some("192.168.50.93"),
            authorization: None,
            marked: true,
            body: body.as_bytes().to_vec(),
        };
        // No `confirm`: the page asks twice, and the second answer is the one
        // that reaches here.
        assert_eq!(handle(ask(r#"{"version":"0.2.0"}"#), &context).status, 400);

        let confirmed = ask(r#"{"version":"0.2.0","confirm":true}"#);
        // The frame is not in the registry, so this is not a frame the bridge
        // can write to, whatever the release list says.
        assert_eq!(handle(confirmed, &context).status, 404);

        let idle = handle(get("/api/frames/f/firmware", "192.168.50.93"), &context);
        assert_eq!(idle.status, 200);
        let document: serde_json::Value = serde_json::from_slice(&idle.body).unwrap();
        assert_eq!(document["phase"], "idle");
    }

    #[test]
    fn the_page_marks_every_post_and_ships_the_converter() {
        assert!(APP.contains(&format!("\"{POST_HEADER}\": \"1\"")));
        assert!(EL6.contains("export function packImage("));
        assert!(PAGE.contains("app.js"));
    }

    /// Every text the page shows exists in every language.
    ///
    /// The dictionaries live in the page itself, and a key added to one language
    /// and not the other shows up as the key's name in the interface — the sort of
    /// bug nobody sees when they only read their own language.
    #[test]
    fn every_string_is_written_in_both_languages() {
        fn keys(block: &str) -> std::collections::BTreeSet<String> {
            let mut found = std::collections::BTreeSet::new();
            for line in block.lines() {
                let line = line.trim();
                let Some(rest) = line.strip_prefix('"') else {
                    continue;
                };
                let Some((key, _)) = rest.split_once("\": ") else {
                    continue;
                };
                found.insert(key.to_owned());
            }
            found
        }
        fn block(language: &str) -> &'static str {
            let start = PAGE
                .find(&format!("  {language}: {{"))
                .unwrap_or_else(|| panic!("no {language} dictionary"));
            let rest = &PAGE[start..];
            let end = rest.find("\n  },").unwrap_or(rest.len());
            &rest[..end]
        }
        let english = keys(block("en"));
        let russian = keys(block("ru"));
        assert!(english.len() > 200, "only {} keys", english.len());
        let missing_in_russian: Vec<&String> = english.difference(&russian).collect();
        let missing_in_english: Vec<&String> = russian.difference(&english).collect();
        assert!(
            missing_in_russian.is_empty() && missing_in_english.is_empty(),
            "missing in ru: {missing_in_russian:?}; missing in en: {missing_in_english:?}"
        );
    }
}
