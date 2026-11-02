//! The request dispatcher, without security (spec section "AUTH GATING" is for a
//! later phase: with no key configured every command is open).
//!
//! One [`Server`] serves one connection at a time. The transport feeds it
//! request frames with [`Server::handle`] and sends each frame passed to `out`.
//! A refresh outlives its request, so the firmware reports the result with
//! [`Server::refresh_finished`], which emits the unsolicited 0x73 or 0x74.

use core::num::NonZeroU16;

use crate::config::{Blob, MAX_CONFIG_SIZE};
use crate::image::ImageReceiver;
use crate::request::{DirectWriteStart, ParseError, RefreshMode, Request};
use crate::response::Response;
use crate::wire::{
    CONFIG_CHUNK_SIZE, CONFIG_CHUNK_SIZE_WITH_PREFIX, DeepSleepError, MAX_CONFIG_CHUNKS,
    MAX_RESPONSE_DATA_SIZE, Opcode, PartialWriteError, PowerOffError, RESP_ACK, RESP_NACK,
    RESP_REFRESH_SUCCESS, RESP_REFRESH_TIMEOUT, auth_status,
};

/// The answer to CMD_FIRMWARE_VERSION.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareVersion<'a> {
    /// Major version.
    pub major: u8,
    /// Minor version.
    pub minor: u8,
    /// Patch version.
    pub patch: u8,
    /// Build hash in ASCII; only the first 40 bytes are sent. Must not be
    /// empty: py-opendisplay rejects a version without a hash.
    pub sha: &'a str,
}

/// Config storage failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageError;

/// The panel is in use by something else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Busy;

/// How a refresh ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The panel shows the new image.
    Done,
    /// The panel did not finish in time.
    TimedOut,
}

/// What the firmware provides. Calls happen inside [`Server::handle`], on the
/// transport's thread.
pub trait Device {
    /// Version reported by CMD_FIRMWARE_VERSION.
    fn firmware_version(&self) -> FirmwareVersion<'_>;

    /// The current config blob, header and CRC included, at most
    /// [`MAX_CONFIG_SIZE`] bytes. An empty blob means "no config".
    fn read_config(&mut self) -> Result<&[u8], StorageError>;

    /// Apply a config the client wrote. Its CRC and packet framing have been
    /// checked; which packets to honour is up to the device.
    fn write_config(&mut self, blob: Blob<'_>) -> Result<(), StorageError>;

    /// Forget the stored config.
    fn clear_config(&mut self) -> Result<(), StorageError>;

    /// The 16-byte manufacturer-specific data record.
    fn msd(&mut self) -> [u8; 16];

    /// Restart now. The spec sends no response.
    fn reboot(&mut self);

    /// Enter deep sleep, or say why not. Success sends no response.
    fn deep_sleep(&mut self, override_secs: Option<NonZeroU16>) -> Result<(), DeepSleepError>;

    /// Claim the panel's frame buffer for a new image.
    fn begin_image(&mut self) -> Result<(), Busy>;

    /// The claimed frame buffer, exactly one frame long, in the panel's byte
    /// order. Called only between [`begin_image`](Self::begin_image) and
    /// [`end_image`](Self::end_image) or [`abort_image`](Self::abort_image).
    fn frame(&mut self) -> &mut [u8];

    /// The frame is complete: start the refresh and release the claim. Report
    /// the result later through [`Server::refresh_finished`].
    fn end_image(&mut self, refresh: RefreshMode) -> Result<(), Busy>;

    /// Release the claim without refreshing.
    fn abort_image(&mut self);
}

#[derive(Clone, Copy, Debug)]
struct ConfigUpload {
    total: usize,
    received: usize,
    frames: usize,
}

/// Protocol state for one connection. About 15 KB, most of it the inflater and
/// the config upload buffer, so the firmware should keep it off small stacks.
#[derive(Debug)]
pub struct Server {
    image: ImageReceiver,
    config_upload: Option<ConfigUpload>,
    config_in: [u8; MAX_CONFIG_SIZE],
    /// `Some` while a refresh is running: the etag to commit when it succeeds.
    refreshing: Option<Option<u32>>,
    displayed_etag: Option<u32>,
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    /// A server with no transfer in progress.
    pub fn new() -> Self {
        Self {
            image: ImageReceiver::new(),
            config_upload: None,
            config_in: [0; MAX_CONFIG_SIZE],
            refreshing: None,
            displayed_etag: None,
        }
    }

    /// The etag of the image on the panel, if the client that sent it gave one.
    pub fn displayed_etag(&self) -> Option<u32> {
        self.displayed_etag
    }

