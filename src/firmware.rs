//! What a firmware file is, and how a refusal to install one reads.
//!
//! The decisions in here are the ones worth testing on the host, so they live
//! apart from `ota` (which talks to ESP-IDF): the magic byte that tells a `.bin`
//! from the HTML page a wrong address returns, and the sentence and HTTP status
//! each refusal carries. Everything else about an update — the slot, the flash
//! write, the rollback — needs the real chip.

use std::fmt;

/// First byte of an ESP application image (`esp_image_header_t.magic`).
///
/// The full check happens in `esp_ota_end`; this is only the early "did the
/// browser send the error page instead of the file" test, and it has to happen
/// before the first chunk is written to flash.
pub const IMAGE_MAGIC: u8 = 0xE9;

/// Why an upload was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// `Content-Length: 0`: an empty body is a mistake, not an update.
    Empty,
    /// Larger than the destination slot.
    TooLarge(usize),
    /// Not an ESP application image at all.
    NotAnImage,
    /// The client stopped sending before `Content-Length` bytes arrived.
    Short,
    /// Another update already holds the OTA state.
    Busy,
    /// The frame could not read the request body.
    Read(String),
    /// Flash or ESP-IDF refused the write.
    Device(String),
}

impl Error {
    /// The one-line answer the portal shows. Everything starts with `ERR`, as
    /// every other refusal this firmware sends does.
    pub fn message(&self) -> String {
        match self {
            Self::Empty => {
                "ERR firmware: empty body; send the .bin the build produced\n".to_owned()
            }
            Self::TooLarge(max) => format!("ERR firmware: the app slot holds {max} bytes\n"),
            Self::NotAnImage => {
                "ERR firmware: that is not an app image (it does not start with 0xE9)\n".to_owned()
            }
            Self::Short => "ERR firmware: the connection ended before the image did\n".to_owned(),
            Self::Busy => "ERR firmware: another update is in progress\n".to_owned(),
            Self::Read(error) => format!("ERR firmware read: {error}\n"),
            Self::Device(error) => format!("ERR firmware: {error}\n"),
        }
    }

    /// HTTP status and reason phrase. A request the frame could not parse is the
    /// client's fault; a flash that refused the write is not.
    pub fn status(&self) -> (u16, &'static str) {
        match self {
            Self::TooLarge(_) => (413, "Payload Too Large"),
            Self::Busy => (503, "Service Unavailable"),
            Self::Device(_) => (500, "Internal Server Error"),
            Self::Empty | Self::NotAnImage | Self::Short | Self::Read(_) => (400, "Bad Request"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message().trim_end())
    }
}

impl std::error::Error for Error {}

/// Whether the first bytes look like an ESP application image. One byte is not
/// a checksum and does not try to be one; it tells a `.bin` from a page of HTML,
/// which is the mistake people actually make.
pub fn looks_like_image(head: &[u8]) -> bool {
    head.first() == Some(&IMAGE_MAGIC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_esp_image_is_recognised_by_its_magic() {
        assert!(looks_like_image(&[IMAGE_MAGIC, 0x05, 0x02]));
        // What a wrong URL or a mistyped file name brings back instead.
        assert!(!looks_like_image(b"<!doctype html><title>404</title>"));
        assert!(!looks_like_image(b""));
    }

    #[test]
    fn a_refusal_says_what_to_do_in_one_line() {
        for error in [
            Error::Empty,
            Error::TooLarge(3 * 1024 * 1024),
            Error::NotAnImage,
            Error::Short,
            Error::Busy,
            Error::Read("timeout".to_owned()),
            Error::Device("ESP_ERR_FLASH_OP_FAIL".to_owned()),
        ] {
            let message = error.message();
            assert!(message.starts_with("ERR firmware"), "{message}");
            assert!(message.ends_with('\n'), "{message}");
            assert_eq!(message.matches('\n').count(), 1, "{message}");
            let (status, reason) = error.status();
            assert!((400..600).contains(&status), "{status}");
            assert!(!reason.is_empty());
        }
        assert!(Error::TooLarge(0x300000).message().contains("3145728"));
        // A body the frame could not read and a flash that failed are different
        // mistakes, and a page that retries the first should not retry the second.
        assert_eq!(Error::Short.status(), (400, "Bad Request"));
        assert_eq!(
            Error::Device("x".into()).status(),
            (500, "Internal Server Error")
        );
    }
}
