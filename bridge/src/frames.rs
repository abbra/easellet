//! The frames this bridge knows: found by mDNS or added by hand, with their
//! status, their portal (gallery, upload, show, delete) and OpenDisplay.
//!
//! Two channels, as `research/bridge-ui-design.md` section 3 lays out:
//!
//! - **The frame's portal** (HTTP :80, our firmware only): status, the gallery,
//!   storing pictures. Proxied, so the browser never talks to a frame and a
//!   frame's password never leaves this host.
//! - **OpenDisplay** (TCP 2446, any OpenDisplay device): the panel's description
//!   and showing one picture without storing it.
//!
//! A battery frame sleeps most of the time and answers neither; it is shown as
//! offline with when it was last seen, and nothing here keeps it awake.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use opendisplay_proto::client::{self, ConfigReader};
use opendisplay_proto::config::{Blob, DisplayConfig, PacketType};
use opendisplay_proto::wire::{LAN_MAX_PAYLOAD, LAN_TCP_PORT, Opcode};
use opendisplay_proto::{RefreshMode, lan, mdns};
use serde::Serialize;

use crate::settings::FrameEntry;

/// How long a browse listens for answers.
const BROWSE_WAIT: Duration = Duration::from_millis(1_500);
/// Connect and read timeout for a frame's portal or OpenDisplay port: a frame
/// on the LAN answers in milliseconds, and a sleeping one never does.
const FRAME_TIMEOUT: Duration = Duration::from_secs(3);
/// Larger timeout for a whole picture over OpenDisplay.
const IMAGE_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest answer accepted from a frame's portal: a 192 000-byte frame plus slack.
const MAX_PORTAL_ANSWER: u64 = 256 * 1024;

/// The panel, from the frame's OpenDisplay config.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Panel {
    pub width: u16,
    pub height: u16,
    /// Quarter turns the client applies before packing.
    pub rotation: u8,
    /// OpenDisplay colour scheme id (4 = six-colour Spectra).
    pub color_scheme: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct Frame {
    /// Stable id for URLs: the announced MAC without colons, or `ip-…` for a
    /// frame known only by address.
    pub id: String,
    pub name: String,
    pub host: String,
    /// OpenDisplay port.
    pub port: u16,
    /// Found by mDNS, listed by hand, or both.
    pub mdns: bool,
    pub manual: bool,
    pub mac: Option<String>,
    pub firmware: Option<String>,
    /// Answered the last status check.
    pub online: bool,
    /// Runs this project's firmware (its portal answered).
    pub portal: bool,
    /// The portal's `/sensors` document, as it was last read.
    pub sensors: Option<serde_json::Value>,
    pub panel: Option<Panel>,
    /// Seconds since the frame last answered anything.
    pub seen_s: Option<u64>,
    pub error: Option<String>,
    #[serde(skip)]
    seen: Option<Instant>,
    #[serde(skip)]
    password: String,
}

impl Frame {
    /// The frame portal's password from the settings, if it has one.
    pub fn portal_password(&self) -> &str {
        &self.password
    }
}

/// One frame as an mDNS answer describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub instance: String,
    pub address: Ipv4Addr,
    pub port: u16,
    pub txt: BTreeMap<String, String>,
}

#[derive(Default)]
pub struct Registry {
    frames: Mutex<BTreeMap<String, Frame>>,
}

fn id_for(mac: Option<&str>, host: &str) -> String {
    match mac {
        Some(mac) => mac
            .chars()
            .filter(char::is_ascii_hexdigit)
            .collect::<String>()
            .to_ascii_lowercase(),
        None => format!(
            "ip-{}",
            host.chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect::<String>()
        ),
    }
}

impl Registry {
    /// Everything known, with `seen_s` brought up to date.
    pub fn list(&self) -> Vec<Frame> {
        let Ok(frames) = self.frames.lock() else {
            return Vec::new();
        };
        frames
            .values()
            .cloned()
            .map(|mut frame| {
                frame.seen_s = frame.seen.map(|at| at.elapsed().as_secs());
                frame
            })
            .collect()
    }

    /// The frame's address, OpenDisplay port and portal password, by id.
    pub fn target(&self, id: &str) -> Option<(String, u16, String)> {
        let frames = self.frames.lock().ok()?;
        let frame = frames.get(id)?;
        Some((frame.host.clone(), frame.port, frame.password.clone()))
    }

