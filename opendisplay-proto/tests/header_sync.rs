//! The crate's constants against the vendored upstream headers. A header update
//! that changes a value, adds an opcode or resizes a packet fails here.

use std::collections::BTreeMap;

use opendisplay_proto::config::{
    DataBus, DataExtended, DisplayConfig, ManufacturerData, PacketType, PowerOption,
    SecurityConfig, SensorData, SystemConfig,
};
use opendisplay_proto::wire::{self, Opcode};

const PROTOCOL_H: &str = include_str!("../upstream/opendisplay_protocol.h");
const STRUCTS_H: &str = include_str!("../upstream/opendisplay_structs.h");

/// `#define NAME VALUE` lines, values as written (`0x0040u`, `"2.2"`, ...).
fn defines(header: &str) -> BTreeMap<&str, &str> {
    header
        .lines()
        .filter_map(|line| {
            let mut words = line.strip_prefix("#define ")?.split_whitespace();
            Some((words.next()?, words.next()?))
        })
        .collect()
}

fn number(value: &str) -> u64 {
    let value = value.trim_end_matches('u');
    match value.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).unwrap(),
        None => value.parse().unwrap(),
    }
}

fn value(defines: &BTreeMap<&str, &str>, name: &str) -> u64 {
    let mut raw = defines[name];
    // AUTH_STATUS_SUCCESS is defined as another macro.
    while let Some(target) = defines.get(raw) {
        raw = target;
    }
    number(raw)
}

#[test]
fn every_opcode_matches() {
    let defines = defines(PROTOCOL_H);
    let opcodes: Vec<(&str, u64)> = defines
        .keys()
        .filter(|name| name.starts_with("CMD_"))
        .map(|name| (*name, value(&defines, name)))
        .collect();
    assert_eq!(opcodes.len(), Opcode::ALL.len(), "{opcodes:?}");
    for (name, raw) in opcodes {
        let opcode = Opcode::from_u16(raw as u16);
        assert!(opcode.is_some(), "{name} = {raw:#06x} has no Opcode");
    }
}

