//! A small HTTP/1.1 server that accepts pictures from a browser.
//!
//! The frame already knows how to draw a raw `.el6`; what it lacks is a way for
//! a person to put one on the card without removing it. That is all this server
//! does, and the interesting decisions are about where the work happens:
//!
//! - **The browser converts.** `web/index.html` draws the picture into an
//!   800x480 canvas, runs Floyd-Steinberg against the panel's six colours and
//!   posts 192 000 bytes. The frame therefore decodes nothing: no JPEG, no PNG,
//!   no EXIF, no 36 MB RGB buffer for a phone photo. `tools/el6.mjs` is the same
//!   module the host converter is tested against, and it is served from the file
//!   the gate checks, not a copy.
//! - **The card and the panel keep one owner each.** This thread parses HTTP and
//!   validates the frame; the write and the refresh are handed to the main loop
//!   over a channel, because the SD card and the panel are touched there and
//!   nowhere else. A POST therefore blocks until the loop answers, which is fast
//!   (a file write) because the ~31 s refresh happens in the paint worker after.
//! - **Nothing from the request reaches the filesystem.** Routes are fixed
//!   strings and the file name is chosen by the main loop, so a path in a URL
//!   cannot escape the card root even if it is a traversal.
//!
//! - **Every POST carries [`POST_HEADER`].** A web page on another site can
//!   make the browser send a form or a raw body here without asking anyone, and
//!   the browser would attach a cached password too. A custom header is
//!   different: a cross-site page can only add one after a CORS preflight, and
//!   this server never answers one, so a POST without the header is refused.
//!   That keeps a stranger's page from rewriting `ai.url` (and with it where the
//!   microphone and `ai.key` go), deleting pictures or clearing the password.
//!
//! - **Only local names in `Host`.** DNS rebinding points an attacker's own
//!   domain at the frame, after which the browser treats the frame as that
//!   site and lets its page read the answers. The `Host` header still carries
//!   the attacker's name, so anything but an IPv4 address or a local name
//!   (no dot, or `.local`, `.lan`, `.home`, `.home.arpa`, `.internal`) is refused.
//!
//! Deliberately plain: one connection at a time, `Connection: close`, no
//! keep-alive, no chunked bodies, no TLS. The station is a photo frame on a home
//! LAN.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// Default port. 80 so the page is reachable as `http://<frame>/` with no port
/// to remember; the bridge's own port lives on another machine.
pub const PORT: u16 = 80;

/// Bytes in one raw frame: 800x480, two 4bpp pixels per byte.
pub const FRAME_BYTES: usize = 192_000;

/// Headers stop here. A browser sends well under 2 KB; the bound exists so a
/// client cannot make the frame allocate on a whim.
const MAX_HEAD: usize = 8 * 1024;

/// Largest body accepted. One frame, exactly: anything bigger is not a picture
/// this panel can show, and anything smaller would paint a partial image.
const MAX_BODY: usize = FRAME_BYTES;

/// How long a read may stall before the connection is abandoned.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a whole request may take to arrive, head and body. The server takes
/// one connection at a time, so without this a client sending a byte every few
/// seconds would hold the page for hours; a real upload is 192 KB on a LAN.
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);

/// A firmware upload gets its own allowance: 1 MB arrives in a second on a LAN,
/// but every 4 KB of it waits for a flash write, and erasing a slot before the
/// write is the slowest thing this frame does on purpose.
const OTA_DEADLINE: Duration = Duration::from_secs(180);

/// Header the portal adds to every POST; its value is not checked, only that a
/// page could set it, which a cross-site page cannot without a preflight.
pub const POST_HEADER: &str = "X-Easellet";

/// Largest settings form accepted. The portal sends a dozen short fields.
const MAX_FORM: usize = 4 * 1024;

/// How long a POST waits for the main loop to write the file. The refresh is
/// not part of this: it is queued and answered separately.
const UPLOAD_WAIT: Duration = Duration::from_secs(30);

/// Accept-loop tick. Non-blocking accept plus a short sleep, so `stop` is
/// honoured promptly instead of waiting for the next client.
const TICK: Duration = Duration::from_millis(25);

/// What `GET /status` advertises. A route missing here is a route nobody finds,
/// so the page's own calls are checked against it in the tests.
const ROUTES: &str =
    "routes=/ /el6.mjs /gallery /image /upload /show /delete /settings /sensors /ota /status";

const PAGE: &str = include_str!("../web/index.html");
const CONVERTER: &str = include_str!("../tools/el6.mjs");

/// One picture the portal may show, as the slideshow sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Card-relative path, exactly the string the console prints.
    pub name: String,
    pub bytes: u64,
}

