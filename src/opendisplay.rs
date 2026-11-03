//! OpenDisplay over the LAN: TCP port 2446, so py-opendisplay, the Home Assistant
//! integration and the opendisplay.org toolbox can drive the frame unchanged.
//!
//! The protocol itself is the host-tested `opendisplay-proto` crate; this file
//! is the transport and the glue, arranged like the web portal:
//!
//! - **One thread, one client.** The thread owns the listener, the protocol
//!   state and a staging frame. A new connection evicts the old one, and a
//!   client silent for 30 s is dropped, both as the spec requires.
//! - **The card and the panel keep one owner each.** The staging frame fills
//!   here; the finished picture, the stored names and the "is the panel free"
//!   question all go to the main loop as a [`Job`]. The ~31 s refresh then runs
//!   on the paint worker, and its result comes back on a channel so the client
//!   gets its 0x73 (done) or 0x74 (timed out).
//! - **Clients may rename the frame, nothing more.** The config a client reads
//!   is generated from the board (`opendisplay_proto::board`); of what it
//!   writes, only the names packet is kept, in [`NAMES_FILE`].
//! - **Discovery is built in.** The same thread answers mDNS for
//!   `_opendisplay._tcp` (the crate's `mdns` responder, a few KB instead of
//!   IDF's 15-30 KB component), so Home Assistant finds the frame without an
//!   address. Names are `OD` plus the chip id, as upstream firmware uses.
//!
//! Frames are `[len:2 LE][payload]`, payload 1..=4094 bytes, no TLS.

use std::ffi::CStr;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::sys::{self, EspError};
use opendisplay_proto::config::{Blob, DataExtended, PacketType};
use opendisplay_proto::server::RefreshOutcome;
use opendisplay_proto::wire::{self, DeepSleepError};
use opendisplay_proto::{
    Busy, Device, FirmwareVersion, RefreshMode, Server, StorageError, board, lan, mdns, msd,
};
use zerocopy::{FromBytes, IntoBytes};

/// The spec's default plaintext port.
pub const PORT: u16 = wire::LAN_TCP_PORT;

/// Card-relative file holding the names packet a client wrote: the raw
/// 288-byte `DataExtended`. Under `EASELLET`, the one directory the firmware writes.
pub const NAMES_FILE: &str = "EASELLET/OPENDISP.BIN";

/// Accept-loop tick while nobody is connected.
const TICK: Duration = Duration::from_millis(25);

/// Socket read timeout: how often a connected session looks up from the socket
/// to check for a newer client, a finished refresh or `stop`.
const READ_TICK: Duration = Duration::from_millis(50);

/// A client silent this long is dropped.
const IDLE: Duration = Duration::from_secs(wire::LAN_READ_TIMEOUT_S as u64);

/// A stalled write means the client is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a request waits for the main loop. It answers within a loop turn
/// unless a console command is running.
const JOB_WAIT: Duration = Duration::from_secs(30);

/// Work for the main loop, answered on `reply`.
pub enum Job {
    /// The stored names packet, if a client wrote one.
    LoadNames {
        reply: Sender<Result<Option<Vec<u8>>, String>>,
    },
    /// Store a names packet, or forget it with `None`.
    SaveNames {
        names: Option<Vec<u8>>,
        reply: Sender<Result<(), String>>,
    },
    /// True when the panel could take a picture now.
    PanelFree { reply: Sender<bool> },
    /// Paint this frame. The buffer always comes back: `Ok` once the refresh
    /// has started (its result follows on `done`), `Err` if the panel is busy.
    Show {
        frame: Vec<u8>,
        done: Sender<Result<String, String>>,
        reply: Sender<Result<Vec<u8>, Vec<u8>>>,
    },
    /// A client asked for a restart. The main loop does it between its own
    /// card writes, never in the middle of one, and only if `opendisplay.reboot`
    /// allows it.
    Reboot,
    /// A client asked the frame to deep-sleep, optionally for this many seconds
    /// instead of `power.sleep_secs`.
    DeepSleep(Option<u32>),
}

/// What the frame last measured, as the MSD record carries it. The main loop
/// owns the I2C bus and publishes here; the server reads it for `ReadMsd` and
/// the mDNS `msd` key. Starts with the reboot flag set, as the spec asks.
static TELEMETRY: Mutex<msd::Telemetry> = Mutex::new(msd::Telemetry {
    battery_mv: None,
    chip_temp_c: None,
    ambient: None,
    rebooted: true,
    counter: 0,
});

