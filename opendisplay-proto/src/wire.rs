//! Wire constants of `opendisplay_protocol.h`, spec 2.2.
//!
//! `tests/header_sync.rs` checks every value here against the vendored header.

/// The spec revision these constants follow. Never sent on the wire.
pub const PROTOCOL_VERSION: &str = "2.2";

/// A command opcode: two bytes, big-endian, at the start of every request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum Opcode {
    /// Reset the device. No response.
    Reboot = 0x000F,
    /// Read the config blob as chunked notifications.
    ConfigRead = 0x0040,
    /// Write a config blob, whole or as the first of several chunks.
    ConfigWrite = 0x0041,
    /// Continue a chunked config write.
    ConfigChunk = 0x0042,
    /// Report firmware version and build hash.
    FirmwareVersion = 0x0043,
    /// Report the 16-byte manufacturer-specific advertising record.
    ReadMsd = 0x0044,
    /// Erase the stored config.
    ConfigClear = 0x0045,
    /// Two-step session authentication.
    Authenticate = 0x0050,
    /// Jump to a Nordic or Silabs bootloader.
    EnterDfu = 0x0051,
    /// Cut power through a D flip-flop latch.
    PowerOff = 0x0052,
    /// Timer deep sleep, optionally with a one-shot duration.
    DeepSleep = 0x0053,
    /// Begin a full-frame image, raw or zlib.
    DirectWriteStart = 0x0070,
    /// One chunk of image (or zlib) bytes.
    DirectWriteData = 0x0071,
    /// Finish the image and refresh the panel.
    DirectWriteEnd = 0x0072,
    /// Run an LED flash pattern.
    LedActivate = 0x0073,
    /// Stop an LED pattern.
    LedStop = 0x0075,
    /// Begin a partial-region write (1 bpp panels only).
    PartialWriteStart = 0x0076,
    /// Play a buzzer pattern.
    Buzzer = 0x0077,
    /// Negotiate a sliding-window image transfer.
    PipeWriteStart = 0x0080,
    /// One sequenced PIPE frame.
    PipeWriteData = 0x0081,
    /// Finish a PIPE transfer.
    PipeWriteEnd = 0x0082,
    /// NFC tag read and write.
    NfcEndpoint = 0x0083,
}

impl Opcode {
    /// Every opcode the spec defines, in numeric order.
    pub const ALL: [Self; 22] = [
        Self::Reboot,
        Self::ConfigRead,
        Self::ConfigWrite,
        Self::ConfigChunk,
        Self::FirmwareVersion,
        Self::ReadMsd,
        Self::ConfigClear,
        Self::Authenticate,
        Self::EnterDfu,
        Self::PowerOff,
        Self::DeepSleep,
        Self::DirectWriteStart,
        Self::DirectWriteData,
        Self::DirectWriteEnd,
        Self::LedActivate,
        Self::LedStop,
        Self::PartialWriteStart,
        Self::Buzzer,
        Self::PipeWriteStart,
        Self::PipeWriteData,
        Self::PipeWriteEnd,
        Self::NfcEndpoint,
    ];

    /// The opcode for a raw value, if the spec defines one.
    pub fn from_u16(value: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|opcode| *opcode as u16 == value)
    }

    /// The low byte, which every response echoes in its second byte.
    pub const fn echo(self) -> u8 {
        (self as u16 & 0xFF) as u8
    }
}

/// First byte of a response: success.
pub const RESP_ACK: u8 = 0x00;
/// First byte of a response: failure. A following error byte is scoped to the
/// echoed opcode.
pub const RESP_NACK: u8 = 0xFF;
/// First byte of a response: the command needs an authenticated session.
pub const RESP_AUTH_REQUIRED: u8 = 0xFE;

/// Unsolicited echo byte after a DIRECT_WRITE or PIPE END: the panel refreshed.
pub const RESP_REFRESH_SUCCESS: u8 = 0x73;
/// Unsolicited echo byte after an END: the refresh timed out.
pub const RESP_REFRESH_TIMEOUT: u8 = 0x74;

/// Status byte of a CMD_AUTHENTICATE response.
pub mod auth_status {
    /// Step 1: challenge follows. Step 2: session open.
    pub const CHALLENGE: u8 = 0x00;
    /// Step 2 success (same value as [`CHALLENGE`]).
    pub const SUCCESS: u8 = CHALLENGE;
    /// Wrong key.
    pub const FAILED: u8 = 0x01;
    /// A session is already open.
    pub const ALREADY: u8 = 0x02;
    /// No key is configured, so there is nothing to authenticate against.
    pub const NOT_CONFIG: u8 = 0x03;
    /// Ten or more attempts within 60 seconds.
    pub const RATE_LIMIT: u8 = 0x04;
    /// Anything else.
    pub const ERROR: u8 = 0xFF;
}

/// NACK codes for CMD_DEEP_SLEEP. Only `Unsupported` means the device cannot
/// sleep; the other two mean "refused, still awake".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DeepSleepError {
    /// The target has no deep sleep.
    Unsupported = 0x00,
    /// Deep sleep is disabled in the config.
    Disabled = 0x01,
    /// The device is not battery powered.
    NotBattery = 0x02,
}

