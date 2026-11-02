//! Test support shared by the integration tests: a fake [`Device`] and helpers
//! that talk to a [`Server`] the way a client would.
#![allow(dead_code)]

use std::num::NonZeroU16;

use opendisplay_proto::config::Blob;
use opendisplay_proto::wire::DeepSleepError;
use opendisplay_proto::{Busy, Device, FirmwareVersion, RefreshMode, Server, StorageError};

pub const FRAME_LEN: usize = 192_000;

#[derive(Default)]
pub struct FakeDevice {
    pub config: Option<Vec<u8>>,
    pub written_config: Option<Vec<u8>>,
    pub frame: Vec<u8>,
    pub claimed: bool,
    pub busy: bool,
    pub sleep_answer: Option<DeepSleepError>,
    pub slept: Option<Option<u16>>,
    pub rebooted: bool,
    pub aborts: usize,
    pub refreshed: Option<RefreshMode>,
}

impl FakeDevice {
    pub fn new() -> Self {
        Self {
            frame: vec![0xEE; FRAME_LEN],
            ..Self::default()
        }
    }
}

impl Device for FakeDevice {
    fn firmware_version(&self) -> FirmwareVersion<'_> {
        FirmwareVersion {
            major: 1,
            minor: 2,
            patch: 3,
            sha: "abc1234",
        }
    }

    fn read_config(&mut self) -> Result<&[u8], StorageError> {
        self.config.as_deref().ok_or(StorageError)
    }

    fn write_config(&mut self, blob: Blob<'_>) -> Result<(), StorageError> {
        self.written_config = Some(blob.as_bytes().to_vec());
        Ok(())
    }

    fn clear_config(&mut self) -> Result<(), StorageError> {
        self.config = None;
        Ok(())
    }

    fn msd(&mut self) -> [u8; 16] {
        core::array::from_fn(|i| i as u8)
    }

    fn reboot(&mut self) {
        self.rebooted = true;
    }

    fn deep_sleep(&mut self, override_secs: Option<NonZeroU16>) -> Result<(), DeepSleepError> {
        match self.sleep_answer {
            Some(error) => Err(error),
            None => {
                self.slept = Some(override_secs.map(NonZeroU16::get));
                Ok(())
            }
        }
    }

    fn begin_image(&mut self) -> Result<(), Busy> {
        assert!(!self.claimed, "claimed twice");
        if self.busy {
            return Err(Busy);
        }
        self.claimed = true;
        Ok(())
    }

    fn frame(&mut self) -> &mut [u8] {
        assert!(self.claimed, "frame used without a claim");
        &mut self.frame
    }

    fn end_image(&mut self, refresh: RefreshMode) -> Result<(), Busy> {
        assert!(self.claimed);
        self.claimed = false;
        self.refreshed = Some(refresh);
        Ok(())
    }

    fn abort_image(&mut self) {
        assert!(self.claimed);
        self.claimed = false;
        self.aborts += 1;
    }
}

/// Send one request, return every frame the server emitted.
pub fn exchange(server: &mut Server, device: &mut FakeDevice, request: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    server.handle(request, device, &mut |frame| frames.push(frame.to_vec()));
    frames
}

pub fn request(opcode: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = opcode.to_be_bytes().to_vec();
    frame.extend_from_slice(payload);
    frame
}

/// The config the firmware reports.
pub fn board_config() -> Vec<u8> {
    let mut buf = vec![0u8; 4096];
    let len = opendisplay_proto::board::photopainter_config(&mut buf, None, 0).unwrap();
    buf.truncate(len);
    buf
}

/// Reassemble CONFIG_READ notifications as py-opendisplay's `interrogate` does.
pub fn reassemble_config(frames: &[Vec<u8>]) -> Vec<u8> {
    let (first, rest) = frames.split_first().unwrap();
    assert_eq!(&first[..2], &[0x00, 0x40]);
    let total = usize::from(u16::from_le_bytes([first[4], first[5]]));
    let mut data = first[6..].to_vec();
    for (number, frame) in rest.iter().enumerate() {
        assert_eq!(&frame[..2], &[0x00, 0x40]);
        assert_eq!(
            u16::from_le_bytes([frame[2], frame[3]]) as usize,
            number + 1
        );
        data.extend_from_slice(&frame[4..]);
    }
    assert_eq!(data.len(), total);
    data
}