/// Work for the main loop. The sender of `reply` is the HTTP thread waiting on
/// the answer, so whatever the loop says becomes the response body.
pub enum Job {
    Upload {
        frame: Vec<u8>,
        reply: Sender<Result<String, String>>,
    },
    /// List the slideshow directory: `(directory, pictures)`.
    List {
        reply: Sender<Result<(String, Vec<Entry>), String>>,
    },
    /// One picture, decoded to a raw frame whatever its format on the card.
    Image {
        path: String,
        reply: Sender<Result<Vec<u8>, String>>,
    },
    /// Paint an existing picture now, without copying it.
    Show {
        path: String,
        reply: Sender<Result<String, String>>,
    },
    /// Remove a picture from the card. Refused outside the writable directory.
    Delete {
        path: String,
        reply: Sender<Result<String, String>>,
    },
    /// The current settings as JSON, secrets reduced to `true`/`false`.
    Settings { reply: Sender<String> },
    /// Battery, ambient and chip temperature as JSON, from the last reading.
    Sensors { reply: Sender<String> },
    /// Form fields to apply and persist. Answer is the human summary.
    SaveSettings {
        fields: Vec<(String, String)>,
        reply: Sender<Result<String, String>>,
    },
}

/// What the portal needs from a firmware updater, handed in by `main` rather
/// than called directly: this module is host-tested on its own and knows nothing
/// about ESP-IDF (`src/ota.rs` owns that).
pub trait Firmware: Send {
    /// `GET /ota`: the state as the JSON document the page reads.
    fn info(&self) -> Result<String, String>;
    /// `POST /ota`: write `length` bytes from `read` into the inactive slot. The
    /// answer is the body; a refusal carries its own HTTP status, reason phrase
    /// and body.
    fn install<'a>(&self, length: usize, read: &'a mut BodyReader<'a>) -> Refusal;
    /// Leave the frame. Called once the answer is on the wire; a method on the
    /// updater because resetting the chip is as much ESP-IDF as writing it.
    fn restart(&self) -> !;
}

/// Fills the buffer it is given and says how many bytes it wrote; zero means the
/// body ended early. The object's lifetime is the borrow's, so a reader can hold
/// the connection it reads from.
pub type BodyReader<'a> = dyn FnMut(&mut [u8]) -> Result<usize, String> + 'a;

/// A refused firmware upload: `(status, reason phrase, body)`.
pub type Refusal = Result<String, (u16, &'static str, String)>;

/// A running listener. Dropping it asks the thread to stop.
pub struct Server {
    stop: Arc<AtomicBool>,
    port: u16,
    locked: bool,
}

impl Server {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// True when the page asks for a password.
    pub fn locked(&self) -> bool {
        self.locked
    }

    /// Ask the accept loop to exit. Idempotent, and safe to call from the loop
    /// that owns the server: the thread notices within [`TICK`].
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for Server {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Server")
            .field("port", &self.port)
            .field("locked", &self.locked)
            .finish()
    }
}

/// Bind and start serving. The radio must already be up.
///
/// `password` is the `web.password` setting: `None` serves the page to anyone on
/// the network, which is the default for a home frame.
pub fn start(
    port: u16,
    password: Option<String>,
    jobs: Sender<Job>,
    firmware: Box<dyn Firmware + Send>,
) -> Result<Server, String> {
    let listener = bind(port)?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("ERR web nonblocking: {error}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let locked = password.is_some();
    thread::Builder::new()
        .name("web".to_owned())
        // Header parsing and a 192 KB body live on this stack; ESP-IDF's default
        // pthread stack is smaller than what a recursive-free parser plus a
        // temporary `String` comfortably wants.
        .stack_size(12 * 1024)
        .spawn(move || accept_loop(listener, jobs, flag, password, firmware))
        .map_err(|error| format!("ERR web thread: {error}"))?;
    Ok(Server { stop, port, locked })
}

/// Bind, retrying briefly: a previous listener on the same port may still be
/// releasing the socket after `wifi off`/`wifi join`.
fn bind(port: u16) -> Result<TcpListener, String> {
    let mut last = String::new();
    for attempt in 0..4 {
        match TcpListener::bind(("0.0.0.0", port)) {
            Ok(listener) => return Ok(listener),
            Err(error) => {
                last = error.to_string();
                thread::sleep(Duration::from_millis(150 * (attempt + 1)));
            }
        }
    }
    Err(format!("ERR web bind 0.0.0.0:{port}: {last}"))
}

fn accept_loop(
    listener: TcpListener,
    jobs: Sender<Job>,
    stop: Arc<AtomicBool>,
    password: Option<String>,
    firmware: Box<dyn Firmware + Send>,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                // A failed request is the client's problem, not the frame's: the
                // loop keeps listening either way.
                let _ = serve(stream, &jobs, password.as_deref(), &*firmware);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::sleep(TICK),
            Err(_) => thread::sleep(TICK),
        }
    }
    drop(listener);
}

/// A request head, parsed far enough to route it.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    /// Path with any query string removed.
    pub path: String,
    /// Query string as sent, still percent-encoded, without the `?`.
    pub query: String,
    pub content_length: Option<usize>,
    /// `Authorization` header as sent; checked only when a password is set.
    pub authorization: Option<String>,
    /// Whether [`POST_HEADER`] was present, which only a same-origin page can arrange.
    pub same_origin: bool,
    /// `Host` header as sent, port included.
    pub host: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// Parse a request head (everything before the blank line).
