//! The bridge as an OpenDisplay device.
//!
//! One selected frame is exposed on one TCP port. Clients such as Home
//! Assistant speak the normal OpenDisplay protocol to the bridge; the bridge
//! owns the protocol state and one frame behind it. This is deliberately not a
//! TCP proxy: a frame accepts one OpenDisplay client, and a new connection
//! evicts the old one, so the bridge serialises its own status reads and pushes
//! with the proxied session.
//!
//! The selected frame is configured in `bridge.toml` and controlled from the
//! Frames tab. Off by default: a bridge that announces the same panel as the
//! frame creates a second discovery answer that clients cannot tell apart.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use opendisplay_proto::config::{Blob, DataExtended, MAX_CONFIG_SIZE, PacketType};
use opendisplay_proto::server::{RefreshOutcome, Server};
use opendisplay_proto::wire::{self, DeepSleepError};
use opendisplay_proto::{
    Busy, Device, FirmwareVersion, RefreshMode, StorageError, board, lan, mdns, msd,
};
use socket2::{Domain, Protocol, Socket, Type};

use crate::frames::{self, Frame, Registry};
use crate::ui::SettingsState;

/// How long a client may be silent before it is dropped, as the spec says.
const IDLE: Duration = Duration::from_secs(wire::LAN_READ_TIMEOUT_S as u64);
/// Read poll: short enough to notice a newer client and a finished refresh.
const TICK: Duration = Duration::from_millis(10);
/// Socket read timeout for a connected client. A timeout is not an error; the
/// session keeps its partial frame and tries again.
const READ_TICK: Duration = Duration::from_millis(50);
/// A stalled write means the client is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Announcements after start, as RFC 6762 asks.
const ANNOUNCEMENTS: u8 = 2;
const ANNOUNCE_GAP: Duration = Duration::from_secs(1);

/// What the Frames tab and `/api/opendisplay` show.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub running: bool,
    pub instance: String,
    pub port: u16,
    /// The last start, mDNS or refresh failure; `None` when the listener is up.
    pub error: Option<String>,
}

impl Status {
    fn note(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }
}

/// The settings that decide whether a listener should run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Choice {
    enabled: bool,
    frame: String,
    name: String,
    addr: String,
}

impl Choice {
    fn read(settings: &Arc<RwLock<SettingsState>>) -> Self {
        let Ok(state) = settings.read() else {
            return Self::default();
        };
        let settings = &state.settings;
        Self {
            enabled: settings.opendisplay_enabled,
            frame: settings.opendisplay_frame.clone(),
            name: settings.opendisplay_name.clone(),
            addr: settings.opendisplay_addr.clone(),
        }
    }
}

/// A running listener; dropping it stops the worker and sends the mDNS goodbye.
struct Running {
    stop: Arc<AtomicBool>,
    join: thread::JoinHandle<()>,
    frames: Arc<Registry>,
}

impl Running {
    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.join.join();
        self.frames.unexpose();
    }
}

