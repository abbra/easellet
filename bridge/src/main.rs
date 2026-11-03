//! easellet-bridge: the LAN service the frame talks to.
//!
//! The device knows nothing about models, voices or keys. It uploads µ-law, gets text
//! back, and asks for speech. Everything else happens here, where there is a real CPU
//! and a real filesystem. See `README.md` for the wire format and `../TODO.md` for why
//! the pipeline lives here rather than in the cloud.

mod arithmetic;
mod backends;
mod commands;
mod compose;
mod frames;
mod history;
mod intents;
mod jobs;
mod opendisplay;
mod plugins;
mod render;
mod server;
mod settings;
mod ui;
mod ulaw;

#[cfg(feature = "whisper")]
mod whisper;

use backends::{ASSISTANT_RATE, CommandBackend, Llm, Stt, StubLlm, StubStt, StubTts, Tts};
use history::History;
use server::{Bridge, MAX_TEXT, MAX_UTTERANCE};
use settings::Settings;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

const USAGE: &str = "\
usage: easellet-bridge [options]

  --config <path>      settings file, default ~/.config/easellet-bridge/bridge.toml;
                       created from the effective settings if it does not exist
  --addr <host:port>   listen address, default 0.0.0.0:8080
  --token <token>      require 'Authorization: Bearer <token>' from frames
  --stt <backend>      stub (default) | whisper:<tiny|base|small|medium> |
                       cmd:<program> [args...]   (raw 16-bit mono 16 kHz PCM on stdin)
  --llm <backend>      stub (default) | cmd:<program> [args...]  (prompt on stdin)
  --tts <backend>      stub (default) | cmd:<program> [args...]  (text on stdin,
                       raw 16-bit mono 16 kHz PCM on stdout)
  --wake <stems>       comma-separated stems a voice-triggered transcript must
                       contain, e.g. 'рамк,фоторам,frame' (also EASEL_WAKE)

