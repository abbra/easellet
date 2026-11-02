//! Plain HTTP over the station interface, for talking to the LAN bridge.
//!
//! This is deliberately the smallest transport that can carry the bridge protocol:
//! one POST, a bounded reply, no TLS and no redirects. HTTPS is compiled out of the
//! ESP-IDF client in `sdkconfig.defaults`, so a `https://` URL cannot silently pull a
//! certificate bundle into a firmware whose threat model is "the frame is on my own
//! network"; the bridge or a proxy in front of it terminates TLS instead.
//!
//! Request shapes and reply decoding live in `bridge`, which is unit-tested on the
//! host. What is left here cannot be tested off the device, so it is kept small
//! enough to read in one screen.
//!
//! The reply limit is a caller's argument rather than a constant here, because the
//! three endpoints answer at wildly different sizes: a transcript is a line of text,
//! a synthesised sentence is seconds of audio. One constant for all three either
//! truncates speech (64 KiB is four seconds) or lets a text endpoint allocate
//! megabytes.

use std::time::Duration;

use esp_idf_svc::http::{
    Method,
    client::{Configuration, EspHttpConnection},
};

/// Wall-clock budget for one request. Recognition on a LAN machine takes a few
/// seconds of CPU per second of speech, so the ceiling is generous; the console
/// itself gives up at 120 s.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Socket buffers, not body limits: the client hands the body over in slices.
const BUFFER_BYTES: usize = 2_048;

pub struct Reply {
    pub status: u16,
    pub content_type: String,
    /// The `ETag` header, for a picture the frame will ask about again.
    pub etag: Option<String>,
    /// `X-Panel: keep`: a bridge plugin is sending the panel a picture, so the
    /// answer is spoken but not painted over it.
    pub keep_panel: bool,
    /// `X-Action`: a command for the frame after the answer (`actions.rs`
    /// decides whether it is one); bounded, so a header cannot grow the heap.
    pub action: Option<String>,
    pub body: Vec<u8>,
    /// Set when the body hit the caller's limit and the rest was dropped. Callers
    /// decide the consequence: text cannot be used half-said, audio can.
    pub truncated: bool,
}

impl Reply {
    /// Whether this is an answer rather than an error page.
    pub fn ok(&self) -> bool {
        self.status == 200
    }

    /// The body as text, for reporting a failure the host explained in words.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).trim().to_owned()
    }
}

/// POST `body` to `url`, sending `headers` as-is, keeping at most `limit` bytes of
/// the reply.
///
/// A non-200 status is not an error here: the bridge answers 401 for a wrong token
/// and 500 with a reason when recognition fails, and both are worth showing verbatim
/// rather than flattening into "request failed".
pub fn post(
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    limit: usize,
) -> Result<Reply, String> {
    let configuration = Configuration {
        buffer_size: Some(BUFFER_BYTES),
        buffer_size_tx: Some(BUFFER_BYTES),
        timeout: Some(TIMEOUT),
        ..Default::default()
    };
    let mut connection = EspHttpConnection::new(&configuration)
        .map_err(|error| format!("ERR http client: {error}\r\n"))?;

    connection
        .initiate_request(Method::Post, url, headers)
        .map_err(|error| format!("ERR http {url}: {error}\r\n"))?;
    connection
        .write_all(body)
        .map_err(|error| format!("ERR http body {url}: {error}\r\n"))?;
    connection
        .initiate_response()
        .map_err(|error| format!("ERR http response {url}: {error}\r\n"))?;

    read_reply(connection, url, limit)
}

/// GET `url`, keeping at most `limit` bytes of the reply. Used for the picture a
/// sleeping frame fetches from the bridge on a timed wake.
pub fn get(url: &str, headers: &[(&str, &str)], limit: usize) -> Result<Reply, String> {
    let configuration = Configuration {
        buffer_size: Some(BUFFER_BYTES),
        buffer_size_tx: Some(BUFFER_BYTES),
        timeout: Some(TIMEOUT),
        ..Default::default()
    };
    let mut connection = EspHttpConnection::new(&configuration)
        .map_err(|error| format!("ERR http client: {error}\r\n"))?;
    connection
        .initiate_request(Method::Get, url, headers)
        .map_err(|error| format!("ERR http {url}: {error}\r\n"))?;
    connection
        .initiate_response()
        .map_err(|error| format!("ERR http response {url}: {error}\r\n"))?;
    read_reply(connection, url, limit)
}

fn read_reply(mut connection: EspHttpConnection, url: &str, limit: usize) -> Result<Reply, String> {
    let status = connection.status();
    let content_type = connection.header("Content-Type").unwrap_or("").to_owned();
    let etag = connection.header("ETag").map(str::to_owned);
    let keep_panel = connection
        .header("X-Panel")
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("keep"));
    let action = connection
        .header("X-Action")
        .filter(|value| value.len() <= 64)
        .map(str::to_owned);
    let mut reply = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; BUFFER_BYTES];
    loop {
        match connection.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                // Stop at the limit and drop the connection: the rest is not
                // wanted, and draining it would let a server that never stops
                // sending hold the main loop forever (each read has its own
                // timeout, the body as a whole has none).
                let room = limit.saturating_sub(reply.len());
                if count > room {
                    reply.extend_from_slice(&buffer[..room]);
                    truncated = true;
                    break;
                }
                reply.extend_from_slice(&buffer[..count]);
            }
            Err(error) => return Err(format!("ERR http read {url}: {error}\r\n")),
        }
    }
    Ok(Reply {
        status,
        content_type,
        etag,
        keep_panel,
        action,
        body: reply,
        truncated,
    })
}
