//! The config this firmware reports for the Waveshare ESP32-S3-PhotoPainter.
//!
//! The device does not load its hardware description from the config: the pins
//! are fixed in the firmware. So the hardware packets are generated here every
//! time and every pin reads 0xFF ("none"), which also means a toolbox write can
//! never repoint the panel. The only client-writable part kept is the names
//! packet ([`DataExtended`]).

use zerocopy::FromZeros;

use crate::config::{
    BlobWriter, BufferFull, CONFIG_VERSION, DataBus, DataExtended, DisplayConfig, ManufacturerData,
    PIN_UNUSED, PacketType, PowerOption, SensorData, SystemConfig, bus_type, color_scheme,
    communication_mode, display_technology, ic_type, manufacturer_id, panel_ic, power_mode,
    rotation, sensor_type, set_text, transmission_mode,
};
use crate::msd;

/// Panel width in pixels.
pub const WIDTH: u16 = 800;
/// Panel height in pixels.
pub const HEIGHT: u16 = 480;
/// One frame: 4 bits per pixel.
pub const FRAME_LEN: usize = WIDTH as usize * HEIGHT as usize / 2;

/// Names reported until a client writes its own.
pub fn default_names() -> DataExtended {
    let mut names = DataExtended::new_zeroed();
    set_text(&mut names.manufacturer_name, "Waveshare");
    set_text(&mut names.model_name, "ESP32-S3-PhotoPainter");
    set_text(&mut names.friendly_name, "photoframe");
    names
}

