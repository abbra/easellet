//! Strict image loaders for the panel framebuffer.
//!
//! Two formats are accepted from `/sdcard`: an uncompressed 24-bit 800x480
//! bottom-up or top-down BMP, and the host converter's exact 192000-byte raw
//! 4bpp image. Paths are always resolved beneath the read-only SD mount.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

#[cfg(not(test))]
use crate::panel;
// The native harness builds this file as the crate root, so the modules `panel`
// reaches for have to be declared here too, under the names it expects.
#[cfg(test)]
#[path = "font.rs"]
mod font;
#[cfg(test)]
#[path = "panel.rs"]
mod panel;

const BMP_HEADER_LEN: usize = 54;
const BMP_ROW_BYTES: usize = panel::WIDTH * 3;

/// The one place on the card the firmware writes to, and therefore the only
/// place a host may ask it to delete from. Everything else is the vendor's own
/// data and is read-only from the portal.
pub const WRITABLE_DIR: &str = "PFTEST";

/// The file a delete request names, if the request may act on it.
///
/// Two rules, both enforced here rather than in the caller because this module
/// already owns the path policy: the name must resolve inside the card, and it
/// must be a picture inside [`WRITABLE_DIR`]. A `.bmp` the vendor shipped is
/// readable and not removable, which is the whole point of the distinction.
pub fn removable(relative: &str) -> Result<PathBuf, String> {
    let lower = relative.to_ascii_lowercase();
    if !(lower.ends_with(".pf6") || lower.ends_with(".bmp")) {
        return Err(format!(
            "ERR {relative}: only .pf6 and .bmp pictures can be removed\r\n"
        ));
    }
    let path = resolve(relative)?;
    let writable = format!("{}/", WRITABLE_DIR.to_ascii_lowercase());
    if !lower.starts_with(&writable) {
        return Err(format!(
            "ERR {relative}: only files inside {WRITABLE_DIR} can be removed; the rest of the card is the vendor's\r\n"
        ));
    }
    Ok(path)
}

/// Whether `relative` names [`WRITABLE_DIR`] or a directory inside it, the only
/// part of the card the frame writes pictures to or removes them from.
pub fn is_writable_dir(relative: &str) -> bool {
    let lower = relative.to_ascii_lowercase();
    let root = WRITABLE_DIR.to_ascii_lowercase();
    resolve(relative).is_ok() && (lower == root || lower.starts_with(&format!("{root}/")))
}

pub fn resolve(relative: &str) -> Result<PathBuf, String> {
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative.contains('\0')
    {
        return Err("ERR image path must be relative and must not contain backslashes\r\n".into());
    }
    for component in relative.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err("ERR image path contains an empty, '.' or '..' component\r\n".into());
        }
    }
    Ok(Path::new("/sdcard").join(relative))
}

pub fn load_bmp(path: &Path, buffer: &mut [u8]) -> Result<(), String> {
    let mut file = File::open(path)
        .map_err(|error| format!("ERR image open {}: {error}\r\n", path.display()))?;
    decode_bmp(&mut file, buffer)
}

pub fn load_raw(path: &Path, buffer: &mut [u8]) -> Result<(), String> {
    let mut file = File::open(path)
        .map_err(|error| format!("ERR image open {}: {error}\r\n", path.display()))?;
    decode_raw(&mut file, buffer)
}

/// Load either format, choosing the loader by extension.
///
/// The slideshow lists both kinds from one directory, so it cannot ask the user
/// which loader to use; `.pf6` is the converter's exact 4bpp output and anything
/// else is treated as a BMP, which is what `display bmp` already does.
pub fn load_any(relative: &str, buffer: &mut [u8]) -> Result<(), String> {
    let path = resolve(relative)?;
    let raw = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pf6"));
    if raw {
        load_raw(&path, buffer)
    } else {
        load_bmp(&path, buffer)
    }
}