Precedence: the file, then EASEL_WAKE / EASEL_LLM_URL / EASEL_LLM_SYSTEM, then flags. A
value set by a flag or variable wins for this run and is read-only in the web UI
(http://<addr>/); everything else is edited there and saved to the file.

The stubs exist so the frame's whole loop can be tested before any model is chosen:
'stub' recognition reports the duration and peak it received, 'stub' synthesis answers
with a tone rather than silence.
";

/// Worker threads. Recognition holds the bridge lock for seconds; the others keep
/// the web UI answering meanwhile.
const WORKERS: usize = 4;

fn main() {
    if let Err(error) = run() {
        eprintln!("ERR {error}");
        std::process::exit(1);
    }
}

/// Where plugins are looked for: the user's own directory next to the settings
/// file, then the ones shipped in `bridge/plugins` (found from the binary's
/// place in `bridge/target/<profile>/`).
fn plugin_dirs(settings_path: &std::path::Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(parent) = settings_path.parent() {
        dirs.push(parent.join("plugins"));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(bridge) = exe
            .parent()
            .and_then(|dir| dir.parent())
            .and_then(|dir| dir.parent())
    {
        dirs.push(bridge.join("plugins"));
    }
    dirs
}

/// `easellet-bridge plugin list|run ...`: try plugins without a frame.
fn plugin_command(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let dirs = plugin_dirs(&settings::default_path());
    let (found, problems) = plugins::discover(&dirs);
    for problem in &problems {
        eprintln!("skipped {problem}");
    }
    match args.next().as_deref() {
        Some("list") | None => {
            for plugin in &found {
                println!(
                    "{:<16} {}  ({})",
                    plugin.manifest.name,
                    plugin.manifest.title,
                    plugin.dir.display()
                );
            }
            Ok(())
        }
        Some("run") => {
            let name = args
                .next()
                .ok_or("plugin run <name> [--param k=v]... [--preview out.png] [--el6 out.el6]")?;
            let plugin = found
                .iter()
                .find(|plugin| plugin.manifest.name == name)
                .ok_or_else(|| format!("no plugin {name} in {dirs:?}"))?;
            let mut given = serde_json::Map::new();
            let (mut preview, mut el6) = (None, None);
            while let Some(arg) = args.next() {
                let value = args.next().ok_or_else(|| format!("{arg} needs a value"))?;
                match arg.as_str() {
                    "--param" => {
                        let (key, value) =
                            value.split_once('=').ok_or("--param takes key=value")?;
                        given.insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
                    }
                    "--preview" => preview = Some(value),
                    "--el6" => el6 = Some(value),
                    other => return Err(format!("unknown option {other}")),
                }
            }
            let values = plugins::validate(&plugin.manifest, &given)?;
            let outcome = plugins::run(
                plugin,
                &values,
                &plugins::Target::spectra("cli", "cli"),
                None,
            )?;
            if !outcome.log.trim().is_empty() {
                eprintln!("{}", outcome.log.trim());
            }
            println!(
                "{name}: {:?} in {} ms, state={:?}, next_run_s={:?}",
                kind(&outcome.output),
                outcome.ms,
                outcome.state,
                outcome.next_run_s
            );
            let fonts = render::Fonts::load(None).ok();
            let Some(image) = plugins::frame_for(&outcome, fonts.as_ref())? else {
                println!("nothing new: no picture");
                return Ok(());
            };
            if let Some(path) = preview {
                let png = render::png(&render::unpack(&image), render::WIDTH, render::HEIGHT)?;
                std::fs::write(&path, png).map_err(|error| format!("{path}: {error}"))?;
                println!("wrote {path}");
            }
            if let Some(path) = el6 {
                std::fs::write(&path, &image).map_err(|error| format!("{path}: {error}"))?;
                println!("wrote {path}");
            }
            Ok(())
        }
        Some(other) => Err(format!("plugin {other}: expected list or run")),
    }
}

fn kind(output: &plugins::Output) -> &'static str {
    match output {
        plugins::Output::Card(_) => "card",
        plugins::Output::Image { .. } => "image",
        plugins::Output::None => "none",
    }
}

fn run() -> Result<(), String> {
    if std::env::args().nth(1).as_deref() == Some("plugin") {
        return plugin_command(std::env::args().skip(2));
    }
    let mut path = settings::default_path();
    let mut overrides: Vec<(&'static str, String)> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{arg} needs a value\n{USAGE}"))
        };
        match arg.as_str() {
            "--config" => path = PathBuf::from(value()?),
            "--addr" => overrides.push(("addr", value()?)),
            "--token" => overrides.push(("token", value()?)),
            "--stt" => overrides.push(("stt", value()?)),
            "--llm" => overrides.push(("llm", value()?)),
            "--tts" => overrides.push(("tts", value()?)),
            "--wake" => overrides.push(("wake", value()?)),
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    // Variables first so a flag for the same key wins over them.
    for (key, variable) in [
        ("wake", "EASEL_WAKE"),
        ("llm_url", "EASEL_LLM_URL"),
        ("llm_system", "EASEL_LLM_SYSTEM"),
    ] {
        if let Ok(value) = std::env::var(variable)
            && !value.is_empty()
            && !overrides.iter().any(|(existing, _)| *existing == key)
        {
            overrides.insert(0, (key, value));
        }
    }

    let (mut settings, existed) = Settings::load(&path)?;
    let mut pinned = BTreeSet::new();
    for (key, value) in &overrides {
        // The same validation as a change from the UI.
        let change = serde_json::json!({ (*key): value });
        settings
            .apply(&change, &BTreeSet::new())
            .map_err(|error| format!("{key}: {error}"))?;
        // A file written from these very values this run has nothing to protect
        // them from; only an existing file's values are overridden and pinned.
        if existed {
            pinned.insert((*key).to_owned());
        }
    }
    if existed {
        eprintln!("settings: {} (pinned this run: {pinned:?})", path.display());
    } else {
        settings.save(&path)?;
        eprintln!("settings: created {} from the command line", path.display());
    }

    let stt: Box<dyn Stt> = if settings.stt == "stub" {
        Box::new(StubStt)
    } else if let Some(model) = settings.stt.strip_prefix("whisper:") {
        #[cfg(feature = "whisper")]
        {
            Box::new(whisper::WhisperStt::new(model)?)
        }
        #[cfg(not(feature = "whisper"))]
        {
            let _ = model;
            return Err(format!(
                "stt {}: rebuild with --features whisper to use rwhisper",
                settings.stt
            ));
        }
    } else {
        Box::new(CommandBackend::parse(&settings.stt)?)
    };
    let llm: Box<dyn Llm> = if settings.llm == "stub" {
        Box::new(StubLlm)
    } else {
        Box::new(CommandBackend::parse(&settings.llm)?)
    };
    let tts: Box<dyn Tts> = if settings.tts == "stub" {
        Box::new(StubTts)
    } else {
        Box::new(CommandBackend::parse(&settings.tts)?)
    };

    eprintln!(
        "easellet-bridge: audio {ASSISTANT_RATE} Hz mono, limits {} / {} bytes",
        MAX_TEXT, MAX_UTTERANCE
    );
    let live = settings.live();
    if live.wake.is_empty() {
        eprintln!("  wake: off - every transcript is answered");
    } else {
        eprintln!(
            "  wake: only transcripts containing {:?} are answered",
            live.wake
        );
    }
    let addr = settings.addr.clone();
    let live = Arc::new(RwLock::new(live));
    let history = Arc::new(Mutex::new(History::default()));
    let dirs = plugin_dirs(&path);
    let scheduler = Arc::new(jobs::Scheduler::load(&path.with_file_name("jobs.toml"))?);
    let context = ui::Context {
        settings: Arc::new(RwLock::new(ui::SettingsState {
            settings,
            pinned,
            path,
            live: Arc::clone(&live),
        })),
        history: Arc::clone(&history),
        frames: Arc::new(frames::Registry::default()),
        opendisplay: Arc::new(Mutex::new(opendisplay::Status::default())),
        // Cards need a font; without one the rest of the bridge still works.
        fonts: match render::Fonts::load(None) {
            Ok(fonts) => Some(Arc::new(fonts)),
            Err(error) => {
                eprintln!("  render: {error}");
                None
            }
        },
        plugin_dirs: dirs,
        plugin_state: Mutex::new(std::collections::BTreeMap::new()),
        scheduler,
        bridge: std::sync::OnceLock::new(),
        started: Instant::now(),
        hostnames: ui::own_hostnames(),
    };
    let context = Arc::new(context);
    let bridge = Bridge {
        stt,
        llm,
        tts,
        token: None,
        last_lang: None,
        wake: Vec::new(),
        addressed_at: None,
        live: Some(Arc::clone(&live)),
        history: Some(Arc::clone(&history)),
        last_heard: None,
        tools: Some(Arc::new({
            let context = Arc::clone(&context);
            move |question: &str, peer: &str| ui::assistant_tool(&context, question, peer)
        })),
    };
    serve(bridge, context, &addr)
}

fn serve(bridge: Bridge, context: Arc<ui::Context>, addr: &str) -> Result<(), String> {
    // The listener is bound here rather than by `Server::http`: with
    // `--addr 127.0.0.1:0` the kernel picks a free port, and only the listener
    // knows which one it picked. `tools/bridge-smoke.py` starts the real server
    // that way, so the line below has to name the port it is really on.
    let listener =
        std::net::TcpListener::bind(addr).map_err(|error| format!("listen on {addr}: {error}"))?;
    let bound = listener
        .local_addr()
        .map(|socket| socket.to_string())
        .unwrap_or_else(|_| addr.to_string());
    let server = Arc::new(
        tiny_http::Server::from_listener(listener, None)
            .map_err(|error| format!("listen on {addr}: {error:?}"))?,
    );
    eprintln!("listening on http://{bound}  (/stt /llm /tts /health, web UI at /)");
    // The frame endpoints share one bridge: recognition is CPU-bound and the
    // frame sends one request at a time, so they are served in order. The UI
    // does not take this lock and stays responsive during a Whisper run.
    let bridge = Arc::new(Mutex::new(bridge));
    let _ = context.bridge.set(Arc::clone(&bridge));
    // Frames: browse and check them now and every minute, off the request path.
    {
        let context = Arc::clone(&context);
        std::thread::spawn(move || {
            loop {
                ui::rescan(&context);
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        });
    }
    // OpenDisplay: expose the selected frame while the settings ask for it.
    {
        let settings = Arc::clone(&context.settings);
        let frames = Arc::clone(&context.frames);
        let status = Arc::clone(&context.opendisplay);
        std::thread::spawn(move || opendisplay::supervise(settings, frames, status));
    }
    // Jobs: run what is due every half minute, after the first frame scan.
    {
        let context = Arc::clone(&context);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(10));
            loop {
                context.scheduler.tick(&context);
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        });
    }
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let (server, bridge, context) = (
                Arc::clone(&server),
                Arc::clone(&bridge),
                Arc::clone(&context),
            );
            std::thread::spawn(move || {
                for request in server.incoming_requests() {
                    handle(request, &bridge, &context);
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

fn handle(mut request: tiny_http::Request, bridge: &Mutex<Bridge>, context: &ui::Context) {
    let started = Instant::now();
    let method = request.method().as_str().to_owned();
    let url = request.url().to_owned();
    let path = url.split('?').next().unwrap_or("").to_owned();
    let peer = request
        .remote_addr()
        .map(|address| address.ip().to_string())
        .unwrap_or_default();
    // Collected once, up front: `header.field.equiv` needs a `&'static str` for
    // the name, and reading the body later needs `request` mutably.
    let header = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|entry| entry.field.equiv(name))
            .map(|entry| entry.value.as_str().to_owned())
    };
    let auth = header("Authorization");

    if ui::is_pull_path(&path) && method == "GET" {
        let reply = ui::pull(
            context,
            &path,
            auth.as_deref(),
            header("If-None-Match").as_deref(),
        );
        let status = reply.status;
        let mut response = tiny_http::Response::from_data(reply.body).with_status_code(status);
        response.add_header(content_type(reply.content_type));
        for (name, value) in reply.headers {
            if let Ok(header) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
                response.add_header(header);
            }
        }
        let _ = request.respond(response);
        eprintln!(
            "{method} {path} -> {status} {peer} {} ms",
            started.elapsed().as_millis()
        );
        return;
    }
    if ui::is_ui_path(&path) {
        let host = header("Host");
        let marked = header(ui::POST_HEADER).is_some();
        let mut body = Vec::new();
        if let Err(error) = request
            .as_reader()
            .take(ui::body_limit(&path) as u64 + 1)
            .read_to_end(&mut body)
        {
            eprintln!("ERR read {path}: {error}");
            return;
        }
        let reply = ui::handle(
            ui::Request {
                method: &method,
                url: &url,
                host: host.as_deref(),
                authorization: auth.as_deref(),
                marked,
                body,
            },
            context,
        );
        let status = reply.status;
        let mut response = tiny_http::Response::from_data(reply.body).with_status_code(status);
        response.add_header(content_type(reply.content_type));
        for (name, value) in reply.headers {
            if let Ok(header) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
                response.add_header(header);
            }
        }
        let _ = request.respond(response);
        // API polling every few seconds would drown the log; failures still show.
        if status >= 400 || !path.starts_with("/api/") {
            eprintln!(
                "{method} {path} -> {status} {} ms",
                started.elapsed().as_millis()
            );
        }
        return;
    }

    let lang = header("X-Lang").unwrap_or_else(|| "ru-RU".to_owned());
    let voice = header("X-Voice").unwrap_or_default();
    let model = header("X-Model").unwrap_or_default();
    let trigger = header("X-Trigger").unwrap_or_default();
    let limit = if path == "/stt" {
        MAX_UTTERANCE
    } else {
        MAX_TEXT
    };
    let mut body = Vec::new();
    if let Err(error) = request
        .as_reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut body)
    {
        eprintln!("ERR read {path}: {error}");
        return;
    }
    let reply = match bridge.lock() {
        Ok(mut bridge) => bridge.dispatch(server::Incoming {
            method: &method,
            path: &path,
            auth: auth.as_deref(),
            lang: &lang,
            voice: &voice,
            model: &model,
            peer: &peer,
            trigger: &trigger,
            body,
        }),
        Err(_) => {
            eprintln!("ERR bridge state poisoned by an earlier panic");
            return;
        }
    };
    let bytes_in = reply.bytes_in;
    let bytes_out = reply.body.len();
    let status = reply.status;
    let mut response = tiny_http::Response::from_data(reply.body).with_status_code(status);
    response.add_header(content_type(reply.content_type));
    for (name, value) in reply.headers {
        if let Ok(header) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response.add_header(header);
        }
    }
    let _ = request.respond(response);
    eprintln!(
        "{method} {path} -> {status} in={bytes_in} out={bytes_out} {} ms",
        started.elapsed().as_millis()
    );
}

fn content_type(value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(&b"Content-Type"[..], value.as_bytes()).expect("static header")
}