    /// Browse, merge in the hand-added frames, and check every frame's status.
    /// Blocks for the browse plus a few seconds per unreachable frame.
    pub fn refresh(&self, manual: &[FrameEntry]) {
        let found = browse(BROWSE_WAIT).unwrap_or_else(|error| {
            eprintln!("  frames: mDNS browse failed: {error}");
            Vec::new()
        });
        let mut current: BTreeMap<String, Frame> = self
            .frames
            .lock()
            .map(|frames| frames.clone())
            .unwrap_or_default();
        let mut present = BTreeSet::new();
        for found in &found {
            let mac = found.txt.get("mac").cloned();
            let host = found.address.to_string();
            let id = id_for(mac.as_deref(), &host);
            let frame = current.entry(id.clone()).or_insert_with(|| Frame {
                id: id.clone(),
                name: found.instance.clone(),
                host: host.clone(),
                port: found.port,
                mdns: true,
                manual: false,
                mac: mac.clone(),
                firmware: None,
                online: false,
                portal: false,
                sensors: None,
                panel: None,
                seen_s: None,
                error: None,
                seen: None,
                password: String::new(),
            });
            frame.host = host;
            frame.port = found.port;
            frame.mdns = true;
            frame.firmware = found.txt.get("fw").cloned();
            frame.seen = Some(Instant::now());
            present.insert(id);
        }
        for entry in manual {
            // A hand-added address that mDNS also found is the same frame.
            let id = current
                .values()
                .find(|frame| frame.host == entry.host)
                .map_or_else(|| id_for(None, &entry.host), |frame| frame.id.clone());
            let frame = current.entry(id.clone()).or_insert_with(|| Frame {
                id: id.clone(),
                name: entry.host.clone(),
                host: entry.host.clone(),
                port: LAN_TCP_PORT,
                mdns: false,
                manual: true,
                mac: None,
                firmware: None,
                online: false,
                portal: false,
                sensors: None,
                panel: None,
                seen_s: None,
                error: None,
                seen: None,
                password: String::new(),
            });
            frame.manual = true;
            frame.password.clone_from(&entry.password);
            if !entry.name.is_empty() {
                frame.name.clone_from(&entry.name);
            }
            present.insert(id);
        }
        // Frames neither announced nor listed any more are forgotten; a frame that
        // is merely asleep keeps its entry until the bridge restarts.
        current.retain(|id, frame| present.contains(id) || frame.mdns);
        for frame in current.values_mut() {
            check(frame);
        }
        if let Ok(mut frames) = self.frames.lock() {
            *frames = current;
        }
    }

    /// Forget a frame's entry (after it was removed from the settings).
    pub fn forget(&self, host: &str) {
        if let Ok(mut frames) = self.frames.lock() {
            frames.retain(|_, frame| frame.host != host || frame.mdns);
        }
    }
}

/// Status of one frame: its portal's `/sensors`, and the panel once.
fn check(frame: &mut Frame) {
    match portal(&frame.host, &frame.password, "GET", "/sensors", &[]) {
        Ok(answer) if answer.status == 200 => {
            frame.online = true;
            frame.portal = true;
            frame.sensors = serde_json::from_slice(&answer.body).ok();
            frame.seen = Some(Instant::now());
            frame.error = None;
        }
        Ok(answer) if answer.status == 401 => {
            frame.online = true;
            frame.portal = true;
            frame.seen = Some(Instant::now());
            frame.error = Some("the frame's portal wants a password".to_owned());
        }
        Ok(_) | Err(_) => {
            frame.online = false;
            frame.portal = false;
        }
    }
    if frame.panel.is_none() {
        match read_panel(&frame.host, frame.port) {
            Ok(panel) => {
                frame.panel = Some(panel);
                frame.online = true;
                frame.seen = Some(Instant::now());
            }
            Err(error) if frame.online => frame.error = Some(error),
            Err(_) => {}
        }
    }
}

/// Send one legacy mDNS browse and collect what answers within `wait`.
pub fn browse(wait: Duration) -> Result<Vec<Found>, String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|error| error.to_string())?;
    socket
        .set_multicast_ttl_v4(255)
        .map_err(|error| error.to_string())?;
    let mut query = [0u8; 64];
    let len = mdns::browse_query(0x2446, &mut query).map_err(|_| "query too long".to_owned())?;
    socket
        .send_to(&query[..len], (Ipv4Addr::new(224, 0, 0, 251), mdns::PORT))
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + wait;
    let mut found = Vec::new();
    let mut packet = [0u8; 1500];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if left.is_zero() {
            break;
        }
        socket
            .set_read_timeout(Some(left))
            .map_err(|error| error.to_string())?;
        let Ok((len, _)) = socket.recv_from(&mut packet) else {
            break;
        };
        found.extend(read_answer(&packet[..len]));
    }
    found.sort_by(|a, b| a.instance.cmp(&b.instance));
    found.dedup_by(|a, b| a.instance == b.instance);
    Ok(found)
}