fn decode_bmp<R: Read + Seek>(reader: &mut R, buffer: &mut [u8]) -> Result<(), String> {
    if buffer.len() != panel::buffer_len() {
        return Err("ERR image framebuffer size mismatch\r\n".into());
    }
    let mut header = [0_u8; BMP_HEADER_LEN];
    reader
        .read_exact(&mut header)
        .map_err(|error| format!("ERR BMP header: {error}\r\n"))?;
    if &header[0..2] != b"BM" {
        return Err("ERR BMP signature is not BM\r\n".into());
    }
    let file_size = u32::from_le_bytes(header[2..6].try_into().expect("4 bytes")) as u64;
    let pixel_offset = u32::from_le_bytes(header[10..14].try_into().expect("4 bytes")) as u64;
    let dib_size = u32::from_le_bytes(header[14..18].try_into().expect("4 bytes"));
    let width = i32::from_le_bytes(header[18..22].try_into().expect("4 bytes"));
    let signed_height = i32::from_le_bytes(header[22..26].try_into().expect("4 bytes"));
    let planes = u16::from_le_bytes(header[26..28].try_into().expect("2 bytes"));
    let bits = u16::from_le_bytes(header[28..30].try_into().expect("2 bytes"));
    let compression = u32::from_le_bytes(header[30..34].try_into().expect("4 bytes"));

    if dib_size < 40 {
        return Err(format!("ERR BMP DIB header too short: {dib_size}\r\n"));
    }
    if width != panel::WIDTH as i32 || signed_height.unsigned_abs() as usize != panel::HEIGHT {
        return Err(format!(
            "ERR BMP dimensions must be {}x{}, got {width}x{}\r\n",
            panel::WIDTH,
            panel::HEIGHT,
            signed_height.unsigned_abs()
        ));
    }
    if planes != 1 || bits != 24 || compression != 0 {
        return Err(format!(
            "ERR BMP must be uncompressed 24-bit RGB, got planes={planes} bits={bits} compression={compression}\r\n"
        ));
    }
    let file_len = reader
        .seek(SeekFrom::End(0))
        .map_err(|error| format!("ERR BMP length: {error}\r\n"))?;
    if file_size != 0 && file_size > file_len {
        return Err(format!(
            "ERR BMP header size {file_size} exceeds file length {file_len}\r\n"
        ));
    }
    let top_down = signed_height < 0;
    let row_stride = (BMP_ROW_BYTES + 3) & !3;
    let needed = pixel_offset
        .checked_add(row_stride as u64 * panel::HEIGHT as u64)
        .ok_or_else(|| "ERR BMP size overflow\r\n".to_owned())?;
    if needed > file_len {
        return Err(format!(
            "ERR BMP truncated: need {needed} bytes, file has {file_len}\r\n"
        ));
    }
    reader
        .seek(SeekFrom::Start(pixel_offset))
        .map_err(|error| format!("ERR BMP seek: {error}\r\n"))?;

    let mut row = vec![0_u8; row_stride];
    for file_row in 0..panel::HEIGHT {
        reader
            .read_exact(&mut row)
            .map_err(|error| format!("ERR BMP row {file_row}: {error}\r\n"))?;
        let y = if top_down {
            file_row
        } else {
            panel::HEIGHT - 1 - file_row
        };
        for x in 0..panel::WIDTH {
            let offset = x * 3;
            let color = panel::nearest(row[offset + 2], row[offset + 1], row[offset]);
            panel::set_pixel(buffer, x, y, color);
        }
    }
    Ok(())
}

