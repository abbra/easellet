//! The config blob that CONFIG_READ returns and CONFIG_WRITE carries.
//!
//! Layout: `[length:2 LE][version:1]`, then packets `[number:1][type:1][struct]`
//! whose struct size is fixed by the type, then `[crc:2 LE]` (see [`crate::crc`]).
//! There is no per-packet length, so a reader that meets an unknown type cannot
//! skip it: [`Packets`] stops there and leaves the rest as [`Blob::unparsed_tail`],
//! which keeps configs written by newer tools intact on a round trip.

use zerocopy::byteorder::little_endian::{U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::crc::config_crc;

/// Config-format major version carried in the header byte.
pub const CONFIG_VERSION: u8 = 1;
/// Largest blob, header and CRC included.
pub const MAX_CONFIG_SIZE: usize = 4096;
/// Pin field value for "no pin".
pub const PIN_UNUSED: u8 = 0xFF;

const HEADER_LEN: usize = 3;
const CRC_LEN: usize = 2;

/// A packet type id. Open-ended on purpose: a value this crate does not know is
/// still a valid id, it just has no known size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PacketType(pub u8);

impl PacketType {
    /// [`SystemConfig`], required.
    pub const SYSTEM: Self = Self(0x01);
    /// [`ManufacturerData`], required.
    pub const MANUFACTURER: Self = Self(0x02);
    /// [`PowerOption`], required.
    pub const POWER: Self = Self(0x04);
    /// [`DisplayConfig`], up to 4.
    pub const DISPLAY: Self = Self(0x20);
    /// LED config, up to 4.
    pub const LED: Self = Self(0x21);
    /// Sensor config, up to 4.
    pub const SENSOR: Self = Self(0x23);
    /// Data bus config, up to 4.
    pub const DATA_BUS: Self = Self(0x24);
    /// Binary inputs, up to 4.
    pub const BINARY_INPUT: Self = Self(0x25);
    /// Wi-Fi config.
    pub const WIFI: Self = Self(0x26);
    /// [`SecurityConfig`].
    pub const SECURITY: Self = Self(0x27);
    /// Touch controller, up to 4.
    pub const TOUCH: Self = Self(0x28);
    /// Buzzer, up to 4.
    pub const BUZZER: Self = Self(0x29);
    /// NFC, up to 2.
    pub const NFC: Self = Self(0x2A);
    /// External flash, up to 2.
    pub const FLASH: Self = Self(0x2B);
    /// [`DataExtended`] identity strings.
    pub const DATA_EXTENDED: Self = Self(0x2C);

    /// The struct size that follows a packet header of this type.
    pub const fn wire_size(self) -> Option<usize> {
        Some(match self.0 {
            0x01 | 0x02 | 0x21 => 22,
            0x04 | 0x23 | 0x24 | 0x25 => 30,
            0x20 => 46,
            0x26 => 160,
            0x27 => 64,
            0x28..=0x2B => 32,
            0x2C => 288,
            _ => return None,
        })
    }
}

/// Why a blob was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobError {
    /// Shorter than header plus CRC, or longer than [`MAX_CONFIG_SIZE`].
    BadLength(usize),
    /// The stored CRC does not match the content.
    Crc {
        /// CRC read from the last two bytes.
        stored: u16,
        /// CRC of the content.
        computed: u16,
    },
    /// A known packet runs past the CRC.
    Truncated {
        /// Offset of the packet header within the blob.
        offset: usize,
    },
}

/// A config blob whose CRC and packet framing have been checked.
#[derive(Clone, Copy, Debug)]
pub struct Blob<'a> {
    bytes: &'a [u8],
}