/// `power.sleep_secs`, for the config's `deep_sleep_time_seconds`.
static SLEEP_SECS: AtomicU16 = AtomicU16::new(0);

/// New readings from the main loop. Each one advances the record's counter, so
/// a client can tell a fresh record from a repeat.
pub fn publish(battery_mv: Option<u16>, chip_temp_c: Option<i8>, ambient: Option<msd::Ambient>) {
    if let Ok(mut telemetry) = TELEMETRY.lock() {
        telemetry.battery_mv = battery_mv;
        telemetry.chip_temp_c = chip_temp_c;
        telemetry.ambient = ambient;
        telemetry.counter = telemetry.counter.wrapping_add(1);
    }
}

/// The timed wake a client should expect, from the settings; longer than the
/// field can say is reported as its maximum.
pub fn set_sleep_secs(secs: u32) {
    SLEEP_SECS.store(u16::try_from(secs).unwrap_or(u16::MAX), Ordering::Relaxed);
}

fn record() -> [u8; msd::LEN] {
    TELEMETRY.lock().map_or_else(
        |_| msd::Telemetry::default().encode(),
        |telemetry| telemetry.encode(),
    )
}

/// The spec's identity is the advertised BLE address, which ESP32 parts derive
/// as base MAC + 2, even before the BLE transport exists.
pub fn ble_mac(mac: [u8; 6]) -> [u8; 6] {
    let value = (u64::from_be_bytes([0, 0, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]]) + 2)
        & 0xFFFF_FFFF_FFFF;
    let bytes = value.to_be_bytes();
    [bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]]
}