fn decode_raw<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<(), String> {
    if buffer.len() != panel::buffer_len() {
        return Err("ERR image framebuffer size mismatch\r\n".into());
    }
    reader.read_exact(buffer).map_err(|error| {
        format!(
            "ERR raw image must be exactly {} bytes: {error}\r\n",
            buffer.len()
        )
    })?;
    let mut extra = [0_u8; 1];
    if reader
        .read(&mut extra)
        .map_err(|error| format!("ERR raw image trailing data: {error}\r\n"))?
        != 0
    {
        return Err(format!(
            "ERR raw image must be exactly {} bytes; trailing data found\r\n",
            buffer.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn only_pftest_and_below_is_writable() {
        assert!(is_writable_dir("PFTEST"));
        assert!(is_writable_dir("pftest/UPLOADS"));
        assert!(!is_writable_dir("PFTESTX"));
        assert!(!is_writable_dir("PICTURES"));
        assert!(!is_writable_dir("PFTEST/../PICTURES"));
        assert!(!is_writable_dir("/PFTEST"));
        assert!(!is_writable_dir(""));
    }

    fn bmp(top_down: bool, set: impl Fn(&mut Vec<u8>)) -> Vec<u8> {
        let stride = BMP_ROW_BYTES;
        let len = BMP_HEADER_LEN + stride * panel::HEIGHT;
        let mut bytes = vec![0_u8; len];
        bytes[0..2].copy_from_slice(b"BM");
        bytes[2..6].copy_from_slice(&(len as u32).to_le_bytes());
        bytes[10..14].copy_from_slice(&54_u32.to_le_bytes());
        bytes[14..18].copy_from_slice(&40_u32.to_le_bytes());
        bytes[18..22].copy_from_slice(&(panel::WIDTH as i32).to_le_bytes());
        let height = if top_down {
            -(panel::HEIGHT as i32)
        } else {
            panel::HEIGHT as i32
        };
        bytes[22..26].copy_from_slice(&height.to_le_bytes());
        bytes[26..28].copy_from_slice(&1_u16.to_le_bytes());
        bytes[28..30].copy_from_slice(&24_u16.to_le_bytes());
        set(&mut bytes);
        bytes
    }

    fn put_bgr(bytes: &mut [u8], x: usize, file_y: usize, rgb: [u8; 3]) {
        let offset = 54 + file_y * BMP_ROW_BYTES + x * 3;
        bytes[offset] = rgb[2];
        bytes[offset + 1] = rgb[1];
        bytes[offset + 2] = rgb[0];
    }

    #[test]
    fn bmp_bottom_up_is_flipped_and_palette_mapped() {
        let bytes = bmp(false, |bytes| {
            put_bgr(bytes, 0, 479, [0, 0, 255]);
            put_bgr(bytes, 799, 0, [255, 0, 0]);
        });
        let mut frame = vec![0_u8; panel::buffer_len()];
        decode_bmp(&mut Cursor::new(bytes), &mut frame).unwrap();
        assert_eq!(panel::pixel(&frame, 0, 0), panel::BLUE);
        assert_eq!(panel::pixel(&frame, 799, 479), panel::RED);
    }

    #[test]
    fn bmp_top_down_works() {
        let bytes = bmp(true, |bytes| put_bgr(bytes, 10, 20, [255, 255, 0]));
        let mut frame = vec![0_u8; panel::buffer_len()];
        decode_bmp(&mut Cursor::new(bytes), &mut frame).unwrap();
        assert_eq!(panel::pixel(&frame, 10, 20), panel::YELLOW);
    }

    #[test]
    fn rejects_non_bmp_wrong_size_and_traversal() {
        let mut frame = vec![0_u8; panel::buffer_len()];
        assert!(decode_bmp(&mut Cursor::new(b"not a bmp"), &mut frame).is_err());
        let mut bytes = bmp(false, |_| {});
        bytes[18..22].copy_from_slice(&(panel::WIDTH as i32 - 1).to_le_bytes());
        assert!(decode_bmp(&mut Cursor::new(bytes), &mut frame).is_err());
        assert!(resolve("../secret").is_err());
        assert!(resolve("/absolute").is_err());
        assert!(resolve("a//b").is_err());
        assert_eq!(
            resolve("01_SYS~1/00_init.bmp").unwrap(),
            Path::new("/sdcard/01_SYS~1/00_init.bmp")
        );
    }

    #[test]
    fn raw_requires_exact_length() {
        let mut frame = vec![0_u8; panel::buffer_len()];
        let exact = vec![0x11_u8; panel::buffer_len()];
        decode_raw(&mut Cursor::new(exact), &mut frame).unwrap();
        assert!(frame.iter().all(|&byte| byte == 0x11));
        let short = vec![0_u8; panel::buffer_len() - 1];
        assert!(decode_raw(&mut Cursor::new(short), &mut frame).is_err());
        let mut long = vec![0_u8; panel::buffer_len()];
        long.push(0);
        assert!(decode_raw(&mut Cursor::new(long), &mut frame).is_err());
    }

    #[test]
    fn removal_stays_inside_the_writable_directory() {
        assert!(removable("PFTEST/UPLOADS/UP0001.PF6").is_ok());
        assert!(removable("PFTEST/A.BMP").is_ok());
        assert_eq!(
            removable("PFTEST/UPLOADS/UP0001.PF6").unwrap(),
            Path::new("/sdcard/PFTEST/UPLOADS/UP0001.PF6")
        );
    }

    #[test]
    fn the_vendor_directory_cannot_be_emptied() {
        // The pictures that came with the frame are readable and permanent.
        assert!(removable("06_USE~1/800X480.BMP").is_err());
        assert!(removable("01_SYS~1/00_INIT.BMP").is_err());
    }

    #[test]
    fn removal_refuses_what_is_not_a_picture() {
        // `config save` writes this file; a delete request must not reach it.
        assert!(removable("PFTEST/CONFIG.TXT").is_err());
        assert!(removable("PFTEST/REC.WAV").is_err());
        assert!(removable("PFTEST/UPLOADS/UP0001.PF6/../CONFIG.TXT").is_err());
    }

    #[test]
    fn removal_refuses_to_climb_out() {
        assert!(removable("../PFTEST/UP0001.PF6").is_err());
        assert!(removable("/PFTEST/UP0001.PF6").is_err());
        assert!(removable("PFTEST\\..\\UP0001.PF6").is_err());
        assert!(removable("PFTESTFUPLOADS/UP0001.PF6").is_err());
    }
}
