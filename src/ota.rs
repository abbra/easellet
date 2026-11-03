//! Firmware updates over the portal: writing the other app slot, and the
//! probation a new image serves until it proves it boots.
//!
//! `partitions.csv` gives the 16 MB flash two 3 MB application slots (`ota_0`,
//! `ota_1`) and an `otadata` record of which one boots; an upload is streamed
//! into the slot that is *not* running, so a failed transfer can never leave the
//! frame without firmware. Only when the whole image arrived and ESP-IDF's own
//! verifier accepted it does the boot slot change (`esp_ota_end` checks the
//! image header, the chip it was built for, and the checksum; a truncated or
//! foreign file fails there, not at boot).
//!
//! Rollback is the other half. `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y` makes
//! the bootloader treat a freshly booted update as *unverified*: the frame marks
//! it valid once it reaches its console and is operable ([`mark_running_valid`],
//! called from `main`), and a reset before that statement puts the previous image
//! back. That covers the failure an OTA most easily produces — an image that
//! does not start — while an image that starts but is broken in some later way
//! still needs a person to install another one.
//!
//! The flash writes happen on the portal's own thread, not on the main loop:
//! unlike the card and the panel there is nothing here with a single owner, the
//! body must be streamed rather than buffered whole, and the main loop has to
//! keep answering whether the update is going well. The parts worth testing on
//! the host — what an image looks like, how a refusal reads — live in
//! [`crate::firmware`].

use esp_idf_svc::ota::{EspOta, SlotState};
use esp_idf_svc::sys;

use crate::firmware::{self, Error};

/// How much is written to flash at a time. A multiple of the flash sector size
/// so no write crosses a sector it did not have to, and on the heap because the
/// portal's thread has 12 KB of stack.
const CHUNK: usize = 4 * 1024;

/// What the portal shows about the firmware that is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    /// The version the running image carries in its application description,
    /// which is the string a release list names; the crate's own number
    /// (`CARGO_PKG_VERSION`) only when the image carries no description.
    pub version: String,
    /// Slot the frame booted from: `ota_0` or `ota_1`.
    pub running: String,
    /// `valid`, `unverified`, `invalid`, `factory` or `unknown`.
    pub state: &'static str,
    /// The slot an upload would go to.
    pub other: String,
    /// What is in it now: the version of the image it holds, or nothing.
    pub other_version: Option<String>,
    /// Largest image the other slot accepts.
    pub max_bytes: usize,
}

fn state_name(state: SlotState) -> &'static str {
    match state {
        SlotState::Factory => "factory",
        SlotState::Valid => "valid",
        SlotState::Invalid => "invalid",
        SlotState::Unverified => "unverified",
        SlotState::Unknown => "unknown",
    }
}

/// Bytes one app slot accepts. Read from the partition table rather than
/// compiled in: the number belongs to `partitions.csv`, and a build flashed
/// with another table would otherwise refuse or overrun by a megabyte.
pub fn max_image_bytes() -> Result<usize, String> {
    // SAFETY: the call only reads the partition table and returns a pointer
    // into ESP-IDF's own static partition description, which lives as long as
    // the program does.
    let partition = unsafe { sys::esp_ota_get_next_update_partition(std::ptr::null()) };
    if partition.is_null() {
        return Err(
            "ERR firmware: no second app slot; flash partitions.csv (see README)".to_owned(),
        );
    }
    Ok(unsafe { (*partition).size as usize })
}

/// Which slot runs, what is in the other one, and how big it is.
pub fn info() -> Result<Info, String> {
    let ota = EspOta::new().map_err(|_| "ERR firmware: an update is in progress".to_owned())?;
    let running = ota
        .get_running_slot()
        .map_err(|error| format!("ERR firmware: {error}"))?;
    let update = ota
        .get_update_slot()
        .map_err(|error| format!("ERR firmware: {error}"))?;
    Ok(Info {
        version: crate::firmware::running_version(
            running.firmware.as_ref().map(|info| info.version.as_str()),
            env!("CARGO_PKG_VERSION"),
        ),
        running: running.label.to_string(),
        state: state_name(running.state),
        other: update.label.to_string(),
        other_version: update
            .firmware
            .as_ref()
            .map(|info| info.version.to_string()),
        max_bytes: max_image_bytes()?,
    })
}