/// Watch the settings and keep one listener in step with them. The web UI does
/// not need to start or stop anything itself; it saves the file and this loop
/// notices within a second.
pub fn supervise(
    settings: Arc<RwLock<SettingsState>>,
    frames: Arc<Registry>,
    status: Arc<Mutex<Status>>,
) {
    let mut current: Option<(Choice, Running)> = None;
    loop {
        let wanted = Choice::read(&settings);
        let same = current
            .as_ref()
            .is_some_and(|(choice, _)| *choice == wanted);
        if !same {
            if let Some((_, running)) = current.take() {
                running.stop();
            }
            if let Ok(mut status) = status.lock() {
                *status = Status::default();
            }
            if wanted.enabled {
                match start(&wanted, &frames, &status) {
                    Ok((running, instance, warning)) => {
                        if let Ok(mut status) = status.lock() {
                            status.running = true;
                            status.instance = instance;
                            status.port = wanted
                                .addr
                                .rsplit_once(':')
                                .map_or(0, |(_, port)| port.parse().unwrap_or(0));
                            status.error = warning;
                        }
                        current = Some((wanted, running));
                    }
                    Err(error) => {
                        if let Ok(mut status) = status.lock() {
                            status.note(error);
                        }
                    }
                }
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// Bind and prepare discovery. The TCP listener is created here so a bad
/// address is reported to the UI instead of disappearing into a thread.
fn start(
    choice: &Choice,
    frames: &Arc<Registry>,
    status: &Arc<Mutex<Status>>,
) -> Result<(Running, String, Option<String>), String> {
    let addr: SocketAddr = choice
        .addr
        .parse()
        .map_err(|_| format!("{} is not an ip:port address", choice.addr))?;
    let listener =
        TcpListener::bind(addr).map_err(|error| format!("listen on {}: {error}", choice.addr))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("listen on {}: {error}", choice.addr))?;

    let frame = frames.get(&choice.frame);
    let instance = instance_name(&choice.name, frame.as_ref(), &choice.frame);
    let txt = txt_records(frame.as_ref(), &choice.frame);
    let ip = advertised_ipv4(addr);
    if let Some(ip) = ip {
        frames.expose(ip, port_of(addr));
    }
    let (discovery, warning) = match ip {
        Some(ip) => match Discovery::new(ip, &instance, &host_label(), port_of(addr), txt) {
            Ok(discovery) => (Some(discovery), None),
            Err(error) => (None, Some(format!("mDNS is off: {error}"))),
        },
        None => (
            None,
            Some("mDNS is off: no IPv4 address on the LAN".to_owned()),
        ),
    };

    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker_status = Arc::clone(status);
    let worker_frames = Arc::clone(frames);
    let frame_id = choice.frame.clone();
    let join = thread::spawn(move || {
        serve(
            listener,
            discovery,
            frame_id,
            worker_frames,
            worker_stop,
            worker_status,
        );
    });
    Ok((
        Running {
            stop,
            join,
            frames: Arc::clone(frames),
        },
        instance,
        warning,
    ))
}

fn port_of(addr: SocketAddr) -> u16 {
    addr.port()
}

/// The address to put in the mDNS A record. The listener may be on 0.0.0.0;
/// ask the routing table which source address it would use for a LAN host.
fn advertised_ipv4(addr: SocketAddr) -> Option<Ipv4Addr> {
    if let IpAddr::V4(ip) = addr.ip()
        && !ip.is_unspecified()
    {
        return Some(ip);
    }
    for target in ["192.168.50.1:80", "1.1.1.1:80"] {
        let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
            continue;
        };
        if socket.connect(target).is_ok()
            && let Ok(SocketAddr::V4(local)) = socket.local_addr()
            && !local.ip().is_unspecified()
        {
            return Some(*local.ip());
        }
    }
    None
}

/// The mDNS instance label: the configured name, or the frame's name, or the
/// frame id. No dots, because the crate appends the service name.
fn instance_name(configured: &str, frame: Option<&Frame>, id: &str) -> String {
    let base = if !configured.trim().is_empty() {
        configured
    } else if let Some(frame) = frame {
        &frame.name
    } else {
        id
    };
    let label = sanitize_label(base);
    if label.is_empty() {
        "Easellet".to_owned()
    } else {
        label
    }
}

fn sanitize_label(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.truncate(63);
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn host_label() -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .unwrap_or_default();
    let label = sanitize_label(host.trim());
    if label.is_empty() {
        "easellet-bridge".to_owned()
    } else {
        sanitize_label(&format!("easellet-{label}"))
    }
}

/// TXT keys the OpenDisplay client and Home Assistant read during discovery.
fn txt_records(frame: Option<&Frame>, id: &str) -> Vec<String> {
    let record = frame
        .and_then(|frame| frame.msd)
        .unwrap_or_else(|| msd::Telemetry::default().encode());
    let mac = frame
        .and_then(|frame| frame.mac.clone())
        .unwrap_or_else(|| synthetic_mac(id));
    vec![
        format!("mac={mac}"),
        "tls=0".to_owned(),
        format!("fw={}", env!("CARGO_PKG_VERSION")),
        format!(
            "cm={:02x}",
            opendisplay_proto::config::communication_mode::WIFI
        ),
        format!("pv={}", wire::PROTOCOL_VERSION),
        format!("msd={}", String::from_utf8_lossy(&msd::txt(&record))),
    ]
}

/// A stable locally-administered address for a frame known only by id.
fn synthetic_mac(id: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let bytes = hash.to_be_bytes();
    format!("02:45:41:53:{:02x}:{:02x}", bytes[2], bytes[3])
}

/// The worker: one client at a time, a new connection evicts the old one.
fn serve(
    listener: TcpListener,
    mut discovery: Option<Discovery>,
    frame_id: String,
    frames: Arc<Registry>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
) {
    let mut server = Server::new();
    let mut device = Proxy::new(frame_id, frames, status);
    let mut client: Option<Client> = None;
    let mut last = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if client.take().is_some() {
                    server.disconnected(&mut device);
                }
                if let Ok(session) = Client::new(stream) {
                    client = Some(session);
                    last = Instant::now();
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {}
        }

        let mut drop_client = false;
        if let Some(session) = client.as_mut() {
            if device.refreshing() {
                // A real refresh takes longer than the idle limit; the wait is
                // the panel's doing, not the client's.
                last = Instant::now();
            }
            match session.poll() {
                Ok(frames) => {
                    if !frames.is_empty() {
                        last = Instant::now();
                    }
                    for frame in frames {
                        let mut failed = false;
                        server.handle(&frame, &mut device, &mut |out| {
                            if session.send(out).is_err() {
                                failed = true;
                            }
                        });
                        if failed {
                            drop_client = true;
                            break;
                        }
                    }
                }
                Err(_) => drop_client = true,
            }
        }
        if drop_client {
            client = None;
            server.disconnected(&mut device);
        }
        if let Some(outcome) = device.refresh_outcome() {
            server.refresh_finished(outcome, &mut |out| {
                if let Some(session) = client.as_mut() {
                    let _ = session.send(out);
                }
            });
        }
        if client.is_some() && last.elapsed() > IDLE && !device.refreshing() {
            client = None;
            server.disconnected(&mut device);
        }
        if let Some(discovery) = discovery.as_mut() {
            discovery.poll();
        }
        thread::sleep(TICK);
    }
    if let Some(discovery) = discovery.as_mut() {
        discovery.send_announcement(true);
    }
}

/// A connected client, with its partial input frame kept between polls.
struct Client {
    stream: TcpStream,
    input: Vec<u8>,
}

impl Client {
    fn new(stream: TcpStream) -> Result<Self, String> {
        stream
            .set_read_timeout(Some(READ_TICK))
            .and_then(|()| stream.set_write_timeout(Some(WRITE_TIMEOUT)))
            .and_then(|()| stream.set_nodelay(true))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            stream,
            input: Vec::new(),
        })
    }

    fn send(&mut self, payload: &[u8]) -> Result<(), String> {
        let prefix = lan::prefix(payload).map_err(|_| "answer too long".to_owned())?;
        let mut frame = Vec::with_capacity(2 + payload.len());
        frame.extend_from_slice(&prefix);
        frame.extend_from_slice(payload);
        self.stream
            .write_all(&frame)
            .map_err(|error| error.to_string())
    }

    /// Read whatever arrived and return the complete request frames. A read
    /// timeout is normal: the client is waiting for its next instruction.
    fn poll(&mut self) -> Result<Vec<Vec<u8>>, String> {
        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk) {
            Ok(0) => return Err("the client closed the connection".to_owned()),
            Ok(len) => self.input.extend_from_slice(&chunk[..len]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.to_string()),
        }
        let mut frames = Vec::new();
        while let Some(prefix) = self.input.get(..2) {
            let len = lan::payload_len([prefix[0], prefix[1]])
                .map_err(|_| "bad frame length".to_owned())?;
            if self.input.len() < 2 + len {
                break;
            }
            let frame = self.input[2..2 + len].to_vec();
            self.input.drain(..2 + len);
            frames.push(frame);
        }
        Ok(frames)
    }
}