impl<'a> Blob<'a> {
    /// Check the CRC and walk every packet of known type.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, BlobError> {
        if bytes.len() < HEADER_LEN + CRC_LEN || bytes.len() > MAX_CONFIG_SIZE {
            return Err(BlobError::BadLength(bytes.len()));
        }
        let (body, crc) = bytes.split_at(bytes.len() - CRC_LEN);
        let stored = u16::from_le_bytes([crc[0], crc[1]]);
        let computed = config_crc(body);
        if stored != computed {
            return Err(BlobError::Crc { stored, computed });
        }
        let blob = Self { bytes };
        let mut packets = blob.packets();
        for _ in packets.by_ref() {}
        match packets.error {
            Some(error) => Err(error),
            None => Ok(blob),
        }
    }

    /// The whole blob, header and CRC included.
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Header version byte.
    pub fn version(&self) -> u8 {
        self.bytes[2]
    }

    /// The packets of known type, in order, up to the first unknown one.
    pub fn packets(&self) -> Packets<'a> {
        Packets {
            area: self.packet_area(),
            offset: 0,
            error: None,
        }
    }

    /// The bytes from the first packet of unknown type up to the CRC. Empty when
    /// every packet was understood.
    pub fn unparsed_tail(&self) -> &'a [u8] {
        let mut packets = self.packets();
        for _ in packets.by_ref() {}
        &self.packet_area()[packets.offset..]
    }

    /// The first packet of the given type, read as its struct.
    pub fn find<T: FromBytes + KnownLayout + Immutable + Unaligned>(
        &self,
        kind: PacketType,
    ) -> Option<&'a T> {
        let packet = self.packets().find(|packet| packet.kind == kind)?;
        T::ref_from_bytes(packet.body).ok()
    }

    fn packet_area(&self) -> &'a [u8] {
        &self.bytes[HEADER_LEN..self.bytes.len() - CRC_LEN]
    }
}

/// One packet inside a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    /// The packet's index byte.
    pub number: u8,
    /// Its type.
    pub kind: PacketType,
    /// The fixed-size struct, exactly [`PacketType::wire_size`] bytes.
    pub body: &'a [u8],
}

/// Iterator over the packets of a [`Blob`].
#[derive(Clone, Debug)]
pub struct Packets<'a> {
    area: &'a [u8],
    offset: usize,
    error: Option<BlobError>,
}

impl<'a> Iterator for Packets<'a> {
    type Item = Packet<'a>;

    fn next(&mut self) -> Option<Packet<'a>> {
        let rest = &self.area[self.offset..];
        // A single stray byte before the CRC is padding, not a packet.
        let [number, kind, ..] = *rest else {
            return None;
        };
        let kind = PacketType(kind);
        let size = kind.wire_size()?;
        let Some(body) = rest.get(2..2 + size) else {
            self.error = Some(BlobError::Truncated {
                offset: HEADER_LEN + self.offset,
            });
            return None;
        };
        self.offset += 2 + size;
        Some(Packet { number, kind, body })
    }
}

/// The builder ran out of room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferFull;

/// Writes a blob into a caller's buffer.
#[derive(Debug)]
pub struct BlobWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
    count: u8,
}

impl<'a> BlobWriter<'a> {
    /// Start a blob with the given header version.
    pub fn new(buf: &'a mut [u8], version: u8) -> Result<Self, BufferFull> {
        let header = buf.get_mut(..HEADER_LEN).ok_or(BufferFull)?;
        header.copy_from_slice(&[0, 0, version]);
        Ok(Self {
            buf,
            len: HEADER_LEN,
            count: 0,
        })
    }

    /// Append a packet with a known struct, numbered in sequence.
    pub fn push<T: IntoBytes + Immutable>(
        &mut self,
        kind: PacketType,
        packet: &T,
    ) -> Result<(), BufferFull> {
        self.push_raw(kind, packet.as_bytes())
    }

    /// Append a packet given as raw bytes. Unknown types are allowed, so a
    /// stored packet can be written back unchanged.
    pub fn push_raw(&mut self, kind: PacketType, body: &[u8]) -> Result<(), BufferFull> {
        let header = [self.count, kind.0];
        self.append(&header)?;
        self.append(body)?;
        self.count = self.count.wrapping_add(1);
        Ok(())
    }

    /// Append bytes after the packets as they are, such as
    /// [`Blob::unparsed_tail`].
    pub fn push_tail(&mut self, tail: &[u8]) -> Result<(), BufferFull> {
        self.append(tail)
    }