#[test]
fn scalar_constants_match() {
    let defines = defines(PROTOCOL_H);
    let ours: &[(&str, u64)] = &[
        ("RESP_ACK", wire::RESP_ACK.into()),
        ("RESP_NACK", wire::RESP_NACK.into()),
        ("RESP_AUTH_REQUIRED", wire::RESP_AUTH_REQUIRED.into()),
        (
            "RESP_DIRECT_WRITE_REFRESH_SUCCESS",
            wire::RESP_REFRESH_SUCCESS.into(),
        ),
        (
            "RESP_DIRECT_WRITE_REFRESH_TIMEOUT",
            wire::RESP_REFRESH_TIMEOUT.into(),
        ),
        ("AUTH_STATUS_CHALLENGE", wire::auth_status::CHALLENGE.into()),
        ("AUTH_STATUS_SUCCESS", wire::auth_status::SUCCESS.into()),
        ("AUTH_STATUS_FAILED", wire::auth_status::FAILED.into()),
        ("AUTH_STATUS_ALREADY", wire::auth_status::ALREADY.into()),
        (
            "AUTH_STATUS_NOT_CONFIG",
            wire::auth_status::NOT_CONFIG.into(),
        ),
        (
            "AUTH_STATUS_RATE_LIMIT",
            wire::auth_status::RATE_LIMIT.into(),
        ),
        ("AUTH_STATUS_ERROR", wire::auth_status::ERROR.into()),
        (
            "OD_ERR_DEEP_SLEEP_UNSUPPORTED",
            wire::DeepSleepError::Unsupported as u64,
        ),
        (
            "OD_ERR_DEEP_SLEEP_DISABLED",
            wire::DeepSleepError::Disabled as u64,
        ),
        (
            "OD_ERR_DEEP_SLEEP_NOT_BATTERY",
            wire::DeepSleepError::NotBattery as u64,
        ),
        (
            "OD_ERR_POWER_OFF_UNSUPPORTED",
            wire::PowerOffError::Unsupported as u64,
        ),
        (
            "OD_ERR_PIPE_START_BAD_HEADER",
            wire::PipeStartError::BadHeader as u64,
        ),
        (
            "OD_ERR_PIPE_START_UNKNOWN_FLAG",
            wire::PipeStartError::UnknownFlag as u64,
        ),
        (
            "OD_ERR_PIPE_START_SIZE_MISMATCH",
            wire::PipeStartError::SizeMismatch as u64,
        ),
        (
            "OD_ERR_PIPE_START_ETAG_MISMATCH",
            wire::PipeStartError::EtagMismatch as u64,
        ),
        (
            "OD_ERR_PIPE_START_PARTIAL_UNSUPPORTED",
            wire::PipeStartError::PartialUnsupported as u64,
        ),
        (
            "OD_ERR_PIPE_START_RECT_INVALID",
            wire::PipeStartError::RectInvalid as u64,
        ),
        (
            "OD_ERR_PARTIAL_ETAG_MISMATCH",
            wire::PartialWriteError::EtagMismatch as u64,
        ),
        (
            "OD_ERR_PARTIAL_RECT_OOB",
            wire::PartialWriteError::RectOutOfBounds as u64,
        ),
        (
            "OD_ERR_PARTIAL_RECT_ALIGN",
            wire::PartialWriteError::RectAlign as u64,
        ),
        (
            "OD_ERR_PARTIAL_FLAGS",
            wire::PartialWriteError::Flags as u64,
        ),
        (
            "OD_ERR_PARTIAL_STREAM",
            wire::PartialWriteError::Stream as u64,
        ),
        (
            "OD_ERR_PARTIAL_UNSUPPORTED",
            wire::PartialWriteError::Unsupported as u64,
        ),
        ("CONFIG_CHUNK_SIZE", wire::CONFIG_CHUNK_SIZE as u64),
        (
            "CONFIG_CHUNK_SIZE_WITH_PREFIX",
            wire::CONFIG_CHUNK_SIZE_WITH_PREFIX as u64,
        ),
        ("MAX_CONFIG_CHUNKS", wire::MAX_CONFIG_CHUNKS as u64),
        (
            "MAX_RESPONSE_DATA_SIZE",
            wire::MAX_RESPONSE_DATA_SIZE as u64,
        ),
        ("OD_BLE_MAX_FRAME", wire::BLE_MAX_FRAME as u64),
        ("PIPE_VERSION", wire::PIPE_VERSION.into()),
        ("PIPE_FLAG_COMPRESSED", wire::PIPE_FLAG_COMPRESSED.into()),
        ("PIPE_FLAG_PARTIAL", wire::PIPE_FLAG_PARTIAL.into()),
        ("PIPE_MAX_FRAME", wire::PIPE_MAX_FRAME as u64),
        ("PIPE_FRAME_OVERHEAD", wire::PIPE_FRAME_OVERHEAD as u64),
        ("PIPE_ACK_MASK_BITS", wire::PIPE_ACK_MASK_BITS as u64),
        ("BLE_CMD_HEADER_SIZE", wire::BLE_CMD_HEADER_SIZE as u64),
        ("ENCRYPTION_NONCE_SIZE", wire::ENCRYPTION_NONCE_SIZE as u64),
        ("ENCRYPTION_TAG_SIZE", wire::ENCRYPTION_TAG_SIZE as u64),
        ("OD_LAN_TCP_PORT", wire::LAN_TCP_PORT.into()),
        ("OD_LAN_TLS_PORT", wire::LAN_TLS_PORT.into()),
        ("OD_LAN_MAX_FRAME", wire::LAN_MAX_FRAME as u64),
        ("OD_LAN_MAX_PAYLOAD", wire::LAN_MAX_PAYLOAD as u64),
        ("OD_LAN_READ_TIMEOUT_S", wire::LAN_READ_TIMEOUT_S.into()),
    ];
    for (name, ours) in ours {
        assert_eq!(value(&defines, name), *ours, "{name}");
    }
    assert_eq!(
        defines["OD_PROTOCOL_VERSION_STR"],
        format!("\"{}\"", wire::PROTOCOL_VERSION)
    );
    assert_eq!(
        defines["OD_LAN_MDNS_SERVICE"],
        format!("\"{}\"", wire::LAN_MDNS_SERVICE)
    );
}

/// `NAME = 0x01,` entries of a C enum, in order.
fn enum_values<'a>(header: &'a str, name: &str) -> Vec<(&'a str, u64)> {
    let start = header.find(&format!("enum {name} {{")).unwrap();
    let body = &header[start..start + header[start..].find("};").unwrap()];
    body.lines()
        .filter_map(|line| {
            let (key, rest) = line.trim().split_once('=')?;
            let value = rest.trim().split([',', ' ']).next()?;
            Some((key.trim(), number(value)))
        })
        .collect()
}