///
/// Header names are case-insensitive, the target's query string is dropped, and
/// only `GET` and `POST` are routable - which is why the parser has no `Other`
/// arm to forget about.
pub fn parse_head(head: &str) -> Result<Request, String> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("").trim_end();
    let mut parts = request_line.split_whitespace();
    let method = match parts.next() {
        Some("GET") => Method::Get,
        Some("POST") => Method::Post,
        Some(other) => return Err(format!("ERR web method {other}")),
        None => return Err("ERR web empty request line".to_owned()),
    };
    let target = parts
        .next()
        .ok_or_else(|| "ERR web request line has no target".to_owned())?;
    let version = parts
        .next()
        .ok_or_else(|| "ERR web request line has no version".to_owned())?;
    if !version.starts_with("HTTP/1.") {
        return Err(format!("ERR web version {version}"));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = path.to_owned();
    let query = query.to_owned();

    let mut content_length = None;
    let mut authorization = None;
    let mut same_origin = false;
    let mut host = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err("ERR web malformed header".to_owned());
        };
        if name.trim().eq_ignore_ascii_case("authorization") {
            authorization = Some(value.trim().to_owned());
        }
        if name.trim().eq_ignore_ascii_case("host") {
            host = Some(value.trim().to_owned());
        }
        if name.trim().eq_ignore_ascii_case(POST_HEADER) {
            same_origin = true;
        }
        if name.trim().eq_ignore_ascii_case("content-length") {
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|error| format!("ERR web content-length: {error}"))?;
            content_length = Some(parsed);
        }
    }
    Ok(Request {
        method,
        path,
        query,
        content_length,
        authorization,
        same_origin,
        host,
    })
}

fn serve<F: Firmware + ?Sized>(
    stream: TcpStream,
    jobs: &Sender<Job>,
    password: Option<&str>,
    firmware: &F,
) -> Result<(), String> {
    stream
        .set_write_timeout(Some(CLIENT_TIMEOUT))
        .map_err(|error| format!("ERR web timeout: {error}"))?;
    let mut incoming = Incoming::new(stream);
    let head = incoming.read_head()?;
    let text = String::from_utf8_lossy(&head);
    let request = match parse_head(&text) {
        Ok(request) => request,
        Err(error) => {
            return respond(
                &mut incoming.stream,
                400,
                "Bad Request",
                "text/plain",
                error.as_bytes(),
            );
        }
    };
    if let Some(password) = password
        && request
            .authorization
            .as_deref()
            .and_then(basic_password)
            .as_deref()
            != Some(password)
    {
        // The browser shows its own login box for this challenge; the page needs
        // no JavaScript to log in, which is why Basic is enough here.
        return respond_locked(&mut incoming.stream);
    }

    if !request.host.as_deref().is_none_or(local_host) {
        return respond(
            &mut incoming.stream,
            403,
            "Forbidden",
            "text/plain",
            b"ERR use the frame's address or local name\n",
        );
    }
    if request.method == Method::Post && !request.same_origin {
        return respond(
            &mut incoming.stream,
            403,
            "Forbidden",
            "text/plain",
            format!("ERR POST needs the {POST_HEADER} header the portal page sends\n").as_bytes(),
        );
    }

    match (request.method, request.path.as_str()) {
        (Method::Get, "/" | "/index.html") => respond(
            &mut incoming.stream,
            200,
            "OK",
            "text/html; charset=utf-8",
            PAGE.as_bytes(),
        ),
        (Method::Get, "/el6.mjs") => respond(
            &mut incoming.stream,
            200,
            "OK",
            "text/javascript; charset=utf-8",
            CONVERTER.as_bytes(),
        ),
        (Method::Get, "/status") => respond(
            &mut incoming.stream,
            200,
            "OK",
            "text/plain; charset=utf-8",
            format!("OK easellet-upload frame_bytes={FRAME_BYTES} {ROUTES}\n").as_bytes(),
        ),
        (Method::Post, "/upload") => upload(&mut incoming, &request, jobs),
        (Method::Get, "/ota") => ota_info(&mut incoming.stream, firmware),
        (Method::Post, "/ota") => ota_upload(&mut incoming, &request, firmware),
        (Method::Get, "/gallery") => list(&mut incoming.stream, jobs),
        (Method::Get, "/image") => image(&mut incoming.stream, &request, jobs),
        (Method::Post, "/show") => show(&mut incoming.stream, &request, jobs),
        (Method::Post, "/delete") => delete(&mut incoming.stream, &request, jobs),
        (Method::Get, "/settings") => settings(&mut incoming.stream, jobs),
        (Method::Get, "/sensors") => sensors(&mut incoming.stream, jobs),
        (Method::Post, "/settings") => save_settings(&mut incoming, &request, jobs),
        (Method::Get, _) => respond(
            &mut incoming.stream,
            404,
            "Not Found",
            "text/plain",
            b"ERR not here\n",
        ),
        (Method::Post, _) => respond(
            &mut incoming.stream,
            404,
            "Not Found",
            "text/plain",
            b"ERR unknown upload target\n",
        ),
    }
}

