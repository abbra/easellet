//! Read-only FAT filesystem bring-up on the board's 4-bit SDMMC card slot.
//!
//! The card is mounted at `/sdcard` without ever formatting it. The IDF VFS
//! layer is registered explicitly because the safe `esp-idf-svc` FAT wrapper
//! exposes only the raw FATFS handle, not the POSIX mount used by `std::fs`.

use std::{
    ffi::{CStr, CString},
    fs,
    path::Path,
    ptr,
};

use esp_idf_svc::hal::{
    gpio::{InputPin, OutputPin},
    sd::{
        SdCardConfiguration, SdCardDriver,
        mmc::{SdMmc, SdMmcHostDriver, config::Configuration as HostConfig},
    },
};
use esp_idf_svc::sys;

const MOUNT_PATH: &CStr = c"/sdcard";
const DRIVE: &CStr = c"0:";
const DRIVE_NUMBER: u8 = 0;
const MAX_FILES: usize = 5;
/// Console listings are printed over USB at ~57 KiB/s, so a directory dump is
/// deliberately short; `sd ls` is for looking, not for inventory.
const MAX_ENTRIES: usize = 32;
/// How many pictures one directory can hold. This is a memory ceiling rather
/// than a policy: the frame keeps the whole list in RAM and serves it to the
/// browser as JSON, so 512 names with paths is roughly 30 KB, and a card with
/// more than that is an archive rather than a slideshow.
pub const MAX_IMAGES: usize = 512;

pub struct Storage {
    card: SdCardDriver<SdMmcHostDriver<'static>>,
}

impl Storage {
    /// Probe the card and mount its first FAT partition read-only.
    ///
    /// `format_if_mount_failed` is deliberately absent: an unreadable card is
    /// reported, never rewritten.
    #[allow(clippy::too_many_arguments)]
    pub fn mount(
        slot: impl SdMmc + 'static,
        cmd: impl OutputPin + 'static,
        clk: impl OutputPin + 'static,
        d0: impl InputPin + OutputPin + 'static,
        d1: impl InputPin + OutputPin + 'static,
        d2: impl InputPin + OutputPin + 'static,
        d3: impl InputPin + OutputPin + 'static,
    ) -> Result<Self, String> {
        // Vendor SDMMC pins for the V2 board: CLK 39, CMD 41, D0..D3 40,1,2,38.
        let host = SdMmcHostDriver::new_4bits(
            slot,
            cmd,
            clk,
            d0,
            d1,
            d2,
            d3,
            None::<esp_idf_svc::hal::gpio::AnyIOPin>,
            None::<esp_idf_svc::hal::gpio::AnyIOPin>,
            &HostConfig::new(),
        )
        .map_err(|error| format!("ERR sd host init: {error}\r\n"))?;

        let mut card_config = SdCardConfiguration::new();
        card_config.command_timeout_ms = 1_000;
        let card = SdCardDriver::new_mmc(host, &card_config)
            .map_err(|error| format!("ERR sd card absent or unreadable: {error}\r\n"))?;

        let mut fatfs = ptr::null_mut();
        // SAFETY: both strings are static and NUL terminated; `fatfs` receives
        // the VFS-owned FATFS instance on success.
        let registered = unsafe {
            sys::esp_vfs_fat_register(MOUNT_PATH.as_ptr(), DRIVE.as_ptr(), MAX_FILES, &mut fatfs)
        };
        if registered != sys::ESP_OK || fatfs.is_null() {
            return Err(format!("ERR sd VFS register: {registered}\r\n"));
        }

        // Keep the card alive for as long as FATFS can call into the disk I/O
        // hooks; the hooks store this pointer.
        let card_ptr = card.card() as *const _ as *mut _;
        // SAFETY: the card outlives the registration and owns the sdmmc_card_t.
        unsafe {
            sys::ff_diskio_register_sdmmc(DRIVE_NUMBER, card_ptr);
        }

        let mounted = unsafe { sys::f_mount(fatfs, DRIVE.as_ptr(), 1) };
        if mounted != sys::FRESULT_FR_OK {
            cleanup();
            return Err(format!("ERR sd FAT mount: f_mount={mounted}\r\n"));
        }

        Ok(Self { card })
    }

    /// One-line status plus a bounded, read-only root listing.
    pub fn report(&self) -> Result<String, String> {
        let card = self.card.card();
        let sectors = u64::try_from(card.csd.capacity).unwrap_or(0);
        let sector_size = u64::try_from(card.csd.sector_size).unwrap_or(0);
        let capacity = sectors.saturating_mul(sector_size);

        let mut total = 0_u64;
        let mut free = 0_u64;
        // SAFETY: MOUNT_PATH is a valid NUL-terminated path; the outputs are
        // plain u64 values owned by this function.
        let info = unsafe { sys::esp_vfs_fat_info(MOUNT_PATH.as_ptr(), &mut total, &mut free) };

        let entries = list_directory(Path::new("/sdcard"), MAX_ENTRIES)?;
        let mut output = format!(
            "OK sd mounted={} capacity={}MiB real_freq_khz={}",
            MOUNT_PATH.to_string_lossy(),
            capacity / (1024 * 1024),
            card.real_freq_khz
        );
        if info == sys::ESP_OK {
            output.push_str(&format!(
                " total={}MiB free={}MiB",
                total / (1024 * 1024),
                free / (1024 * 1024)
            ));
        }
        output.push_str(&format!(" entries={}\r\n", entries.len()));
        for entry in entries {
            output.push_str(&entry);
            output.push_str("\r\n");
        }
        Ok(output)
    }