/// The id the bridge knows this frame by: the announced MAC without colons.
pub fn frame_id(mac: [u8; 6]) -> String {
    ble_mac(mac)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The running server; dropping it stops the thread.
pub struct Handle {
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
}

impl Handle {
    /// A client is connected. The frame does not sleep under one: it would
    /// drop the session, and a client waiting for its 0x73 would never get it.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Bind and start serving. The radio must already be up; `identity` is the
/// station's address and MAC for discovery. Returns the handle and a note on
/// discovery, which may fail on its own without stopping the server.
pub fn start(
    jobs: Sender<Job>,
    identity: Option<(Ipv4Addr, [u8; 6])>,
    mdns_enabled: bool,
) -> Result<(Handle, String), String> {
    let listener = TcpListener::bind(("0.0.0.0", PORT))
        .map_err(|error| format!("ERR opendisplay bind 0.0.0.0:{PORT}: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("ERR opendisplay nonblocking: {error}"))?;
    let discovery = if mdns_enabled {
        match identity {
            Some((ip, mac)) => Discovery::new(ip, mac),
            None => Err("no address yet".to_owned()),
        }
    } else {
        Err("disabled by opendisplay.mdns=0".to_owned())
    };
    let note = match &discovery {
        Ok(discovery) => format!("mdns={}.local", discovery.name),
        Err(error) => format!("mdns=off ({error})"),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let connected = Arc::new(AtomicBool::new(false));
    let session = Arc::clone(&connected);
    // Building the ~15 KB protocol state peaks at about 26 KB of stack (see
    // `new_server`); 24 KB overflowed and reset the chip in a loop. The frame
    // itself lives in PSRAM, and so does this stack.
    spawn_in_psram(c"opendisplay", 40 * 1024, move || {
        serve(listener, discovery.ok(), jobs, flag, session)
    })?;
    Ok((Handle { stop, connected }, note))
}

/// Spawn a thread whose stack is in PSRAM rather than internal RAM, which Wi-Fi
/// and (later) BLE need. IDF takes the stack's memory caps from the *calling*
/// thread's pthread config, so it is set for this one spawn and then put back.
///
/// Only for threads that never run with the flash cache off (no flash writes,
/// no IRAM-only callbacks) and never hand stack buffers to DMA: this one only
/// moves bytes through lwIP sockets and channels.
fn spawn_in_psram(
    name: &'static CStr,
    stack_size: usize,
    body: impl FnOnce() + Send + 'static,
) -> Result<(), String> {
    let mut previous = sys::esp_pthread_cfg_t::default();
    // SAFETY: `previous` is a valid, writable config for IDF to fill in.
    let had_config = unsafe { sys::esp_pthread_get_cfg(&mut previous) } == sys::ESP_OK;
    // SAFETY: returns a config by value, no pointers involved.
    let default = unsafe { sys::esp_pthread_get_default_config() };
    let config = sys::esp_pthread_cfg_t {
        stack_size,
        // IDF keeps the pointer; `name` is 'static.
        thread_name: name.as_ptr(),
        stack_alloc_caps: sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT,
        ..default
    };
    // SAFETY: IDF copies the config; the name it points to is 'static.
    EspError::convert(unsafe { sys::esp_pthread_set_cfg(&config) })
        .map_err(|error| format!("ERR opendisplay thread config: {error}"))?;
    let spawned = thread::Builder::new().stack_size(stack_size).spawn(body);
    let restore = if had_config { &previous } else { &default };
    // SAFETY: as above; `previous` came from IDF, `default` is IDF's own.
    let restored = EspError::convert(unsafe { sys::esp_pthread_set_cfg(restore) });
    spawned.map_err(|error| format!("ERR opendisplay thread: {error}"))?;
    restored.map_err(|error| format!("ERR opendisplay thread config restore: {error}"))
}

/// The protocol state, on the heap. Kept out of line so its construction
/// (`Server::new`'s 10 KB frame plus the 15 KB value before it moves to the
/// heap) costs stack once, here, instead of staying in `serve`'s frame for the
/// thread's whole life.
#[inline(never)]
fn new_server() -> Box<Server> {
    Box::new(Server::new())
}

fn serve(
    listener: TcpListener,
    mut discovery: Option<Discovery>,
    jobs: Sender<Job>,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
) {
    let mut server = new_server();
    let mut device = Frame::new(jobs);
    let mut client: Option<Client> = None;
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                // One client at a time: the newcomer wins.
                if client.take().is_some() {
                    server.disconnected(&mut device);
                }
                client = Client::new(stream).ok();
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(_) => {}
        }
        if let Some(session) = client.as_mut() {
            // A client waiting for its refresh result sends nothing for the
            // ~31 s the panel takes, which is longer than the 30 s idle limit:
            // the wait is the device's doing, so it does not count as idle.
            if device.refresh.is_some() {
                session.last = Instant::now();
            }
        }
        if let Some(session) = client.as_mut()
            && session.poll(&mut server, &mut device).is_err()
        {
            server.disconnected(&mut device);
            client = None;
        }
        if let Some(outcome) = device.refresh_outcome() {
            server.refresh_finished(outcome, &mut |frame| {
                if let Some(session) = client.as_mut() {
                    // A failed write shows up as an error on the next read.
                    let _ = session.send(frame);
                }
            });
        }
        if let Some(discovery) = discovery.as_mut() {
            discovery.poll();
        }
        connected.store(client.is_some(), Ordering::Relaxed);
        if client.is_none() {
            thread::sleep(TICK);
        }
    }
    // Tell caches the service is gone rather than letting it linger for an hour.
    if let Some(discovery) = discovery.as_mut() {
        discovery.send_announcement(true);
    }
}

/// Announcements after start: RFC 6762 asks for at least two, a second apart.
const ANNOUNCEMENTS: u8 = 2;
const ANNOUNCE_GAP: Duration = Duration::from_secs(1);

/// The mDNS responder for this one service, polled from [`serve`].
struct Discovery {
    socket: UdpSocket,
    /// Host and instance label, `OD` plus the low three bytes of the chip MAC.
    name: String,
    address: [u8; 4],
    txt: Vec<String>,
    announced: u8,
    next_announcement: Instant,
    packet: Vec<u8>,
    answer: Vec<u8>,
}

impl Discovery {
    fn new(ip: Ipv4Addr, mac: [u8; 6]) -> Result<Self, String> {
        let group = Ipv4Addr::from(mdns::GROUP);
        let socket = UdpSocket::bind(("0.0.0.0", mdns::PORT))
            .map_err(|error| format!("bind {}: {error}", mdns::PORT))?;
        socket
            .join_multicast_v4(&group, &ip)
            .map_err(|error| format!("join {group}: {error}"))?;
        socket
            .set_multicast_ttl_v4(255)
            .and_then(|()| socket.set_nonblocking(true))
            .map_err(|error| format!("socket: {error}"))?;
        let ble = ble_mac(mac)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(":");
        let txt = vec![
            format!("mac={ble}"),
            "tls=0".to_owned(),
            format!(
                "fw={}.{}",
                env!("CARGO_PKG_VERSION_MAJOR"),
                env!("CARGO_PKG_VERSION_MINOR")
            ),
            format!(
                "cm={:02x}",
                opendisplay_proto::config::communication_mode::WIFI
            ),
            format!("pv={}", wire::PROTOCOL_VERSION),
        ];
        Ok(Self {
            socket,
            name: format!("OD{:02X}{:02X}{:02X}", mac[3], mac[4], mac[5]),
            address: ip.octets(),
            txt,
            announced: 0,
            next_announcement: Instant::now(),
            packet: vec![0; 1500],
            answer: vec![0; 512],
        })
    }

    /// The TXT keys with the current telemetry as `msd` (spec section 9: the
    /// MSD record without its company id, 28 hex digits).
    fn txt_now(&self) -> Vec<String> {
        let digits = msd::txt(&record());
        let mut txt = self.txt.clone();
        txt.push(format!("msd={}", String::from_utf8_lossy(&digits)));
        txt
    }

    /// Send any due announcement and answer the queries that have arrived.
    fn poll(&mut self) {
        if self.announced < ANNOUNCEMENTS && Instant::now() >= self.next_announcement {
            self.send_announcement(false);
            self.announced += 1;
            self.next_announcement = Instant::now() + ANNOUNCE_GAP;
        }
        // Bounded, so a flood of multicast cannot starve the TCP client.
        for _ in 0..8 {
            let Ok((len, from)) = self.socket.recv_from(&mut self.packet) else {
                return;
            };
            let legacy = from.port() != mdns::PORT;
            let txt = self.txt_now();
            let txt: Vec<&str> = txt.iter().map(String::as_str).collect();
            let service = mdns::Service {
                instance: &self.name,
                host: &self.name,
                port: PORT,
                address: self.address,
                txt: &txt,
            };
            if let Some(answer) =
                mdns::respond(&self.packet[..len], &service, legacy, &mut self.answer)
            {
                // A legacy resolver gets its answer directly; everyone else
                // through the group, so all caches stay in step.
                let to = if legacy { from } else { group() };
                let _ = self.socket.send_to(&self.answer[..answer], to);
            }
        }
    }

    fn send_announcement(&mut self, goodbye: bool) {
        let txt = self.txt_now();
        let txt: Vec<&str> = txt.iter().map(String::as_str).collect();
        let service = mdns::Service {
            instance: &self.name,
            host: &self.name,
            port: PORT,
            address: self.address,
            txt: &txt,
        };
        if let Ok(len) = mdns::announce(&service, goodbye, &mut self.answer) {
            let _ = self.socket.send_to(&self.answer[..len], group());
        }
    }
}

fn group() -> SocketAddr {
    SocketAddr::from((mdns::GROUP, mdns::PORT))
}

/// One connected client and the frame being read from it.
struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
    have: usize,
    payload: Option<usize>,
    last: Instant,
}

impl Client {
    fn new(stream: TcpStream) -> std::io::Result<Self> {
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(READ_TICK))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            buf: vec![0; wire::LAN_MAX_FRAME],
            have: 0,
            payload: None,
            last: Instant::now(),
        })
    }

    /// Read what has arrived; handle the frame once it is whole. An error
    /// means the session is over.
    fn poll(&mut self, server: &mut Server, device: &mut Frame) -> std::io::Result<()> {
        let want = 2 + self.payload.unwrap_or(0);
        match self.stream.read(&mut self.buf[self.have..want]) {
            Ok(0) => return Err(ErrorKind::ConnectionAborted.into()),
            Ok(count) => {
                self.have += count;
                self.last = Instant::now();
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return if self.last.elapsed() > IDLE {
                    Err(ErrorKind::TimedOut.into())
                } else {
                    Ok(())
                };
            }
            Err(error) => return Err(error),
        }
        if self.have < want {
            return Ok(());
        }
        let Some(len) = self.payload else {
            // A bad length leaves the stream out of step: drop the client.
            let len = lan::payload_len([self.buf[0], self.buf[1]])
                .map_err(|_| std::io::Error::from(ErrorKind::InvalidData))?;
            self.payload = Some(len);
            return Ok(());
        };
        let Self { stream, buf, .. } = self;
        let mut failed = None;
        server.handle(&buf[2..2 + len], device, &mut |response| {
            if failed.is_none()
                && let Err(error) = write_frame(stream, response)
            {
                failed = Some(error);
            }
        });
        self.have = 0;
        self.payload = None;
        failed.map_or(Ok(()), Err)
    }

    fn send(&mut self, response: &[u8]) -> std::io::Result<()> {
        write_frame(&mut self.stream, response)
    }
}