/// The device behind the bridge's OpenDisplay server: one selected frame.
struct Proxy {
    frame_id: String,
    frames: Arc<Registry>,
    status: Arc<Mutex<Status>>,
    names: Option<DataExtended>,
    config: Vec<u8>,
    image: Vec<u8>,
    refresh: Option<Receiver<RefreshOutcome>>,
}

impl Proxy {
    fn new(frame_id: String, frames: Arc<Registry>, status: Arc<Mutex<Status>>) -> Self {
        Self {
            frame_id,
            frames,
            status,
            names: None,
            config: vec![0; MAX_CONFIG_SIZE],
            image: vec![0; board::FRAME_LEN],
            refresh: None,
        }
    }

    fn target(&self) -> Option<Frame> {
        self.frames.get(&self.frame_id)
    }

    fn refreshing(&self) -> bool {
        self.refresh.is_some()
    }

    fn refresh_outcome(&mut self) -> Option<RefreshOutcome> {
        let receiver = self.refresh.as_ref()?;
        match receiver.try_recv() {
            Ok(outcome) => {
                self.refresh = None;
                Some(outcome)
            }
            Err(TryRecvError::Disconnected) => {
                self.refresh = None;
                Some(RefreshOutcome::TimedOut)
            }
            Err(TryRecvError::Empty) => None,
        }
    }

    fn label(&self) -> String {
        self.target()
            .map(|frame| frame.name)
            .unwrap_or_else(|| self.frame_id.clone())
    }
}