    /// Handle one request frame.
    pub fn handle<D: Device + ?Sized>(
        &mut self,
        frame: &[u8],
        device: &mut D,
        out: &mut dyn FnMut(&[u8]),
    ) {
        let request = match Request::parse(frame) {
            Ok(request) => request,
            Err(ParseError::NoOpcode) => return,
            Err(ParseError::UnknownOpcode(raw)) => {
                out(&[RESP_NACK, raw.to_be_bytes()[1]]);
                return;
            }
            Err(ParseError::Malformed(opcode)) => {
                if opcode == Opcode::DirectWriteStart {
                    // A START replaces the transfer in progress even when it
                    // is itself unusable.
                    self.abort_image(device);
                }
                out(Response::nack(opcode).as_bytes());
                return;
            }
        };
        let opcode = request.opcode();
        let reply = match request {
            Request::Reboot => {
                device.reboot();
                return;
            }
            Request::ConfigRead => {
                self.config_read(device, out);
                return;
            }
            Request::ConfigWrite(payload) => self.config_write(payload, device),
            Request::ConfigChunk(payload) => self.config_chunk(payload, device),
            Request::FirmwareVersion => {
                let version = device.firmware_version();
                let sha = &version.sha.as_bytes()[..version.sha.len().min(40)];
                // At most 40 bytes, so the length fits a byte.
                Response::ack(opcode)
                    .with(&[version.major, version.minor, sha.len() as u8])
                    .with(sha)
                    .with(&[version.patch])
            }
            Request::ReadMsd => Response::ack(opcode).with(&device.msd()),
            Request::ConfigClear => match device.clear_config() {
                Ok(()) => Response::ack(opcode),
                Err(StorageError) => Response::nack(opcode),
            },
            // Without a key there is no session to open.
            Request::Authenticate(_) => Response::ack(opcode).with(&[auth_status::NOT_CONFIG]),
            Request::PowerOff => Response::power_off_nack(PowerOffError::Unsupported),
            Request::DeepSleep { override_secs } => match device.deep_sleep(override_secs) {
                Ok(()) => return,
                Err(error) => Response::deep_sleep_nack(error),
            },
            Request::DirectWriteStart(start) => self.image_start(start, device),
            Request::DirectWriteData(chunk) => self.image_data(chunk, device),
            Request::DirectWriteEnd { refresh, new_etag } => {
                self.image_end(refresh, new_etag, device)
            }
            Request::PartialWriteStart(_) => {
                Response::nack(opcode).with(&[PartialWriteError::Unsupported as u8, 0])
            }
            Request::EnterDfu
            | Request::LedActivate(_)
            | Request::LedStop(_)
            | Request::Buzzer(_)
            | Request::PipeWriteStart(_)
            | Request::PipeWriteData(_)
            | Request::PipeWriteEnd(_)
            | Request::NfcEndpoint(_) => Response::nack(opcode),
        };
        out(reply.as_bytes());
    }

    /// Report the end of the refresh started by the last image. Emits 0x73 or
    /// 0x74 if that refresh still belongs to this connection.
    pub fn refresh_finished(&mut self, outcome: RefreshOutcome, out: &mut dyn FnMut(&[u8])) {
        let Some(etag) = self.refreshing.take() else {
            return;
        };
        let echo = match outcome {
            RefreshOutcome::Done => {
                self.displayed_etag = etag;
                RESP_REFRESH_SUCCESS
            }
            RefreshOutcome::TimedOut => {
                self.displayed_etag = None;
                RESP_REFRESH_TIMEOUT
            }
        };
        out(&[RESP_ACK, echo]);
    }

    /// The connection is gone: drop partial transfers, and stop reporting a
    /// running refresh to whoever connects next.
    pub fn disconnected<D: Device + ?Sized>(&mut self, device: &mut D) {
        self.abort_image(device);
        self.config_upload = None;
        self.refreshing = None;
    }

    fn config_read<D: Device + ?Sized>(&mut self, device: &mut D, out: &mut dyn FnMut(&[u8])) {
        let config = match device.read_config() {
            Ok(config) if config.len() <= MAX_CONFIG_SIZE => config,
            _ => {
                out(Response::nack(Opcode::ConfigRead).with(&[0, 0]).as_bytes());
                return;
            }
        };
        let len = config.len();
        let mut rest = config;
        let mut number: u16 = 0;
        loop {
            let mut frame = Response::ack(Opcode::ConfigRead).with(&number.to_le_bytes());
            if number == 0 {
                // At most MAX_CONFIG_SIZE, so it fits.
                frame = frame.with(&(len as u16).to_le_bytes());
            }
            let take = rest.len().min(frame.remaining());
            let (data, tail) = rest.split_at(take);
            out(frame.with(data).as_bytes());
            rest = tail;
            number += 1;
            if rest.is_empty() {
                return;
            }
        }
    }