/// Prefix and payload in one write, so a response is one TCP segment.
fn write_frame(stream: &mut TcpStream, response: &[u8]) -> std::io::Result<()> {
    let prefix =
        lan::prefix(response).map_err(|_| std::io::Error::from(ErrorKind::InvalidInput))?;
    let mut frame = [0_u8; 2 + wire::MAX_RESPONSE_DATA_SIZE];
    let end = 2 + response.len();
    frame[..2].copy_from_slice(&prefix);
    frame[2..end].copy_from_slice(response);
    stream.write_all(&frame[..end])
}

/// The [`Device`] the protocol sees: a staging frame on this thread, and
/// everything else asked of the main loop.
struct Frame {
    jobs: Sender<Job>,
    /// Allocated on the first image and kept; 192 KB, so it lands in PSRAM.
    frame: Vec<u8>,
    config: Vec<u8>,
    refresh: Option<Receiver<Result<String, String>>>,
    major: u8,
    minor: u8,
    patch: u8,
}

impl Frame {
    fn new(jobs: Sender<Job>) -> Self {
        let part = |text: &str| text.parse().unwrap_or(0);
        Self {
            jobs,
            frame: Vec::new(),
            config: vec![0; opendisplay_proto::config::MAX_CONFIG_SIZE],
            refresh: None,
            major: part(env!("CARGO_PKG_VERSION_MAJOR")),
            minor: part(env!("CARGO_PKG_VERSION_MINOR")),
            patch: part(env!("CARGO_PKG_VERSION_PATCH")),
        }
    }