impl Device for Proxy {
    fn firmware_version(&self) -> FirmwareVersion<'_> {
        let mut parts = env!("CARGO_PKG_VERSION").split('.');
        FirmwareVersion {
            major: parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
            minor: parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
            patch: parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
            sha: env!("CARGO_PKG_NAME"),
        }
    }

    fn read_config(&mut self) -> Result<&[u8], StorageError> {
        let len = board::photopainter_config(&mut self.config, self.names.as_ref(), 0)
            .map_err(|_| StorageError)?;
        Ok(&self.config[..len])
    }

    fn write_config(&mut self, blob: Blob<'_>) -> Result<(), StorageError> {
        let Some(names) = blob.find::<DataExtended>(PacketType::DATA_EXTENDED) else {
            return Ok(());
        };
        self.names = Some(names.clone());
        Ok(())
    }

    fn clear_config(&mut self) -> Result<(), StorageError> {
        self.names = None;
        Ok(())
    }

    fn msd(&mut self) -> [u8; 16] {
        self.target()
            .and_then(|frame| frame.msd)
            .unwrap_or_else(|| msd::Telemetry::default().encode())
    }

    fn reboot(&mut self) {
        if let Some((host, port, _)) = self.frames.target(&self.frame_id)
            && let Err(error) = frames::reboot(&host, port)
            && let Ok(mut status) = self.status.lock()
        {
            status.note(format!("reboot {}: {error}", self.label()));
        }
    }

    fn deep_sleep(&mut self, override_secs: Option<NonZeroU16>) -> Result<(), DeepSleepError> {
        let Some((host, port, _)) = self.frames.target(&self.frame_id) else {
            return Err(DeepSleepError::Unsupported);
        };
        frames::deep_sleep(&host, port, override_secs).map_err(|error| {
            if let Ok(mut status) = self.status.lock() {
                status.note(format!("deep sleep {}: {error}", self.label()));
            }
            DeepSleepError::Unsupported
        })
    }

    fn begin_image(&mut self) -> Result<(), Busy> {
        let Some(frame) = self.target() else {
            return Err(Busy);
        };
        let Some(panel) = frame.panel else {
            return Err(Busy);
        };
        if !frame.online
            || usize::from(panel.width) * usize::from(panel.height) / 2 != self.image.len()
            || self.refresh.is_some()
        {
            return Err(Busy);
        }
        self.image.fill(0);
        Ok(())
    }

    fn frame(&mut self) -> &mut [u8] {
        &mut self.image
    }

    fn end_image(&mut self, _refresh: RefreshMode) -> Result<(), Busy> {
        if self.refresh.is_some() {
            return Err(Busy);
        }
        let Some((host, port, _)) = self.frames.target(&self.frame_id) else {
            return Err(Busy);
        };
        let image = self.image.clone();
        let (sender, receiver) = mpsc::channel();
        let status = Arc::clone(&self.status);
        let label = self.label();
        thread::spawn(move || {
            let outcome = match frames::display_wait(&host, port, &image) {
                Ok(outcome) => {
                    if let Ok(mut status) = status.lock()
                        && status
                            .error
                            .as_deref()
                            .is_some_and(|error| error.starts_with("refresh "))
                    {
                        status.error = None;
                    }
                    outcome
                }
                Err(error) => {
                    if let Ok(mut status) = status.lock() {
                        status.note(format!("refresh {label}: {error}"));
                    }
                    RefreshOutcome::TimedOut
                }
            };
            let _ = sender.send(outcome);
        });
        self.refresh = Some(receiver);
        Ok(())
    }

    fn abort_image(&mut self) {}
}

/// The mDNS responder for the bridge's one service.
struct Discovery {
    socket: UdpSocket,
    instance: String,
    host: String,
    port: u16,
    address: [u8; 4],
    txt: Vec<String>,
    announced: u8,
    next_announcement: Instant,
    packet: Vec<u8>,
    answer: Vec<u8>,
}