/// Take the frame, hand it to the main loop, and return whatever the loop says.
fn upload(incoming: &mut Incoming, request: &Request, jobs: &Sender<Job>) -> Result<(), String> {
    let length = match request.content_length {
        Some(length) if length == FRAME_BYTES => length,
        // A short body would paint a partial picture, so it is refused before a
        // byte is read rather than discovered halfway through.
        Some(length) => {
            return respond(
                &mut incoming.stream,
                413,
                "Payload Too Large",
                "text/plain",
                format!("ERR need exactly {FRAME_BYTES} bytes, got {length}\n").as_bytes(),
            );
        }
        None => {
            return respond(
                &mut incoming.stream,
                411,
                "Length Required",
                "text/plain",
                format!("ERR send Content-Length: {FRAME_BYTES}\n").as_bytes(),
            );
        }
    };
    debug_assert!(length <= MAX_BODY);

    let mut frame = vec![0_u8; length];
    incoming
        .read_exact(&mut frame)
        .map_err(|error| format!("ERR web body: {error}"))?;
    let stream = &mut incoming.stream;

    let (reply, answers) = mpsc::channel();
    if jobs.send(Job::Upload { frame, reply }).is_err() {
        return respond(
            stream,
            503,
            "Service Unavailable",
            "text/plain",
            b"ERR frame is shutting down\n",
        );
    }
    match answers.recv_timeout(UPLOAD_WAIT) {
        Ok(Ok(message)) => respond(
            stream,
            200,
            "OK",
            "text/plain; charset=utf-8",
            message.as_bytes(),
        ),
        Ok(Err(error)) => respond(
            stream,
            500,
            "Internal Server Error",
            "text/plain; charset=utf-8",
            error.as_bytes(),
        ),
        Err(_) => respond(
            stream,
            504,
            "Gateway Timeout",
            "text/plain",
            b"ERR the frame did not answer in time\n",
        ),
    }
}

/// What the firmware page shows before anyone uploads anything.
fn ota_info<F: Firmware + ?Sized>(stream: &mut TcpStream, firmware: &F) -> Result<(), String> {
    let reply = match firmware.info() {
        Ok(document) => document,
        Err(error) => error,
    };
    let ok = reply.starts_with('{');
    respond(
        stream,
        if ok { 200 } else { 500 },
        if ok { "OK" } else { "Internal Server Error" },
        if ok {
            "application/json; charset=utf-8"
        } else {
            "text/plain; charset=utf-8"
        },
        reply.as_bytes(),
    )
}

/// Take the firmware image, stream it into the inactive slot, and restart into
/// it. Writes happen here rather than through the main loop: nothing on the card
/// or the panel is involved, and a 1 MB body cannot be handed over whole.
fn ota_upload<F: Firmware + ?Sized>(
    incoming: &mut Incoming,
    request: &Request,
    firmware: &F,
) -> Result<(), String> {
    let Some(length) = request.content_length else {
        return respond(
            &mut incoming.stream,
            411,
            "Length Required",
            "text/plain",
            b"ERR send Content-Length: the size of the .bin\n",
        );
    };
    // A firmware upload is the one request that outlives the general deadline:
    // the socket waits while flash erases and writes.
    incoming.allow(OTA_DEADLINE);
    let result = firmware.install(length, &mut |chunk| {
        incoming
            .read_exact(chunk)
            .map(|()| chunk.len())
            .map_err(|error| error.to_string())
    });
    match result {
        Ok(message) => {
            respond(
                &mut incoming.stream,
                200,
                "OK",
                "text/plain; charset=utf-8",
                message.as_bytes(),
            )?;
            // The answer must leave the frame before the frame leaves: TCP data
            // still in the kernel is dropped by a reset, and the person would
            // see a failed upload that actually succeeded.
            thread::sleep(Duration::from_millis(500));
            firmware.restart()
        }
        Err((status, reason, message)) => respond(
            &mut incoming.stream,
            status,
            reason,
            "text/plain; charset=utf-8",
            message.as_bytes(),
        ),
    }
}

/// The settings form's backing document, built by the main loop because only it
/// knows the live values (audio levels, the watcher's gate, the slideshow).
fn settings(stream: &mut TcpStream, jobs: &Sender<Job>) -> Result<(), String> {
    let (reply, answers) = mpsc::channel();
    if jobs.send(Job::Settings { reply }).is_err() {
        return unavailable(stream);
    }
    match answers.recv_timeout(UPLOAD_WAIT) {
        Ok(document) => respond(
            stream,
            200,
            "OK",
            "application/json; charset=utf-8",
            document.as_bytes(),
        ),
        Err(_) => timed_out(stream),
    }
}

/// The last battery and climate reading, for the page's header.
fn sensors(stream: &mut TcpStream, jobs: &Sender<Job>) -> Result<(), String> {
    let (reply, answers) = mpsc::channel();
    if jobs.send(Job::Sensors { reply }).is_err() {
        return unavailable(stream);
    }
    match answers.recv_timeout(UPLOAD_WAIT) {
        Ok(document) => respond(
            stream,
            200,
            "OK",
            "application/json; charset=utf-8",
            document.as_bytes(),
        ),
        Err(_) => timed_out(stream),
    }
}