    /// Fill in the length and CRC and return the blob's size.
    pub fn finish(self) -> Result<usize, BufferFull> {
        let total = self.len + CRC_LEN;
        if total > MAX_CONFIG_SIZE || total > self.buf.len() {
            return Err(BufferFull);
        }
        let length = u16::try_from(total).map_err(|_| BufferFull)?;
        let crc = config_crc(&self.buf[..self.len]);
        self.buf[..2].copy_from_slice(&length.to_le_bytes());
        self.buf[self.len..total].copy_from_slice(&crc.to_le_bytes());
        Ok(total)
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), BufferFull> {
        let end = self.len + bytes.len();
        // Leave room for the CRC so `finish` cannot fail on space alone.
        if end + CRC_LEN > self.buf.len() {
            return Err(BufferFull);
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
}

/// `SystemConfig.ic_type` values used here.
pub mod ic_type {
    /// ESP32-S3.
    pub const ESP32S3: u16 = 2;
}

/// `SystemConfig.communication_modes` bits.
pub mod communication_mode {
    /// BLE transfer.
    pub const BLE: u8 = 1 << 0;
    /// OpenEPaperLink transfer.
    pub const OEPL: u8 = 1 << 1;
    /// Wi-Fi transfer.
    pub const WIFI: u8 = 1 << 2;
}

/// `ManufacturerData.manufacturer_id` values used here.
pub mod manufacturer_id {
    /// Self-built devices.
    pub const DIY: u16 = 0;
    /// Waveshare Electronics.
    pub const WAVESHARE: u16 = 2;
}

/// `PowerOption.power_mode` values.
pub mod power_mode {
    /// Battery powered.
    pub const BATTERY: u8 = 1;
    /// USB powered.
    pub const USB: u8 = 2;
    /// Solar powered.
    pub const SOLAR: u8 = 3;
}

/// `PowerOption.capacity_estimator` values used here.
pub mod capacity_estimator {
    /// Single-cell Li-ion.
    pub const LI_ION: u8 = 1;
}

/// `DisplayConfig.display_technology` values used here.
pub mod display_technology {
    /// E-paper.
    pub const E_PAPER: u8 = 1;
}

/// `DisplayConfig.rotation` values: how the panel is mounted. Clients rotate
/// the image by this much before packing it.
pub mod rotation {
    /// Upright.
    pub const ROTATE_0: u8 = 0;
    /// 90 degrees.
    pub const ROTATE_90: u8 = 1;
    /// 180 degrees.
    pub const ROTATE_180: u8 = 2;
    /// 270 degrees.
    pub const ROTATE_270: u8 = 3;
}

/// `DisplayConfig.panel_ic_type` values used here (the full table is upstream's).
pub mod panel_ic {
    /// Spectra 6 7.3-inch, 800x480.
    pub const EP73_SPECTRA_800X480: u16 = 35;
}

/// `DisplayConfig.color_scheme` values used here.
pub mod color_scheme {
    /// Spectra 6: 4 bits per pixel, high nibble first, codes 0-3, 5, 6.
    pub const BWGBRY: u8 = 4;
}

/// `DisplayConfig.transmission_modes` bits.
pub mod transmission_mode {
    /// zlib with a 512-byte window, inflated while streaming.
    pub const STREAMING_DECOMPRESSION: u8 = 1 << 0;
    /// zlib with a full window.
    pub const ZIP: u8 = 1 << 1;
    /// Group 5 compression (not implemented upstream).
    pub const G5: u8 = 1 << 2;
    /// DIRECT_WRITE (0x70-0x72).
    pub const DIRECT_WRITE: u8 = 1 << 3;
    /// PIPE_WRITE (0x80-0x82).
    pub const PIPE_WRITE: u8 = 1 << 4;
    /// Clear the screen on boot.
    pub const CLEAR_ON_BOOT: u8 = 1 << 7;
}

/// Packet 0x01: host IC and power-management pins. 22 bytes.
#[derive(Clone, Debug, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
#[allow(missing_docs)]
pub struct SystemConfig {
    pub ic_type: U16,
    pub communication_modes: u8,
    pub device_flags: u8,
    pub pwr_pin: u8,
    pub reserved: [u8; 15],
    pub pwr_pin_2: u8,
    pub pwr_pin_3: u8,
}

/// Packet 0x02: manufacturer and board identity. 22 bytes.
#[derive(Clone, Debug, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
#[allow(missing_docs)]
pub struct ManufacturerData {
    pub manufacturer_id: U16,
    pub board_type: u8,
    pub board_revision: u8,
    pub simple_config_driver_index: U16,
    pub simple_config_display_index: U16,
    pub simple_config_power_index: U16,
    pub simple_config_configured_at: [u8; 6],
    pub reserved: [u8; 6],
}

/// Packet 0x04: power source, battery and sleep timing. 30 bytes.
#[derive(Clone, Debug, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
#[allow(missing_docs)]
pub struct PowerOption {
    pub power_mode: u8,
    pub battery_capacity_mah: [u8; 3],
    pub sleep_timeout_ms: U16,
    pub tx_power: u8,
    pub sleep_flags: u8,
    pub battery_sense_pin: u8,
    pub battery_sense_enable_pin: u8,
    pub battery_sense_flags: u8,
    pub capacity_estimator: u8,
    pub voltage_scaling_factor: U16,
    pub deep_sleep_current_ua: U32,
    pub deep_sleep_time_seconds: U16,
    pub charge_enable_pin: u8,
    pub charge_state_pin: u8,
    pub charger_flags: u8,
    pub min_wake_time_seconds: U16,
    pub screen_timeout_seconds: u8,
    pub reserved: [u8; 4],
}

/// Packet 0x20: one panel's geometry, controller, colours and transfer modes.
/// 46 bytes.
#[derive(Clone, Debug, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
#[allow(missing_docs)]
pub struct DisplayConfig {
    pub instance_number: u8,
    pub display_technology: u8,
    pub panel_ic_type: U16,
    pub pixel_width: U16,
    pub pixel_height: U16,
    pub active_width_mm: U16,
    pub active_height_mm: U16,
    pub legacy_tag_type: U16,
    pub rotation: u8,
    pub reset_pin: u8,
    pub busy_pin: u8,
    pub dc_pin: u8,
    pub cs_pin: u8,
    pub data_pin: u8,
    pub partial_update_support: u8,
    pub color_scheme: u8,
    pub transmission_modes: u8,
    pub clk_pin: u8,
    pub cs_pin_2: u8,
    pub reserved_pins: [u8; 6],
    pub full_update_mc: U16,
    pub reserved: [u8; 13],
}

/// Packet 0x27: the AES-128 master key and its policy. 64 bytes.
#[derive(Clone, Debug, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
#[allow(missing_docs)]
pub struct SecurityConfig {
    pub encryption_enabled: u8,
    pub encryption_key: [u8; 16],
    pub session_timeout_seconds: U16,
    pub flags: u8,
    pub reset_pin: u8,
    pub reserved: [u8; 43],
}

impl SecurityConfig {
    /// Encryption is on only with the flag set and a key that is not all zero.
    pub fn is_enabled(&self) -> bool {
        self.encryption_enabled == 1 && self.encryption_key.iter().any(|&byte| byte != 0)
    }
}

/// Packet 0x24: a shared bus other packets refer to by instance. 30 bytes.
#[derive(Clone, Debug, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
#[allow(missing_docs)]
pub struct DataBus {
    pub instance_number: u8,
    pub bus_type: u8,
    pub pin_1: u8,
    pub pin_2: u8,
    pub pin_3: u8,
    pub pin_4: u8,
    pub pin_5: u8,
    pub pin_6: u8,
    pub pin_7: u8,
    pub bus_speed_hz: U32,
    pub bus_flags: u8,
    pub pullups: u8,
    pub pulldowns: u8,
    pub reserved: [u8; 14],
}

/// `DataBus.bus_type` values.
pub mod bus_type {
    /// I2C.
    pub const I2C: u8 = 1;
}

/// Packet 0x23: a sensor on a bus, and where it publishes in the MSD. 30 bytes.
#[derive(Clone, Debug, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
#[allow(missing_docs)]
pub struct SensorData {
    pub instance_number: u8,
    pub sensor_type: U16,
    pub bus_id: u8,
    pub i2c_addr_7bit: u8,
    pub msd_data_start_byte: u8,
    pub reserved: [u8; 24],
}

/// `SensorData.sensor_type` values used here.
pub mod sensor_type {
    /// X-Powers AXP2101 PMIC; the source of the battery voltage in the MSD.
    pub const AXP2101: u16 = 3;
    /// Sensirion SHT40 temperature/humidity; its 3-byte MSD block is the format.
    pub const SHT40: u16 = 4;
}

/// Packet 0x2C: identity strings, each NUL-padded UTF-8. 288 bytes.
#[derive(Clone, Debug, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
#[allow(missing_docs)]
pub struct DataExtended {
    pub manufacturer_name: [u8; 32],
    pub model_name: [u8; 32],
    pub serial_number: [u8; 32],
    pub friendly_name: [u8; 32],
    pub device_location: [u8; 32],
    pub device_id: [u8; 32],
    pub custom_string_1: [u8; 32],
    pub custom_string_2: [u8; 32],
    pub custom_string_3: [u8; 32],
}

/// Copy `text` into a NUL-padded field, cutting it at a UTF-8 boundary so the
/// field always keeps a terminating NUL.
pub fn set_text(field: &mut [u8], text: &str) {
    let mut end = text.len().min(field.len().saturating_sub(1));
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    field.fill(0);
    field[..end].copy_from_slice(&text.as_bytes()[..end]);
}

/// The text of a NUL-padded field, up to the first NUL. `None` if it is not
/// UTF-8.
pub fn text(field: &[u8]) -> Option<&str> {
    let end = field
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(field.len());
    core::str::from_utf8(&field[..end]).ok()
}

const _: () = {
    assert!(size_of::<SystemConfig>() == 22);
    assert!(size_of::<ManufacturerData>() == 22);
    assert!(size_of::<PowerOption>() == 30);
    assert!(size_of::<DisplayConfig>() == 46);
    assert!(size_of::<SecurityConfig>() == 64);
    assert!(size_of::<DataExtended>() == 288);
};

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::FromZeros;

    fn sample(buf: &mut [u8]) -> usize {
        let mut writer = BlobWriter::new(buf, CONFIG_VERSION).unwrap();
        let mut system = SystemConfig::new_zeroed();
        system.ic_type = ic_type::ESP32S3.into();
        writer.push(PacketType::SYSTEM, &system).unwrap();
        let mut display = DisplayConfig::new_zeroed();
        display.pixel_width = 800.into();
        writer.push(PacketType::DISPLAY, &display).unwrap();
        writer.finish().unwrap()
    }

    #[test]
    fn writer_output_parses_back() {
        let mut buf = [0u8; 256];
        let len = sample(&mut buf);
        assert_eq!(len, 3 + 2 + 22 + 2 + 46 + 2);
        assert_eq!(u16::from_le_bytes([buf[0], buf[1]]) as usize, len);
        let blob = Blob::parse(&buf[..len]).unwrap();
        assert_eq!(blob.version(), CONFIG_VERSION);
        let kinds: [PacketType; 2] = core::array::from_fn(|i| blob.packets().nth(i).unwrap().kind);
        assert_eq!(kinds, [PacketType::SYSTEM, PacketType::DISPLAY]);
        assert_eq!(blob.packets().nth(1).unwrap().number, 1);
        let display: &DisplayConfig = blob.find(PacketType::DISPLAY).unwrap();
        assert_eq!(display.pixel_width.get(), 800);
        assert!(blob.unparsed_tail().is_empty());
    }

    #[test]
    fn rejects_a_bad_crc_and_a_truncated_packet() {
        let mut buf = [0u8; 256];
        let len = sample(&mut buf);
        buf[10] ^= 1;
        assert!(matches!(
            Blob::parse(&buf[..len]),
            Err(BlobError::Crc { .. })
        ));

        // A display packet header with only 10 of its 46 bytes, CRC fixed up.
        let mut short = [0u8; 3 + 2 + 10 + 2];
        short[2] = CONFIG_VERSION;
        short[3..5].copy_from_slice(&[0, PacketType::DISPLAY.0]);
        let crc = config_crc(&short[..15]);
        short[15..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            Blob::parse(&short).unwrap_err(),
            BlobError::Truncated { offset: 3 }
        );
    }

    #[test]
    fn keeps_unknown_packets_as_the_tail() {
        let mut buf = [0u8; 256];
        let mut writer = BlobWriter::new(&mut buf, CONFIG_VERSION).unwrap();
        writer
            .push(PacketType::SYSTEM, &SystemConfig::new_zeroed())
            .unwrap();
        writer.push_raw(PacketType(0x7E), &[1, 2, 3]).unwrap();
        let len = writer.finish().unwrap();
        let blob = Blob::parse(&buf[..len]).unwrap();
        assert_eq!(blob.packets().count(), 1);
        assert_eq!(blob.unparsed_tail(), &[1, 0x7E, 1, 2, 3]);
    }

    #[test]
    fn length_field_is_ignored_by_the_parser() {
        let mut buf = [0u8; 256];
        let len = sample(&mut buf);
        buf[..2].fill(0);
        assert!(Blob::parse(&buf[..len]).is_ok());
    }

    #[test]
    fn text_fields_truncate_on_char_boundaries() {
        let mut field = [0xAAu8; 4];
        set_text(&mut field, "aéz");
        assert_eq!(&field, b"a\xC3\xA9\0");
        assert_eq!(text(&field), Some("aé"));
        set_text(&mut field, "абв");
        assert_eq!(text(&field), Some("а"));
    }
}