impl Discovery {
    fn new(
        address: Ipv4Addr,
        instance: &str,
        host: &str,
        port: u16,
        txt: Vec<String>,
    ) -> Result<Self, String> {
        let group = Ipv4Addr::from(mdns::GROUP);
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .map_err(|error| error.to_string())?;
        socket
            .set_reuse_address(true)
            .map_err(|error| error.to_string())?;
        socket
            .bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, mdns::PORT)).into())
            .map_err(|error| format!("bind {}: {error}", mdns::PORT))?;
        socket
            .join_multicast_v4(&group, &address)
            .map_err(|error| format!("join {group}: {error}"))?;
        socket
            .set_multicast_ttl_v4(255)
            .and_then(|()| socket.set_nonblocking(true))
            .map_err(|error| error.to_string())?;
        let socket: UdpSocket = socket.into();
        Ok(Self {
            socket,
            instance: instance.to_owned(),
            host: host.to_owned(),
            port,
            address: address.octets(),
            txt,
            announced: 0,
            next_announcement: Instant::now(),
            packet: vec![0; 1500],
            answer: vec![0; 1500],
        })
    }

    fn poll(&mut self) {
        if self.announced < ANNOUNCEMENTS && Instant::now() >= self.next_announcement {
            self.send_announcement(false);
            self.announced += 1;
            self.next_announcement = Instant::now() + ANNOUNCE_GAP;
        }
        loop {
            match self.socket.recv_from(&mut self.packet) {
                Ok((len, from)) => {
                    let instance = self.instance.clone();
                    let host = self.host.clone();
                    let txt: Vec<&str> = self.txt.iter().map(String::as_str).collect();
                    let service = mdns::Service {
                        instance: &instance,
                        host: &host,
                        port: self.port,
                        address: self.address,
                        txt: &txt,
                    };
                    let legacy = from.port() != mdns::PORT;
                    if let Some(answer_len) =
                        mdns::respond(&self.packet[..len], &service, legacy, &mut self.answer)
                    {
                        let _ = self.socket.send_to(&self.answer[..answer_len], from);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    fn send_announcement(&mut self, goodbye: bool) {
        let instance = self.instance.clone();
        let host = self.host.clone();
        let txt: Vec<&str> = self.txt.iter().map(String::as_str).collect();
        let service = mdns::Service {
            instance: &instance,
            host: &host,
            port: self.port,
            address: self.address,
            txt: &txt,
        };
        if let Ok(len) = mdns::announce(&service, goodbye, &mut self.answer) {
            let _ = self.socket.send_to(
                &self.answer[..len],
                SocketAddr::from((Ipv4Addr::from(mdns::GROUP), mdns::PORT)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendisplay_proto::config::{self, Blob};
    use opendisplay_proto::config::{DisplayConfig, PacketType};

    fn proxy() -> Proxy {
        Proxy::new(
            "frame".to_owned(),
            Arc::new(Registry::default()),
            Arc::new(Mutex::new(Status::default())),
        )
    }

    #[test]
    fn a_configured_name_becomes_one_dns_label() {
        assert_eq!(instance_name("", None, "aa-bb"), "aa-bb");
        assert_eq!(instance_name("Kitchen frame", None, "aa"), "Kitchen-frame");
        assert_eq!(instance_name("", None, "frame"), "frame");
        assert_eq!(instance_name("...", None, ""), "Easellet");
        assert!(instance_name(&"x".repeat(100), None, "").len() <= 63);
    }

    #[test]
    fn the_config_is_a_photopainter_blob() {
        let mut proxy = proxy();
        let bytes = proxy.read_config().unwrap().to_vec();
        let blob = Blob::parse(&bytes).unwrap();
        let display: &DisplayConfig = blob.find(PacketType::DISPLAY).unwrap();
        assert_eq!(display.pixel_width.get(), 800);
        assert_eq!(display.pixel_height.get(), 480);
    }

    #[test]
    fn a_client_can_rename_the_bridge_display() {
        let mut proxy = proxy();
        let mut names = board::default_names();
        config::set_text(&mut names.friendly_name, "Kitchen");
        let mut buffer = vec![0; MAX_CONFIG_SIZE];
        let len = board::photopainter_config(&mut buffer, Some(&names), 0).unwrap();
        let blob = Blob::parse(&buffer[..len]).unwrap();
        proxy.write_config(blob).unwrap();
        let bytes = proxy.read_config().unwrap().to_vec();
        let blob = Blob::parse(&bytes).unwrap();
        let names: &DataExtended = blob.find(PacketType::DATA_EXTENDED).unwrap();
        assert_eq!(config::text(&names.friendly_name), Some("Kitchen"));
        proxy.clear_config().unwrap();
        let bytes = proxy.read_config().unwrap().to_vec();
        let blob = Blob::parse(&bytes).unwrap();
        let names: &DataExtended = blob.find(PacketType::DATA_EXTENDED).unwrap();
        assert_eq!(config::text(&names.friendly_name), Some("easellet"));
    }

    #[test]
    fn a_frame_that_is_not_known_or_online_is_busy() {
        let mut proxy = proxy();
        assert_eq!(proxy.begin_image(), Err(Busy));
    }
}