/// The frames one response describes: PTR -> instance, SRV -> port and host,
/// A -> address, TXT -> keys. A responder sends them together.
fn read_answer(packet: &[u8]) -> Vec<Found> {
    let mut instances = Vec::new();
    let mut services: BTreeMap<String, (u16, String)> = BTreeMap::new();
    let mut texts: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut addresses: BTreeMap<String, Ipv4Addr> = BTreeMap::new();
    let service = mdns::SERVICE.to_ascii_lowercase();
    let parsed = mdns::parse_response(packet, |record| match record {
        mdns::Record::Ptr { name, target } => {
            if name.as_str() == Some(service.as_str())
                && let Some(target) = target.as_str()
            {
                instances.push(target.to_owned());
            }
        }
        mdns::Record::Srv { name, port, target } => {
            if let (Some(name), Some(target)) = (name.as_str(), target.as_str()) {
                services.insert(name.to_owned(), (port, target.to_owned()));
            }
        }
        mdns::Record::Txt { name, data } => {
            if let Some(name) = name.as_str() {
                let entries = mdns::txt_entries(data)
                    .map(|(key, value)| {
                        (
                            String::from_utf8_lossy(key).to_ascii_lowercase(),
                            String::from_utf8_lossy(value).into_owned(),
                        )
                    })
                    .collect();
                texts.insert(name.to_owned(), entries);
            }
        }
        mdns::Record::A { name, address } => {
            if let Some(name) = name.as_str() {
                addresses.insert(name.to_owned(), Ipv4Addr::from(address));
            }
        }
    });
    if parsed.is_none() {
        return Vec::new();
    }
    instances
        .into_iter()
        .filter_map(|instance| {
            let (port, host) = services.get(&instance)?;
            let address = *addresses.get(host)?;
            let label = instance
                .strip_suffix(&format!(".{service}"))
                .unwrap_or(&instance);
            Some(Found {
                instance: label.to_uppercase(),
                address,
                port: *port,
                txt: texts.remove(&instance).unwrap_or_default(),
            })
        })
        .collect()
}

/// One answer from a frame's portal.
pub struct PortalAnswer {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// A request to a frame's portal. POSTs carry the portal's `X-Easellet`
/// header; `path` is built by the bridge, never taken whole from a request.
pub fn portal(
    host: &str,
    password: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<PortalAnswer, String> {
    let url = format!("http://{}{path}", bracket(host));
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(FRAME_TIMEOUT)
        .timeout(Duration::from_secs(40))
        .build();
    let mut request = agent.request(method, &url);
    if method == "POST" {
        request = request.set("X-Easellet", "1");
    }
    if !password.is_empty() {
        request = request.set(
            "Authorization",
            &format!("Basic {}", base64(&format!("bridge:{password}"))),
        );
    }
    let result = if method == "POST" {
        request
            .set("Content-Type", "application/octet-stream")
            .send_bytes(body)
    } else {
        request.call()
    };
    let response = match result {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(error) => return Err(format!("{host}: {error}")),
    };
    let status = response.status();
    let content_type = response.content_type().to_owned();
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_PORTAL_ANSWER)
        .read_to_end(&mut body)
        .map_err(|error| format!("{host}: {error}"))?;
    Ok(PortalAnswer {
        status,
        content_type,
        body,
    })
}

fn bracket(host: &str) -> String {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => format!("[{host}]"),
        _ => host.to_owned(),
    }
}

fn base64(text: &str) -> String {
    base64_bytes(text.as_bytes())
}