#[test]
fn packet_types_and_sizes_match() {
    // Every packet type struct and the size its static assert pins.
    let sizes: BTreeMap<&str, u64> = STRUCTS_H
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("OD_STATIC_ASSERT(sizeof(struct ")?;
            let (name, rest) = rest.split_once(')')?;
            let size = rest.trim_start_matches(" == ").split(',').next()?;
            Some((name, number(size)))
        })
        .collect();
    let packet_structs = [
        ("OD_PKT_SYSTEM", "SystemConfig"),
        ("OD_PKT_MANUFACTURER", "ManufacturerData"),
        ("OD_PKT_POWER", "PowerOption"),
        ("OD_PKT_DISPLAY", "DisplayConfig"),
        ("OD_PKT_LED", "LedConfig"),
        ("OD_PKT_SENSOR", "SensorData"),
        ("OD_PKT_DATA_BUS", "DataBus"),
        ("OD_PKT_BINARY_INPUT", "BinaryInputs"),
        ("OD_PKT_WIFI", "WifiConfig"),
        ("OD_PKT_SECURITY", "SecurityConfig"),
        ("OD_PKT_TOUCH", "TouchController"),
        ("OD_PKT_BUZZER", "BuzzerConfig"),
        ("OD_PKT_NFC", "NfcConfig"),
        ("OD_PKT_FLASH", "FlashConfig"),
        ("OD_PKT_DATA_EXTENDED", "DataExtended"),
    ];
    let types = enum_values(STRUCTS_H, "ConfigPacketType");
    assert_eq!(types.len(), packet_structs.len(), "{types:?}");
    for ((name, id), (expected_name, struct_name)) in types.into_iter().zip(packet_structs) {
        assert_eq!(name, expected_name);
        let size = PacketType(id as u8).wire_size();
        assert_eq!(size.map(|s| s as u64), Some(sizes[struct_name]), "{name}");
    }

    let ours = [
        ("SystemConfig", size_of::<SystemConfig>()),
        ("ManufacturerData", size_of::<ManufacturerData>()),
        ("PowerOption", size_of::<PowerOption>()),
        ("DisplayConfig", size_of::<DisplayConfig>()),
        ("SecurityConfig", size_of::<SecurityConfig>()),
        ("DataExtended", size_of::<DataExtended>()),
        ("SensorData", size_of::<SensorData>()),
        ("DataBus", size_of::<DataBus>()),
    ];
    for (name, size) in ours {
        assert_eq!(size as u64, sizes[name], "{name}");
    }
}

#[test]
fn config_format_and_display_values_match() {
    let defines = defines(STRUCTS_H);
    use opendisplay_proto::config as c;
    assert_eq!(
        value(&defines, "OD_CONFIG_VERSION"),
        c::CONFIG_VERSION.into()
    );
    assert_eq!(value(&defines, "OD_PIN_UNUSED"), c::PIN_UNUSED.into());

    let lookup = |name: &str, key: &str| {
        enum_values(STRUCTS_H, name)
            .into_iter()
            .find(|(k, _)| *k == key)
            .unwrap_or_else(|| panic!("{key}"))
            .1
    };
    assert_eq!(
        lookup("ICType", "OD_IC_TYPE_ESP32S3"),
        c::ic_type::ESP32S3.into()
    );
    assert_eq!(
        lookup("ManufacturerId", "OD_MANUFACTURER_WAVESHARE"),
        c::manufacturer_id::WAVESHARE.into()
    );
    assert_eq!(
        lookup("PowerMode", "OD_POWER_MODE_BATTERY"),
        c::power_mode::BATTERY.into()
    );
    assert_eq!(
        lookup("Rotation", "OD_ROTATION_180"),
        c::rotation::ROTATE_180.into()
    );
    assert_eq!(
        lookup("ColorScheme", "OD_COLOR_SCHEME_BWGBRY"),
        c::color_scheme::BWGBRY.into()
    );
    assert_eq!(
        lookup("PanelIC", "OD_PANEL_IC_EP73_SPECTRA_800X480"),
        c::panel_ic::EP73_SPECTRA_800X480.into()
    );
    assert_eq!(
        lookup("DisplayTechnology", "OD_DISPLAY_TECH_E_PAPER"),
        c::display_technology::E_PAPER.into()
    );

    // Bit macros are written `(1u << n)`, so compare the shift.
    let bit = |name: &str| {
        let line = STRUCTS_H
            .lines()
            .find(|line| line.starts_with(&format!("#define {name} ")))
            .unwrap_or_else(|| panic!("{name}"));
        let shift = line.split("<< ").nth(1).unwrap().split(')').next().unwrap();
        1u64 << shift.parse::<u32>().unwrap()
    };
    let modes = [
        (
            "OD_TRANSMISSION_MODE_STREAMING_DECOMPRESSION",
            c::transmission_mode::STREAMING_DECOMPRESSION,
        ),
        ("OD_TRANSMISSION_MODE_ZIP", c::transmission_mode::ZIP),
        ("OD_TRANSMISSION_MODE_G5", c::transmission_mode::G5),
        (
            "OD_TRANSMISSION_MODE_DIRECT_WRITE",
            c::transmission_mode::DIRECT_WRITE,
        ),
        (
            "OD_TRANSMISSION_MODE_PIPE_WRITE",
            c::transmission_mode::PIPE_WRITE,
        ),
        (
            "OD_TRANSMISSION_MODE_CLEAR_ON_BOOT",
            c::transmission_mode::CLEAR_ON_BOOT,
        ),
        ("OD_COMM_MODE_BLE", c::communication_mode::BLE),
        ("OD_COMM_MODE_OEPL", c::communication_mode::OEPL),
        ("OD_COMM_MODE_WIFI", c::communication_mode::WIFI),
    ];
    for (name, ours) in modes {
        assert_eq!(bit(name), ours.into(), "{name}");
    }
}