/// Apply the settings form. Fields are `application/x-www-form-urlencoded`, and
/// a field the form does not send is left alone - so a browser that omits a
/// control cannot silently erase the value behind it.
fn save_settings(
    incoming: &mut Incoming,
    request: &Request,
    jobs: &Sender<Job>,
) -> Result<(), String> {
    let Some(length) = request.content_length else {
        return respond(
            &mut incoming.stream,
            411,
            "Length Required",
            "text/plain",
            b"ERR send Content-Length\n",
        );
    };
    if length > MAX_FORM {
        return respond(
            &mut incoming.stream,
            413,
            "Payload Too Large",
            "text/plain",
            format!("ERR settings form limited to {MAX_FORM} bytes\n").as_bytes(),
        );
    }
    let mut body = vec![0_u8; length];
    incoming
        .read_exact(&mut body)
        .map_err(|error| format!("ERR web body: {error}"))?;
    let stream = &mut incoming.stream;
    let form = String::from_utf8_lossy(&body);
    let fields = match form_fields(&form) {
        Ok(fields) => fields,
        Err(error) => return respond(stream, 400, "Bad Request", "text/plain", error.as_bytes()),
    };
    let (reply, answers) = mpsc::channel();
    if jobs.send(Job::SaveSettings { fields, reply }).is_err() {
        return unavailable(stream);
    }
    match answers.recv_timeout(UPLOAD_WAIT) {
        Ok(Ok(message)) => respond(
            stream,
            200,
            "OK",
            "text/plain; charset=utf-8",
            message.as_bytes(),
        ),
        Ok(Err(error)) => respond(
            stream,
            400,
            "Bad Request",
            "text/plain; charset=utf-8",
            error.as_bytes(),
        ),
        Err(_) => timed_out(stream),
    }
}

/// Decode a form body into key/value pairs, values percent-decoded.
fn form_fields(body: &str) -> Result<Vec<(String, String)>, String> {
    let mut fields = Vec::new();
    for pair in body.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let name = percent_decode(name).ok_or_else(|| "ERR form field name".to_owned())?;
        let value = percent_decode(value).ok_or_else(|| "ERR form field value".to_owned())?;
        fields.push((name, value));
    }
    Ok(fields)
}

/// Whether a `Host` header names this frame the way a person on the LAN would:
/// an IPv4 address, a single-label name, or a name under a local-only suffix.
/// A request without `Host` comes from a tool, not a browser, and is allowed.
fn local_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => name,
        _ => host,
    };
    let name = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
    if name.parse::<std::net::Ipv4Addr>().is_ok() || !name.contains('.') {
        return !name.is_empty();
    }
    [".local", ".lan", ".home", ".home.arpa", ".internal"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// Password from a `Basic` credential, whatever the user name is.
///
/// Only the password is compared: the page has one account, and asking someone
/// to remember a name as well would be a worse trade than comparing one string.
fn basic_password(header: &str) -> Option<String> {
    let (scheme, encoded) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64_decode(encoded.trim())?;
    let text = String::from_utf8(decoded).ok()?;
    let (_user, password) = text.split_once(':')?;
    Some(password.to_owned())
}

/// Standard-alphabet base64 with padding, which is what browsers send.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
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
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut accumulator = 0_u32;
    let mut bits = 0_u32;
    for &byte in bytes {
        if byte == b'=' {
            break;
        }
        let value = value(byte)?;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    Some(decoded)
}

fn respond_locked(stream: &mut TcpStream) -> Result<(), String> {
    let head = "HTTP/1.1 401 Unauthorized\r\n\
                WWW-Authenticate: Basic realm=\"easellet\", charset=\"UTF-8\"\r\n\
                Content-Type: text/plain; charset=utf-8\r\n\
                Content-Length: 36\r\n\
                Connection: close\r\n\r\n\
                ERR the frame asks for its password\n";
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|error| format!("ERR web write: {error}"))
}

/// The slideshow's directory as JSON, so the portal can show what the frame has.
fn list(stream: &mut TcpStream, jobs: &Sender<Job>) -> Result<(), String> {
    let (reply, answers) = mpsc::channel();
    if jobs.send(Job::List { reply }).is_err() {
        return unavailable(stream);
    }
    match answers.recv_timeout(UPLOAD_WAIT) {
        Ok(Ok((directory, entries))) => respond(
            stream,
            200,
            "OK",
            "application/json; charset=utf-8",
            list_json(&directory, &entries).as_bytes(),
        ),
        Ok(Err(error)) => respond(
            stream,
            500,
            "Internal Server Error",
            "text/plain",
            error.as_bytes(),
        ),
        Err(_) => timed_out(stream),
    }
}

/// One picture, converted to a raw frame by the main loop, which owns the card.
fn image(stream: &mut TcpStream, request: &Request, jobs: &Sender<Job>) -> Result<(), String> {
    let Some(path) = query_value(&request.query, "path") else {
        return respond(
            stream,
            400,
            "Bad Request",
            "text/plain",
            b"ERR image needs ?path=\n",
        );
    };
    let (reply, answers) = mpsc::channel();
    if jobs.send(Job::Image { path, reply }).is_err() {
        return unavailable(stream);
    }
    match answers.recv_timeout(UPLOAD_WAIT) {
        Ok(Ok(frame)) => respond(stream, 200, "OK", "application/octet-stream", &frame),
        Ok(Err(error)) => respond(stream, 404, "Not Found", "text/plain", error.as_bytes()),
        Err(_) => timed_out(stream),
    }
}