/// [`info`] as the JSON document `GET /ota` returns. `json_escape` comes from the
/// portal because it owns every other document this frame serves.
pub fn info_json() -> Result<String, String> {
    let info = info()?;
    Ok(format!(
        "{{\"version\":\"{}\",\"running\":\"{}\",\"state\":\"{}\",\"other\":\"{}\",\"other_version\":{},\"max_bytes\":{}}}\n",
        crate::web::json_escape(&info.version),
        crate::web::json_escape(&info.running),
        crate::web::json_escape(info.state),
        crate::web::json_escape(&info.other),
        match &info.other_version {
            Some(version) => format!("\"{}\"", crate::web::json_escape(version)),
            None => "null".to_owned(),
        },
        info.max_bytes
    ))
}

/// Copy `length` bytes from `read` into the inactive slot and make it the boot
/// slot. The message is for the person who pressed the button; the frame still
/// has to be restarted, which the portal does once the answer is on the wire.
///
/// `read` fills the buffer it is given and returns how many bytes it wrote; a
/// return of zero means the body ended early and aborts the update.
pub fn install<F: FnMut(&mut [u8]) -> Result<usize, String> + ?Sized>(
    length: usize,
    read: &mut F,
) -> Result<String, (u16, &'static str, String)> {
    match write_inactive_slot(length, read) {
        Ok(message) => Ok(message),
        Err(error) => {
            let (status, reason) = error.status();
            Err((status, reason, error.message()))
        }
    }
}

fn write_inactive_slot<F: FnMut(&mut [u8]) -> Result<usize, String> + ?Sized>(
    length: usize,
    read: &mut F,
) -> Result<String, Error> {
    if length == 0 {
        return Err(Error::Empty);
    }
    let max = max_image_bytes().map_err(Error::Device)?;
    if length > max {
        return Err(Error::TooLarge(max));
    }
    let mut ota = EspOta::new().map_err(|_| Error::Busy)?;
    let slot = ota
        .get_update_slot()
        .map_err(|error| Error::Device(error.to_string()))?;
    let mut update = ota
        .initiate_update_with_known_size(length)
        .map_err(|error| Error::Device(format!("begin: {error}")))?;
    let mut buffer = vec![0_u8; CHUNK];
    let mut done = 0;
    while done < length {
        let wanted = (length - done).min(CHUNK);
        let count = read(&mut buffer[..wanted]).map_err(Error::Read)?;
        if count == 0 {
            // Dropping `update` runs `esp_ota_abort`, so the half-written slot
            // is marked aborted instead of looking like a usable image.
            return Err(Error::Short);
        }
        if count > wanted {
            return Err(Error::Read(format!("gave {count} bytes for {wanted}")));
        }
        if done == 0 && !firmware::looks_like_image(&buffer[..count]) {
            return Err(Error::NotAnImage);
        }
        update
            .write(&buffer[..count])
            .map_err(|error| Error::Device(format!("write at {done}: {error}")))?;
        done += count;
    }
    update
        .complete()
        .map_err(|error| Error::Device(format!("finish: {error}")))?;
    Ok(format!(
        "OK firmware {done} bytes written to {}; rebooting\n",
        slot.label
    ))
}

/// The updater the portal calls: `src/ota.rs` implements the interface `web.rs`
/// declares, so neither module has to know the other's hardware.
pub struct Updater;

impl crate::web::Firmware for Updater {
    fn info(&self) -> Result<String, String> {
        info_json()
    }

    fn install<'a>(
        &self,
        length: usize,
        read: &'a mut crate::web::BodyReader<'a>,
    ) -> crate::web::Refusal {
        install(length, read)
    }

    fn restart(&self) -> ! {
        esp_idf_svc::hal::reset::restart()
    }
}

/// Tell the bootloader this image is the one to keep. Called once the frame is
/// operable; until then a reset rolls back to the image that was running when
/// the update was installed.
pub fn mark_running_valid() -> Result<(), String> {
    let mut ota = EspOta::new().map_err(|_| "ERR firmware: an update is in progress".to_owned())?;
    ota.mark_running_slot_valid()
        .map_err(|error| format!("ERR firmware: mark valid: {error}"))
}