    fn config_write<D: Device + ?Sized>(&mut self, payload: &[u8], device: &mut D) -> Response {
        let opcode = Opcode::ConfigWrite;
        self.config_upload = None;
        if payload.len() <= CONFIG_CHUNK_SIZE {
            return commit_config(opcode, payload, device);
        }
        let [low, high, ref first @ ..] = *payload else {
            return Response::nack(opcode);
        };
        let total = usize::from(u16::from_le_bytes([low, high]));
        let frames = total.div_ceil(CONFIG_CHUNK_SIZE);
        if payload.len() != CONFIG_CHUNK_SIZE_WITH_PREFIX
            || total <= CONFIG_CHUNK_SIZE
            || total > MAX_CONFIG_SIZE
            || frames > MAX_CONFIG_CHUNKS
        {
            return Response::nack(opcode);
        }
        self.config_in[..first.len()].copy_from_slice(first);
        self.config_upload = Some(ConfigUpload {
            total,
            received: first.len(),
            frames: 1,
        });
        Response::ack(opcode)
    }

    fn config_chunk<D: Device + ?Sized>(&mut self, payload: &[u8], device: &mut D) -> Response {
        let opcode = Opcode::ConfigChunk;
        let Some(mut upload) = self.config_upload.take() else {
            return Response::nack(opcode);
        };
        let end = upload.received + payload.len();
        if payload.is_empty()
            || payload.len() > CONFIG_CHUNK_SIZE
            || end > upload.total
            || upload.frames == MAX_CONFIG_CHUNKS
        {
            return Response::nack(opcode);
        }
        self.config_in[upload.received..end].copy_from_slice(payload);
        upload.received = end;
        upload.frames += 1;
        if end < upload.total {
            self.config_upload = Some(upload);
            return Response::ack(opcode);
        }
        commit_config(opcode, &self.config_in[..end], device)
    }

    fn image_start<D: Device + ?Sized>(
        &mut self,
        start: DirectWriteStart<'_>,
        device: &mut D,
    ) -> Response {
        let opcode = Opcode::DirectWriteStart;
        self.abort_image(device);
        if device.begin_image().is_err() {
            return Response::nack(opcode);
        }
        match self.image.start(start, device.frame()) {
            Ok(()) => Response::ack(opcode),
            Err(_) => {
                device.abort_image();
                Response::nack(opcode)
            }
        }
    }

    fn image_data<D: Device + ?Sized>(&mut self, chunk: &[u8], device: &mut D) -> Response {
        let opcode = Opcode::DirectWriteData;
        if !self.image.is_active() {
            return Response::nack(opcode);
        }
        match self.image.data(chunk, device.frame()) {
            Ok(()) => Response::ack(opcode),
            Err(_) => {
                device.abort_image();
                Response::nack(opcode)
            }
        }
    }

    fn image_end<D: Device + ?Sized>(
        &mut self,
        refresh: RefreshMode,
        new_etag: Option<u32>,
        device: &mut D,
    ) -> Response {
        let opcode = Opcode::DirectWriteEnd;
        if !self.image.is_active() {
            return Response::nack(opcode);
        }
        let frame_len = device.frame().len();
        if self.image.finish(frame_len).is_err() {
            device.abort_image();
            return Response::nack(opcode);
        }
        match device.end_image(refresh) {
            Ok(()) => {
                // A zero etag means "none", as upstream treats it.
                self.refreshing = Some(new_etag.filter(|&etag| etag != 0));
                Response::ack(opcode)
            }
            Err(Busy) => Response::nack(opcode),
        }
    }

    fn abort_image<D: Device + ?Sized>(&mut self, device: &mut D) {
        if self.image.is_active() {
            self.image.abort();
            device.abort_image();
        }
    }
}

fn commit_config<D: Device + ?Sized>(opcode: Opcode, blob: &[u8], device: &mut D) -> Response {
    match Blob::parse(blob).map(|blob| device.write_config(blob)) {
        Ok(Ok(())) => Response::ack(opcode),
        _ => Response::nack(opcode),
    }
}

// Status, echo, major, minor, hash length, a 40-byte hash and the patch byte.
const FIRMWARE_VERSION_FRAME: usize = 2 + 3 + 40 + 1;
const _: () = assert!(FIRMWARE_VERSION_FRAME <= MAX_RESPONSE_DATA_SIZE);