    /// Ask the main loop and wait for its answer. `None` if it did not answer.
    fn ask<T>(&self, job: impl FnOnce(Sender<T>) -> Job) -> Option<T> {
        let (reply, answer) = mpsc::channel();
        self.jobs.send(job(reply)).ok()?;
        answer.recv_timeout(JOB_WAIT).ok()
    }

    /// The result of the last refresh, once it is known.
    fn refresh_outcome(&mut self) -> Option<RefreshOutcome> {
        let outcome = match self.refresh.as_ref()?.try_recv() {
            Err(TryRecvError::Empty) => return None,
            Ok(Ok(_)) => RefreshOutcome::Done,
            // A failed paint, or a paint thread that vanished: either way the
            // panel did not show the picture in time.
            Ok(Err(_)) | Err(TryRecvError::Disconnected) => RefreshOutcome::TimedOut,
        };
        self.refresh = None;
        Some(outcome)
    }
}

impl Device for Frame {
    fn firmware_version(&self) -> FirmwareVersion<'_> {
        FirmwareVersion {
            major: self.major,
            minor: self.minor,
            patch: self.patch,
            // No VCS id is available at build time; the client requires a
            // non-empty hash, so the crate name stands in.
            sha: env!("CARGO_PKG_NAME"),
        }
    }

    fn read_config(&mut self) -> Result<&[u8], StorageError> {
        let stored = self
            .ask(|reply| Job::LoadNames { reply })
            .ok_or(StorageError)?
            .map_err(|_| StorageError)?;
        let names = stored
            .as_deref()
            .and_then(|bytes| DataExtended::ref_from_bytes(bytes).ok());
        self.config
            .resize(opendisplay_proto::config::MAX_CONFIG_SIZE, 0);
        let len =
            board::photopainter_config(&mut self.config, names, SLEEP_SECS.load(Ordering::Relaxed))
                .map_err(|_| StorageError)?;
        Ok(&self.config[..len])
    }

    fn write_config(&mut self, blob: Blob<'_>) -> Result<(), StorageError> {
        // Hardware packets are generated, never stored; a blob without names
        // leaves the stored ones alone.
        let Some(names) = blob.find::<DataExtended>(PacketType::DATA_EXTENDED) else {
            return Ok(());
        };
        let names = Some(names.as_bytes().to_vec());
        match self.ask(|reply| Job::SaveNames { names, reply }) {
            Some(Ok(())) => Ok(()),
            _ => Err(StorageError),
        }
    }

    fn clear_config(&mut self) -> Result<(), StorageError> {
        match self.ask(|reply| Job::SaveNames { names: None, reply }) {
            Some(Ok(())) => Ok(()),
            _ => Err(StorageError),
        }
    }

    fn msd(&mut self) -> [u8; 16] {
        let record = record();
        // A client has now seen the reboot; later records drop the flag.
        if let Ok(mut telemetry) = TELEMETRY.lock() {
            telemetry.rebooted = false;
        }
        record
    }

    fn reboot(&mut self) {
        // Restarting here could cut a settings or upload write on the main
        // loop in half; the loop restarts between jobs instead. The protocol
        // sends no reply either way.
        let _ = self.jobs.send(Job::Reboot);
    }

    fn deep_sleep(&mut self, override_secs: Option<NonZeroU16>) -> Result<(), DeepSleepError> {
        // The main loop quietens the board and sleeps once this client is gone;
        // the protocol expects no reply, and the session ends with the sleep.
        self.jobs
            .send(Job::DeepSleep(
                override_secs.map(|secs| u32::from(secs.get())),
            ))
            .map_err(|_| DeepSleepError::Unsupported)
    }

    fn begin_image(&mut self) -> Result<(), Busy> {
        // Refuse early rather than after the whole upload: the panel stays busy
        // for the ~31 s of a refresh.
        if self.ask(|reply| Job::PanelFree { reply }) != Some(true) {
            return Err(Busy);
        }
        self.frame.resize(board::FRAME_LEN, 0);
        Ok(())
    }

    fn frame(&mut self) -> &mut [u8] {
        &mut self.frame
    }

    fn end_image(&mut self, _refresh: RefreshMode) -> Result<(), Busy> {
        // Spectra 6 has one waveform: fast and full refresh are the same here.
        let (done, result) = mpsc::channel();
        let frame = std::mem::take(&mut self.frame);
        let (reply, answer) = mpsc::channel();
        if let Err(mpsc::SendError(Job::Show { frame, .. })) =
            self.jobs.send(Job::Show { frame, done, reply })
        {
            self.frame = frame;
            return Err(Busy);
        }
        match answer.recv_timeout(JOB_WAIT) {
            Ok(Ok(frame)) => {
                self.frame = frame;
                self.refresh = Some(result);
                Ok(())
            }
            Ok(Err(frame)) => {
                self.frame = frame;
                Err(Busy)
            }
            // The loop kept the buffer; the next image allocates a fresh one.
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => Err(Busy),
        }
    }

    fn abort_image(&mut self) {}
}

/// The names packet stored on the card, if any. Runs on the main loop.
pub fn load_names() -> Result<Option<Vec<u8>>, String> {
    let path = Path::new("/sdcard").join(NAMES_FILE);
    match fs::read(&path) {
        Ok(bytes) if bytes.len() == size_of::<DataExtended>() => Ok(Some(bytes)),
        // A file of the wrong size is not ours to interpret: report defaults.
        Ok(_) => Ok(None),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("ERR read {}: {error}", path.display())),
    }
}

/// Store or remove the names packet. Runs on the main loop.
pub fn save_names(names: Option<&[u8]>) -> Result<(), String> {
    let path = Path::new("/sdcard").join(NAMES_FILE);
    match names {
        Some(bytes) => {
            if let Some(directory) = path.parent() {
                fs::create_dir_all(directory)
                    .map_err(|error| format!("ERR create {}: {error}", directory.display()))?;
            }
            fs::write(&path, bytes)
                .map_err(|error| format!("ERR write {}: {error}", path.display()))
        }
        None => match fs::remove_file(&path) {
            Err(error) if error.kind() != ErrorKind::NotFound => {
                Err(format!("ERR remove {}: {error}", path.display()))
            }
            _ => Ok(()),
        },
    }
}