/// NACK codes for CMD_POWER_OFF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PowerOffError {
    /// No D flip-flop power latch on this board.
    Unsupported = 0x00,
}

/// NACK codes for CMD_PIPE_WRITE_START. A different namespace from
/// [`PartialWriteError`] even where the byte values coincide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PipeStartError {
    /// Bad length or version.
    BadHeader = 0x01,
    /// An unknown flag bit is set.
    UnknownFlag = 0x02,
    /// `total_size` does not match the panel.
    SizeMismatch = 0x03,
    /// Partial: `old_etag` does not match.
    EtagMismatch = 0x05,
    /// Partial mode is not supported.
    PartialUnsupported = 0x06,
    /// Partial: the rectangle is invalid.
    RectInvalid = 0x07,
}

/// NACK codes for CMD_PARTIAL_WRITE_START and its DATA and END frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PartialWriteError {
    /// `old_etag` does not match the displayed etag.
    EtagMismatch = 0x01,
    /// The rectangle is outside the panel.
    RectOutOfBounds = 0x03,
    /// `x` or `width` is not a multiple of 8.
    RectAlign = 0x04,
    /// Bad or unsupported flags.
    Flags = 0x05,
    /// Stream or length error.
    Stream = 0x06,
    /// Partial write is not supported (for example, not a 1 bpp panel).
    Unsupported = 0x07,
}

/// Fatal NACK codes for CMD_PIPE_WRITE_DATA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PipeDataError {
    /// The zlib stream could not be decoded.
    Decompress = 0x02,
    /// Write or size error, or a reorder-queue overflow.
    Write = 0x03,
    /// Out of window on both sides.
    Protocol = 0x04,
}

/// Largest config payload in one CONFIG_WRITE or CONFIG_CHUNK frame.
pub const CONFIG_CHUNK_SIZE: usize = 200;
/// Payload of the first frame of a chunked CONFIG_WRITE: total (2) + 200.
pub const CONFIG_CHUNK_SIZE_WITH_PREFIX: usize = 202;
/// Most frames one chunked config write may use, the first included.
pub const MAX_CONFIG_CHUNKS: usize = 20;
/// Largest notification frame, status and echo included.
pub const MAX_RESPONSE_DATA_SIZE: usize = 100;
/// Largest BLE ATT frame: opcode (1) + handle (2) + value.
pub const BLE_MAX_FRAME: usize = 256;

/// Image bytes per DIRECT_WRITE_DATA frame without encryption (BLE).
pub const CHUNK_SIZE: usize = 230;
/// Image bytes per DIRECT_WRITE_DATA frame inside an encrypted session.
pub const ENCRYPTED_CHUNK_SIZE: usize = 154;

/// PIPE sub-protocol version.
pub const PIPE_VERSION: u8 = 0x01;
/// PIPE START flag: the stream is zlib.
pub const PIPE_FLAG_COMPRESSED: u8 = 0x01;
/// PIPE START flag: partial-region refresh.
pub const PIPE_FLAG_PARTIAL: u8 = 0x02;
/// Largest PIPE frame on the wire.
pub const PIPE_MAX_FRAME: usize = 244;
/// Opcode (2) and sequence (1) bytes in front of PIPE data.
pub const PIPE_FRAME_OVERHEAD: usize = 3;
/// Width of the selective-ACK mask.
pub const PIPE_ACK_MASK_BITS: usize = 32;

/// Opcode bytes, which are also the AAD of the encrypted envelope.
pub const BLE_CMD_HEADER_SIZE: usize = 2;
/// Envelope nonce; the CCM nonce is bytes 3..16 of it.
pub const ENCRYPTION_NONCE_SIZE: usize = 16;
/// CCM tag length.
pub const ENCRYPTION_TAG_SIZE: usize = 12;

/// Default plaintext LAN port.
pub const LAN_TCP_PORT: u16 = 2446;
/// Default TLS-PSK LAN port.
pub const LAN_TLS_PORT: u16 = 2447;
/// Largest LAN wire frame: length prefix (2) + payload.
pub const LAN_MAX_FRAME: usize = 4096;
/// Largest LAN payload after the length prefix.
pub const LAN_MAX_PAYLOAD: usize = LAN_MAX_FRAME - 2;
/// DNS-SD service type.
pub const LAN_MDNS_SERVICE: &str = "_opendisplay._tcp";
/// A LAN client idle this long is dropped.
pub const LAN_READ_TIMEOUT_S: u32 = 30;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcodes_round_trip_and_echo_the_low_byte() {
        for opcode in Opcode::ALL {
            assert_eq!(Opcode::from_u16(opcode as u16), Some(opcode));
        }
        assert_eq!(Opcode::from_u16(0x0074), None);
        assert_eq!(Opcode::ConfigWrite.echo(), 0x41);
        assert_eq!(Opcode::Reboot.echo(), 0x0F);
    }
}