/// Paint a picture that is already on the card.
fn show(stream: &mut TcpStream, request: &Request, jobs: &Sender<Job>) -> Result<(), String> {
    let Some(path) = query_value(&request.query, "path") else {
        return respond(
            stream,
            400,
            "Bad Request",
            "text/plain",
            b"ERR show needs ?path=\n",
        );
    };
    let (reply, answers) = mpsc::channel();
    if jobs.send(Job::Show { path, reply }).is_err() {
        return unavailable(stream);
    }
    match answers.recv_timeout(UPLOAD_WAIT) {
        Ok(Ok(message)) => respond(
            stream,
            200,
            "OK",
            "text/plain; charset=utf-8",
            message.as_bytes(),
        ),
        Ok(Err(error)) => respond(stream, 409, "Conflict", "text/plain", error.as_bytes()),
        Err(_) => timed_out(stream),
    }
}

/// Remove one picture. The decision about *which* files may be deleted is not
/// made here: the main loop owns the card and the writable directory, and this
/// function only carries the request and the answer back.
fn delete(stream: &mut TcpStream, request: &Request, jobs: &Sender<Job>) -> Result<(), String> {
    let Some(path) = query_value(&request.query, "path") else {
        return respond(
            stream,
            400,
            "Bad Request",
            "text/plain",
            b"ERR delete needs ?path=\n",
        );
    };
    let (reply, answers) = mpsc::channel();
    if jobs.send(Job::Delete { path, reply }).is_err() {
        return unavailable(stream);
    }
    match answers.recv_timeout(UPLOAD_WAIT) {
        Ok(Ok(message)) => respond(
            stream,
            200,
            "OK",
            "text/plain; charset=utf-8",
            message.as_bytes(),
        ),
        // A refusal is not a server fault: the name was understood and will not
        // be deleted, so 403 carries that meaning better than 409 or 500.
        Ok(Err(error)) => respond(stream, 403, "Forbidden", "text/plain", error.as_bytes()),
        Err(_) => timed_out(stream),
    }
}

fn unavailable(stream: &mut TcpStream) -> Result<(), String> {
    respond(
        stream,
        503,
        "Service Unavailable",
        "text/plain",
        b"ERR frame is shutting down\n",
    )
}

fn timed_out(stream: &mut TcpStream) -> Result<(), String> {
    respond(
        stream,
        504,
        "Gateway Timeout",
        "text/plain",
        b"ERR the frame did not answer in time\n",
    )
}

/// Value of one query parameter, percent-decoded.
fn query_value(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if name == key {
            return percent_decode(value);
        }
    }
    None
}

/// `%20` and friends, so a picture named with a space can be addressed. Returns
/// `None` on a truncated escape or invalid UTF-8 rather than guessing.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes.get(index + 1..index + 3)?;
                let high = (hex[0] as char).to_digit(16)?;
                let low = (hex[1] as char).to_digit(16)?;
                decoded.push((high * 16 + low) as u8);
                index += 3;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

/// `[{"name":"...","bytes":123}]` plus the directory, built by hand: one tiny
/// response does not justify a JSON dependency.
fn list_json(directory: &str, entries: &[Entry]) -> String {
    let pictures: Vec<String> = entries
        .iter()
        .map(|entry| {
            format!(
                "{{\"name\":\"{}\",\"bytes\":{}}}",
                json_escape(&entry.name),
                entry.bytes
            )
        })
        .collect();
    format!(
        "{{\"dir\":\"{}\",\"frame_bytes\":{FRAME_BYTES},\"images\":[{}]}}",
        json_escape(directory),
        pictures.join(",")
    )
}

pub fn json_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            character if (character as u32) < 0x20 => {
                escaped.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

/// The request side of a connection, held to [`REQUEST_DEADLINE`] as a whole.
///
/// The head is read in chunks rather than a byte per `recv`; whatever arrives
/// after the blank line is the start of the body and is handed out first.
struct Incoming {
    stream: TcpStream,
    deadline: Instant,
    /// Body bytes that arrived with the head, and how many are already used.
    early: Vec<u8>,
    used: usize,
}

impl Incoming {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            deadline: Instant::now() + REQUEST_DEADLINE,
            early: Vec::new(),
            used: 0,
        }
    }

    /// Bytes up to and including the blank line that ends the head.
    fn read_head(&mut self) -> Result<Vec<u8>, String> {
        let mut buffer = Vec::with_capacity(1024);
        let mut chunk = [0_u8; 512];
        loop {
            let count = match self.read_socket(&mut chunk) {
                Ok(0) => return Err("ERR web head too long or closed".to_owned()),
                Ok(count) => count,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(format!("ERR web read: {error}")),
            };
            // The terminator may straddle two chunks.
            let from = buffer.len().saturating_sub(3);
            buffer.extend_from_slice(&chunk[..count]);
            if let Some(at) = find_blank_line(&buffer[from..]) {
                let end = from + at + 4;
                self.early = buffer.split_off(end);
                return Ok(buffer);
            }
            if buffer.len() >= MAX_HEAD {
                return Err("ERR web head too long or closed".to_owned());
            }
        }
    }

    /// Give the rest of the request more time: the deadline is one number for
    /// the whole connection, and a firmware upload has to wait for flash.
    fn allow(&mut self, extra: Duration) {
        self.deadline = Instant::now() + extra;
    }

    /// One socket read, its timeout trimmed to what is left of the deadline.
    fn read_socket(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ErrorKind::TimedOut.into());
        }
        self.stream
            .set_read_timeout(Some(left.min(CLIENT_TIMEOUT)))?;
        self.stream.read(out)
    }
}