    /// The image files in one directory, as paths relative to the card root.
    ///
    /// Both formats the panel can decode are collected - a `.bmp` and the raw
    /// `.pf6` the host converter writes - because a slideshow is about the
    /// picture, not about which of the two loaders happens to read it. Names are
    /// sorted so the order on the panel is the order in the directory listing
    /// rather than whatever order FAT hands back.
    pub fn images(&self, relative: Option<&str>) -> Result<Vec<String>, String> {
        Ok(self
            .pictures(relative)?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }

    /// [`Self::images`] with each file's size, read in one pass over the
    /// directory.
    ///
    /// This goes to FATFS directly because its directory records already carry
    /// the size and the attributes. Through `std::fs`, every `metadata()` is a
    /// path lookup that FATFS answers by scanning the directory from the top
    /// again, so a listing cost grew with the square of the number of pictures.
    ///
    /// The [`MAX_IMAGES`] names kept are the alphabetically first ones, whatever
    /// order the card stores them in.
    pub fn pictures(&self, relative: Option<&str>) -> Result<Vec<(String, u64)>, String> {
        let relative = relative.unwrap_or("");
        // Same rules as every other card path: relative, no `..`.
        if !relative.is_empty() {
            crate::image::resolve(relative)?;
        }
        let path = CString::new(format!("0:/{relative}"))
            .map_err(|_| "ERR sd path contains a NUL byte\r\n".to_owned())?;
        let mut directory = sys::FF_DIR::default();
        // SAFETY: `directory` is a valid, writable FF_DIR and `path` is a
        // NUL-terminated string that outlives the call.
        let opened = unsafe { sys::f_opendir(&mut directory, path.as_ptr()) };
        if opened != sys::FRESULT_FR_OK {
            return Err(format!(
                "ERR sd list /sdcard/{relative}: f_opendir={opened}\r\n"
            ));
        }
        let mut names: Vec<(String, u64)> = Vec::new();
        let outcome = loop {
            let mut entry = sys::FILINFO::default();
            // SAFETY: `directory` was opened above and is closed below.
            let read = unsafe { sys::f_readdir(&mut directory, &mut entry) };
            if read != sys::FRESULT_FR_OK {
                break Err(format!("ERR sd entry: f_readdir={read}\r\n"));
            }
            if entry.fname[0] == 0 {
                break Ok(());
            }
            if u32::from(entry.fattrib) & sys::AM_DIR != 0 {
                continue;
            }
            // SAFETY: FATFS NUL-terminates `fname` within its 13 bytes.
            let name = unsafe { CStr::from_ptr(entry.fname.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            // Case-insensitive: the vendor's own files are `01_DAYU.BMP` while the
            // converter writes `.pf6` in lower case.
            let lower = name.to_ascii_lowercase();
            if !(lower.ends_with(".bmp") || lower.ends_with(".pf6")) {
                continue;
            }
            let name = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            crate::gallery::keep_first(&mut names, (name, u64::from(entry.fsize)), MAX_IMAGES);
        };
        // SAFETY: `directory` is open; closing it is required either way.
        unsafe { sys::f_closedir(&mut directory) };
        outcome.map(|()| names)
    }

    /// Bounded, read-only listing of one directory below `/sdcard`.
    pub fn list(&self, relative: Option<&str>) -> Result<String, String> {
        let Some(relative) = relative else {
            return self.report();
        };
        let path = crate::image::resolve(relative)?;
        let entries = list_directory(&path, MAX_ENTRIES)?;
        let mut output = format!(
            "OK sd list={} entries={}\r\n",
            path.display(),
            entries.len()
        );
        for entry in entries {
            output.push_str(&entry);
            output.push_str("\r\n");
        }
        Ok(output)
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        // SAFETY: these calls reverse the registration performed in `mount`; the
        // card field is still alive until after this method returns.
        unsafe {
            sys::f_mount(ptr::null_mut(), DRIVE.as_ptr(), 0);
            sys::esp_vfs_fat_unregister_path(MOUNT_PATH.as_ptr());
            sys::ff_diskio_register(DRIVE_NUMBER, ptr::null());
        }
    }
}

fn cleanup() {
    // SAFETY: reverses a partial mount. The card is still owned by the caller.
    unsafe {
        sys::f_mount(ptr::null_mut(), DRIVE.as_ptr(), 0);
        sys::esp_vfs_fat_unregister_path(MOUNT_PATH.as_ptr());
        sys::ff_diskio_register(DRIVE_NUMBER, ptr::null());
    }
}

fn list_directory(path: &Path, limit: usize) -> Result<Vec<String>, String> {
    let directory = fs::read_dir(path)
        .map_err(|error| format!("ERR sd list {}: {error}\r\n", path.display()))?;
    let mut entries = Vec::new();
    for entry in directory.take(limit) {
        let entry = entry.map_err(|error| format!("ERR sd entry: {error}\r\n"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        match entry.metadata() {
            Ok(metadata) if metadata.is_dir() => entries.push(format!("dir {name}")),
            Ok(metadata) => entries.push(format!("file {name} {}", metadata.len())),
            Err(_) => entries.push(format!("? {name}")),
        }
    }
    // Unstable on purpose: equal strings are indistinguishable, and the stable sort
    // keeps a 4 KB scratch buffer on the stack (the 8 KB main task runs this).
    entries.sort_unstable();
    Ok(entries)
}