/// Write the PhotoPainter's config blob into `buf` and return its length.
/// `names` replaces [`default_names`] when a client has stored its own;
/// `deep_sleep_secs` is the frame's timed wake (0 = it does not sleep by
/// itself), so a client knows when to expect it back.
pub fn photopainter_config(
    buf: &mut [u8],
    names: Option<&DataExtended>,
    deep_sleep_secs: u16,
) -> Result<usize, BufferFull> {
    let mut writer = BlobWriter::new(buf, CONFIG_VERSION)?;

    let mut system = SystemConfig::new_zeroed();
    system.ic_type = ic_type::ESP32S3.into();
    // Wi-Fi only until the BLE transport exists.
    system.communication_modes = communication_mode::WIFI;
    system.pwr_pin = PIN_UNUSED;
    system.pwr_pin_2 = PIN_UNUSED;
    system.pwr_pin_3 = PIN_UNUSED;
    writer.push(PacketType::SYSTEM, &system)?;

    let mut manufacturer = ManufacturerData::new_zeroed();
    manufacturer.manufacturer_id = manufacturer_id::WAVESHARE.into();
    writer.push(PacketType::MANUFACTURER, &manufacturer)?;

    // Battery powered: the board carries a Li-ion cell behind its AXP2101, which
    // the firmware reads (the voltage goes out in the MSD). Capacity and sleep
    // current are unknown, which the spec spells 0.
    let mut power = PowerOption::new_zeroed();
    power.power_mode = power_mode::BATTERY;
    power.deep_sleep_time_seconds = deep_sleep_secs.into();
    power.battery_sense_pin = PIN_UNUSED;
    power.battery_sense_enable_pin = PIN_UNUSED;
    power.charge_enable_pin = PIN_UNUSED;
    power.charge_state_pin = PIN_UNUSED;
    writer.push(PacketType::POWER, &power)?;

    let mut display = DisplayConfig::new_zeroed();
    display.display_technology = display_technology::E_PAPER;
    display.panel_ic_type = panel_ic::EP73_SPECTRA_800X480.into();
    display.pixel_width = WIDTH.into();
    display.pixel_height = HEIGHT.into();
    // GDEP073E01 active area.
    display.active_width_mm = 160.into();
    display.active_height_mm = 96.into();
    // The panel scans from what the viewer sees as the bottom-right corner.
    // Clients turn the image by this before packing, so the bytes arrive in
    // the panel's own order.
    display.rotation = rotation::ROTATE_180;
    display.reset_pin = PIN_UNUSED;
    display.busy_pin = PIN_UNUSED;
    display.dc_pin = PIN_UNUSED;
    display.cs_pin = PIN_UNUSED;
    display.data_pin = PIN_UNUSED;
    display.clk_pin = PIN_UNUSED;
    display.cs_pin_2 = PIN_UNUSED;
    display.color_scheme = color_scheme::BWGBRY;
    display.transmission_modes = transmission_mode::STREAMING_DECOMPRESSION
        | transmission_mode::ZIP
        | transmission_mode::DIRECT_WRITE;
    writer.push(PacketType::DISPLAY, &display)?;

    // The SHTC3 on the shared I2C bus (SCL 48, SDA 47). The spec has no SHTC3,
    // so it is declared as the SHT40, whose 3-byte MSD block is exactly what the
    // firmware publishes: clients decode temperature and humidity the same way.
    let mut bus = DataBus::new_zeroed();
    bus.bus_type = bus_type::I2C;
    bus.pin_1 = PIN_UNUSED;
    bus.pin_2 = PIN_UNUSED;
    bus.pin_3 = PIN_UNUSED;
    bus.pin_4 = PIN_UNUSED;
    bus.pin_5 = PIN_UNUSED;
    bus.pin_6 = PIN_UNUSED;
    bus.pin_7 = PIN_UNUSED;
    bus.bus_speed_hz = 100_000.into();
    writer.push(PacketType::DATA_BUS, &bus)?;
    let mut sensor = SensorData::new_zeroed();
    sensor.sensor_type = sensor_type::SHT40.into();
    sensor.i2c_addr_7bit = 0x70;
    sensor.msd_data_start_byte = msd::AMBIENT_SLOT as u8;
    writer.push(PacketType::SENSOR, &sensor)?;
    // The PMIC on the same bus: its battery voltage is MSD bytes 14-15, so it
    // takes no dynamic slot (0xFF).
    let mut pmic = SensorData::new_zeroed();
    pmic.instance_number = 1;
    pmic.sensor_type = sensor_type::AXP2101.into();
    pmic.i2c_addr_7bit = 0x34;
    pmic.msd_data_start_byte = 0xFF;
    writer.push(PacketType::SENSOR, &pmic)?;

    let defaults;
    let names = match names {
        Some(names) => names,
        None => {
            defaults = default_names();
            &defaults
        }
    };
    writer.push(PacketType::DATA_EXTENDED, names)?;
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Blob, text};

    #[test]
    fn config_parses_and_carries_the_panel() {
        let mut buf = [0u8; crate::config::MAX_CONFIG_SIZE];
        let len = photopainter_config(&mut buf, None, 0).unwrap();
        let blob = Blob::parse(&buf[..len]).unwrap();
        let display: &DisplayConfig = blob.find(PacketType::DISPLAY).unwrap();
        assert_eq!(display.panel_ic_type.get(), 35);
        assert_eq!(display.rotation, 2);
        assert_eq!(display.color_scheme, 4);
        let names: &DataExtended = blob.find(PacketType::DATA_EXTENDED).unwrap();
        assert_eq!(text(&names.model_name), Some("ESP32-S3-PhotoPainter"));
        assert_eq!(FRAME_LEN, 192_000);
        let sensor: &SensorData = blob.find(PacketType::SENSOR).unwrap();
        assert_eq!(sensor.sensor_type.get(), sensor_type::SHT40);
        assert_eq!(usize::from(sensor.msd_data_start_byte), msd::AMBIENT_SLOT);
        let power: &PowerOption = blob.find(PacketType::POWER).unwrap();
        assert_eq!(power.power_mode, power_mode::BATTERY);
    }

    #[test]
    fn stored_names_replace_the_defaults() {
        let mut names = default_names();
        set_text(&mut names.friendly_name, "kitchen");
        let mut buf = [0u8; crate::config::MAX_CONFIG_SIZE];
        let len = photopainter_config(&mut buf, Some(&names), 0).unwrap();
        let blob = Blob::parse(&buf[..len]).unwrap();
        let stored: &DataExtended = blob.find(PacketType::DATA_EXTENDED).unwrap();
        assert_eq!(text(&stored.friendly_name), Some("kitchen"));
    }
}