impl Read for Incoming {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let early = &self.early[self.used..];
        if !early.is_empty() {
            let count = early.len().min(out.len());
            out[..count].copy_from_slice(&early[..count]);
            self.used += count;
            return Ok(count);
        }
        self.read_socket(out)
    }
}

fn find_blank_line(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn respond(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body))
        .and_then(|()| stream.flush())
        .map_err(|error| format!("ERR web write: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browser_request_parses() {
        let head = "POST /upload HTTP/1.1\r\nHost: 192.168.50.93\r\nContent-Length: 192000\r\nContent-Type: application/octet-stream\r\n\r\n";
        let request = parse_head(head).unwrap();
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.path, "/upload");
        assert_eq!(request.content_length, Some(FRAME_BYTES));
    }

    #[test]
    fn header_names_are_case_insensitive_and_queries_are_dropped() {
        let head = "GET /status?x=1 HTTP/1.1\r\nhost: frame\r\ncontent-LENGTH:  17 \r\n\r\n";
        let request = parse_head(head).unwrap();
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.path, "/status");
        assert_eq!(request.content_length, Some(17));
    }

    #[test]
    fn only_a_post_with_the_portal_header_counts_as_same_origin() {
        let marked = parse_head("POST /settings HTTP/1.1\r\nx-easellet: 1\r\n\r\n").unwrap();
        assert!(marked.same_origin);
        // What a form on another site can make a browser send: no custom header.
        let plain = parse_head(
            "POST /settings HTTP/1.1\r\nContent-Type: application/x-www-form-urlencoded\r\nOrigin: http://evil.example\r\n\r\n",
        )
        .unwrap();
        assert!(!plain.same_origin);
    }

    #[test]
    fn the_page_and_the_status_line_agree_on_the_routes() {
        for route in ["/ota", "/settings", "/gallery", "/sensors"] {
            assert!(ROUTES.contains(route), "{route} missing from GET /status");
            assert!(
                PAGE.contains(&format!("fetch(\"{route}\"")),
                "the page does not read {route}"
            );
        }
        for route in ["/upload", "/settings", "/ota"] {
            assert!(
                PAGE.contains(&format!("post(\"{route}\"")),
                "the page does not post {route}"
            );
        }
    }

    #[test]
    fn the_page_marks_every_post() {
        // Every `method: "POST"` in the page goes through `post()`, which adds
        // the header; a bare fetch would be refused with 403.
        assert!(PAGE.contains(&format!("\"{POST_HEADER}\": \"1\"")));
        assert_eq!(
            PAGE.matches("method: \"POST\"").count(),
            1,
            "only post() may POST"
        );
    }

    #[test]
    fn only_local_host_names_are_served() {
        for host in [
            "192.168.50.204",
            "192.168.50.204:80",
            "easellet",
            "ODCAED44.local",
            "odcaed44.local.",
            "frame.lan",
            "frame.home.arpa",
        ] {
            assert!(local_host(host), "{host}");
        }
        for host in [
            "evil.example",
            "evil.example:80",
            "frame.local.evil.example",
            "",
            "1.2.3.4.nip.io",
        ] {
            assert!(!local_host(host), "{host}");
        }
        let request = parse_head("GET / HTTP/1.1\r\nHost: rebind.example\r\n\r\n").unwrap();
        assert_eq!(request.host.as_deref(), Some("rebind.example"));
    }

    #[test]
    fn the_blank_line_is_found_wherever_it_lands() {
        assert_eq!(find_blank_line(b"GET / HTTP/1.1\r\n\r\nbody"), Some(14));
        assert_eq!(find_blank_line(b"\r\n\r\n"), Some(0));
        assert_eq!(find_blank_line(b"GET / HTTP/1.1\r\n\r"), None);
    }

    #[test]
    fn a_missing_length_is_not_zero() {
        // `None` is refused with 411 rather than read as an empty body, so a
        // client that forgot the header cannot post a zero-byte "picture".
        let request = parse_head("POST /upload HTTP/1.1\r\nHost: f\r\n\r\n").unwrap();
        assert_eq!(request.content_length, None);
    }

    #[test]
    fn junk_is_refused() {
        assert!(parse_head("").is_err());
        assert!(parse_head("PUT /upload HTTP/1.1\r\n\r\n").is_err());
        assert!(parse_head("POST /upload\r\n\r\n").is_err());
        assert!(parse_head("POST /upload HTTP/0.9\r\n\r\n").is_err());
        assert!(parse_head("POST /upload HTTP/1.1\r\nno colon here\r\n\r\n").is_err());
        assert!(parse_head("POST /upload HTTP/1.1\r\nContent-Length: many\r\n\r\n").is_err());
    }

    #[test]
    fn a_query_parameter_is_percent_decoded() {
        assert_eq!(
            query_value("path=EASELTEST%2FUP0001.EL6", "path").unwrap(),
            "EASELTEST/UP0001.EL6"
        );
        assert_eq!(
            query_value("x=1&path=my%20photo.el6", "path").unwrap(),
            "my photo.el6"
        );
        assert_eq!(query_value("x=1", "path"), None);
        // A truncated escape is refused rather than passed through as text.
        assert_eq!(query_value("path=%2", "path"), None);
        assert_eq!(query_value("path=%zz", "path"), None);
    }

    #[test]
    fn the_listing_is_valid_json() {
        let entries = [Entry {
            name: "EASELTEST/UP0001.EL6".to_owned(),
            bytes: 192_000,
        }];
        assert_eq!(
            list_json("EASELTEST", &entries),
            "{\"dir\":\"EASELTEST\",\"frame_bytes\":192000,\"images\":[{\"name\":\"EASELTEST/UP0001.EL6\",\"bytes\":192000}]}"
        );
        // A quote or a backslash in a name must not break the document.
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
    }

    #[test]
    fn a_basic_credential_yields_the_password() {
        // btoa("frame:easellet-lan")
        let header = "Basic ZnJhbWU6ZWFzZWxsZXQtbGFu";
        assert_eq!(basic_password(header).as_deref(), Some("easellet-lan"));
        assert_eq!(
            basic_password("basic ZnJhbWU6ZWFzZWxsZXQtbGFu").as_deref(),
            Some("easellet-lan")
        );
        // No colon, wrong scheme, or broken base64 is simply not a credential.
        assert_eq!(basic_password("Basic bm9jb2xvbg=="), None);
        assert_eq!(basic_password("Bearer xyz"), None);
        assert_eq!(basic_password("Basic !!!"), None);
        assert_eq!(basic_password(""), None);
    }

    #[test]
    fn the_settings_form_is_key_value_pairs() {
        let fields = form_fields("wifi.ssid=Home+WiFi&ai.threshold=150&ai.key=&x=a%20b").unwrap();
        assert_eq!(
            fields,
            [
                ("wifi.ssid".to_owned(), "Home WiFi".to_owned()),
                ("ai.threshold".to_owned(), "150".to_owned()),
                ("ai.key".to_owned(), String::new()),
                ("x".to_owned(), "a b".to_owned()),
            ]
        );
        assert!(form_fields("ok=1&&").is_ok());
        assert!(form_fields("bad=%zz").is_err());
    }

    #[test]
    fn an_authorization_header_is_parsed() {
        let head =
            "GET /settings HTTP/1.1\r\nAuthorization: Basic ZnJhbWU6ZWFzZWxsZXQtbGFu\r\n\r\n";
        let request = parse_head(head).unwrap();
        assert_eq!(
            request.authorization.as_deref(),
            Some("Basic ZnJhbWU6ZWFzZWxsZXQtbGFu")
        );
        assert_eq!(
            parse_head("GET / HTTP/1.1\r\n\r\n").unwrap().authorization,
            None
        );
    }

    #[test]
    fn the_served_converter_is_the_tested_one() {
        // The gate proves `tools/el6.mjs` matches `convert.py`. This asserts the
        // server serves *that file*, not a stale copy that drifted from it.
        assert!(CONVERTER.contains("export function pack("));
        assert!(CONVERTER.contains("export function packImage("));
        assert!(CONVERTER.contains("export const FRAME_BYTES"));
    }

    #[test]
    fn a_delete_request_names_one_picture() {
        let head =
            parse_head("POST /delete?path=EASELTEST%2FUPLOADS%2FUP0001.EL6 HTTP/1.1\r\n\r\n")
                .unwrap();
        assert_eq!(head.method, Method::Post);
        assert_eq!(head.path, "/delete");
        assert_eq!(
            query_value(&head.query, "path").unwrap(),
            "EASELTEST/UPLOADS/UP0001.EL6"
        );
    }

    #[test]
    fn the_page_offers_deletion_only_where_the_frame_allows_it() {
        // The write policy is the firmware's (`image::removable`), but the page
        // must not draw a button bound to fail: it filters by the same prefix.
        assert!(PAGE.contains("\"EASELTEST/\"") || PAGE.contains("\"easeltest/\""));
        assert!(PAGE.contains("/delete?path="));
    }

    #[test]
    fn the_page_only_posts_frames_the_frame_accepts() {
        // The page must not hard-code a size the server would refuse: it imports
        // `FRAME_BYTES` and `packImage`, so the two cannot disagree.
        assert!(PAGE.contains("packImage"));
        assert!(PAGE.contains("FRAME_BYTES"));
        assert!(!PAGE.contains("192000"), "size must come from el6.mjs");
    }
}