/// Standard base64 with padding.
pub fn base64_bytes(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let word = u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(ALPHABET[((word >> (18 - 6 * index)) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// An OpenDisplay session over TCP.
struct Session {
    stream: TcpStream,
}

impl Session {
    fn open(host: &str, port: u16, timeout: Duration) -> Result<Self, String> {
        let address: SocketAddr = format!("{}:{port}", bracket(host)).parse().or_else(|_| {
            std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
                .map_err(|error| error.to_string())?
                .next()
                .ok_or_else(|| format!("{host}: no address"))
        })?;
        let stream = TcpStream::connect_timeout(&address, FRAME_TIMEOUT)
            .map_err(|error| format!("{host}:{port}: {error}"))?;
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)))
            .and_then(|()| stream.set_nodelay(true))
            .map_err(|error| error.to_string())?;
        Ok(Self { stream })
    }

    fn send(&mut self, payload: &[u8]) -> Result<(), String> {
        let prefix = lan::prefix(payload).map_err(|_| "request too long".to_owned())?;
        let mut frame = Vec::with_capacity(2 + payload.len());
        frame.extend_from_slice(&prefix);
        frame.extend_from_slice(payload);
        self.stream
            .write_all(&frame)
            .map_err(|error| format!("send: {error}"))
    }

    fn receive(&mut self) -> Result<Vec<u8>, String> {
        let mut prefix = [0u8; 2];
        self.stream
            .read_exact(&mut prefix)
            .map_err(|error| format!("receive: {error}"))?;
        let len = lan::payload_len(prefix).map_err(|_| "bad frame length".to_owned())?;
        let mut payload = vec![0u8; len];
        self.stream
            .read_exact(&mut payload)
            .map_err(|error| format!("receive: {error}"))?;
        Ok(payload)
    }
}

/// The panel's size and colours, from the frame's config.
pub fn read_panel(host: &str, port: u16) -> Result<Panel, String> {
    let mut session = Session::open(host, port, FRAME_TIMEOUT)?;
    session.send(&client::simple(Opcode::ConfigRead))?;
    let mut reader = ConfigReader::default();
    let blob = loop {
        let frame = session.receive()?;
        if let Some(blob) = reader
            .feed(&frame)
            .map_err(|error| format!("config: {error:?}"))?
        {
            break blob.to_vec();
        }
    };
    let blob = Blob::parse(&blob).map_err(|error| format!("config: {error:?}"))?;
    let display: &DisplayConfig = blob
        .find(PacketType::DISPLAY)
        .ok_or_else(|| "config has no display".to_owned())?;
    Ok(Panel {
        width: display.pixel_width.get(),
        height: display.pixel_height.get(),
        rotation: display.rotation,
        color_scheme: display.color_scheme,
    })
}

/// Show one picture, already in the panel's own format, without storing it:
/// `DirectWrite` with zlib. Returns once the frame has accepted the image and
/// started its refresh; the refresh itself takes the panel about 30 s more.
pub fn display(host: &str, port: u16, image: &[u8]) -> Result<String, String> {
    let started = Instant::now();
    let stream = miniz_oxide::deflate::compress_to_vec_zlib(image, 6);
    let mut session = Session::open(host, port, IMAGE_TIMEOUT)?;
    let mut out = vec![0u8; LAN_MAX_PAYLOAD];
    let image_len = u32::try_from(image.len()).map_err(|_| "image too large".to_owned())?;
    let (len, mut sent) = client::write_start_zlib(image_len, &stream, &mut out);
    session.send(&out[..len])?;
    client::expect_ack(&session.receive()?, Opcode::DirectWriteStart)
        .map_err(|error| refused(error, "the frame is busy (refreshing) or refused the image"))?;
    while sent < stream.len() {
        let (len, took) = client::write_data(&stream[sent..], &mut out);
        sent += took;
        session.send(&out[..len])?;
        client::expect_ack(&session.receive()?, Opcode::DirectWriteData)
            .map_err(|error| refused(error, "the frame refused the data"))?;
    }
    let mut end = [0u8; 7];
    let len = client::write_end(RefreshMode::Full, None, &mut end);
    session.send(&end[..len])?;
    client::expect_ack(&session.receive()?, Opcode::DirectWriteEnd)
        .map_err(|error| refused(error, "the frame did not start the refresh"))?;
    Ok(format!(
        "OK sent {} bytes as {} zlib bytes in {} ms; the panel refreshes now (~30 s)",
        image.len(),
        stream.len(),
        started.elapsed().as_millis()
    ))
}

fn refused(error: client::ClientError, meaning: &str) -> String {
    match error {
        client::ClientError::Refused(..) => meaning.to_owned(),
        other => format!("unexpected answer: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_from_our_responder_describes_one_frame() {
        let service = mdns::Service {
            instance: "ODCAED44",
            host: "ODCAED44",
            port: 2446,
            address: [192, 168, 50, 204],
            txt: &["mac=94:a9:90:ca:ed:46", "tls=0", "fw=0.1"],
        };
        let mut query = [0u8; 64];
        let len = mdns::browse_query(1, &mut query).unwrap();
        let mut answer = [0u8; 512];
        let len = mdns::respond(&query[..len], &service, true, &mut answer).unwrap();
        let found = read_answer(&answer[..len]);
        assert_eq!(found.len(), 1);
        let frame = &found[0];
        assert_eq!(frame.instance, "ODCAED44");
        assert_eq!(frame.address, Ipv4Addr::new(192, 168, 50, 204));
        assert_eq!(frame.port, 2446);
        assert_eq!(frame.txt["mac"], "94:a9:90:ca:ed:46");
        assert_eq!(id_for(Some(&frame.txt["mac"]), "x"), "94a990caed46");
        assert_eq!(id_for(None, "192.168.50.204"), "ip-192-168-50-204");
    }

    #[test]
    fn basic_credentials_are_standard_base64() {
        assert_eq!(base64("x:hunter2hunter2"), "eDpodW50ZXIyaHVudGVyMg==");
        assert_eq!(base64("ab"), "YWI=");
    }
}
