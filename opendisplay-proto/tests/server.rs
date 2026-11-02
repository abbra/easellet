//! Byte-level conversations with [`Server`], as py-opendisplay would hold them.
//!
//! Requests are built the way py-opendisplay's `protocol/commands.py` builds
//! them; expected responses come from `opendisplay_protocol.h`.

mod common;

use common::{FakeDevice, board_config, exchange, reassemble_config, request};
use opendisplay_proto::config::{Blob, BlobWriter, CONFIG_VERSION, PacketType, PowerOption};
use opendisplay_proto::server::RefreshOutcome;
use opendisplay_proto::wire::DeepSleepError;
use opendisplay_proto::{RefreshMode, Server};
use zerocopy::FromZeros;

/// The frame tests/fixtures/make_fixtures.py compresses.
fn spectra_frame() -> Vec<u8> {
    const CODES: [u8; 6] = [0, 1, 2, 3, 5, 6];
    let nibble = |x: usize, y: usize| {
        let mut v = (x / 40 + y / 30) % 6;
        if (x * 31 + y * 17).is_multiple_of(7) {
            v = (v + 3) % 6;
        }
        CODES[v]
    };
    (0..480)
        .flat_map(|y| {
            (0..800)
                .step_by(2)
                .map(move |x| (nibble(x, y) << 4) | nibble(x + 1, y))
        })
        .collect()
}

/// `tests/fixtures/board-config.bin` is this config; the client vectors are
/// generated from it. `OD_UPDATE_FIXTURES=1` rewrites it after a change.
#[test]
fn board_config_fixture_is_current() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/board-config.bin"
    );
    if std::env::var_os("OD_UPDATE_FIXTURES").is_some() {
        std::fs::write(path, board_config()).unwrap();
    }
    assert_eq!(std::fs::read(path).unwrap(), board_config());
}

#[test]
fn firmware_version_and_msd() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x43]),
        [b"\x00\x43\x01\x02\x07abc1234\x03".to_vec()]
    );
    let mut msd = vec![0x00, 0x44];
    msd.extend(0..16u8);
    assert_eq!(exchange(&mut server, &mut device, &[0x00, 0x44]), [msd]);
}

#[test]
fn config_read_is_chunked_into_100_byte_frames() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    let config = board_config();
    device.config = Some(config.clone());
    let frames = exchange(&mut server, &mut device, &[0x00, 0x40]);
    assert!(frames.iter().all(|frame| frame.len() <= 100));
    assert_eq!(frames[0].len(), 100);
    assert_eq!(frames.len(), (config.len() - 94).div_ceil(96) + 1);
    assert_eq!(reassemble_config(&frames), config);
    Blob::parse(&config).unwrap();
}

#[test]
fn config_read_without_config() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x40]),
        [vec![0xFF, 0x40, 0x00, 0x00]]
    );
    device.config = Some(Vec::new());
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x40]),
        [vec![0x00, 0x40, 0x00, 0x00, 0x00, 0x00]]
    );
}

#[test]
fn config_write_single_frame() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    let mut buf = [0u8; 200];
    let mut writer = BlobWriter::new(&mut buf, CONFIG_VERSION).unwrap();
    writer
        .push(PacketType::POWER, &PowerOption::new_zeroed())
        .unwrap();
    let len = writer.finish().unwrap();
    assert_eq!(
        exchange(&mut server, &mut device, &request(0x41, &buf[..len])),
        [vec![0x00, 0x41]]
    );
    assert_eq!(device.written_config.as_deref(), Some(&buf[..len]));
}

#[test]
fn config_write_chunked_like_the_client() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    let config = board_config();
    // commands.py build_write_config_command: [total:2 LE] + 200, then 200s.
    let mut first = (config.len() as u16).to_le_bytes().to_vec();
    first.extend_from_slice(&config[..200]);
    assert_eq!(
        exchange(&mut server, &mut device, &request(0x41, &first)),
        [vec![0x00, 0x41]]
    );
    for chunk in config[200..].chunks(200) {
        assert_eq!(
            exchange(&mut server, &mut device, &request(0x42, chunk)),
            [vec![0x00, 0x42]]
        );
    }
    assert_eq!(device.written_config, Some(config));
}

#[test]
fn config_write_errors() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    let nack41 = [vec![0xFF, 0x41]];
    let nack42 = [vec![0xFF, 0x42]];
    // A chunk with no write in progress.
    assert_eq!(
        exchange(&mut server, &mut device, &request(0x42, &[1])),
        nack42
    );
    // 201 bytes: too long for one frame, too short for a chunked start.
    assert_eq!(
        exchange(&mut server, &mut device, &request(0x41, &[0; 201])),
        nack41
    );
    // A total that would need more than 20 frames.
    let mut first = 4001u16.to_le_bytes().to_vec();
    first.extend_from_slice(&[0; 200]);
    assert_eq!(
        exchange(&mut server, &mut device, &request(0x41, &first)),
        nack41
    );
    // A corrupted blob is refused at the last chunk, and nothing is stored.
    let mut config = board_config();
    config[50] ^= 0x01;
    let mut first = (config.len() as u16).to_le_bytes().to_vec();
    first.extend_from_slice(&config[..200]);
    exchange(&mut server, &mut device, &request(0x41, &first));
    exchange(&mut server, &mut device, &request(0x42, &config[200..400]));
    assert_eq!(
        exchange(&mut server, &mut device, &request(0x42, &config[400..])),
        nack42
    );
    assert_eq!(device.written_config, None);
    // More bytes than announced.
    let config = board_config();
    let mut first = (config.len() as u16).to_le_bytes().to_vec();
    first.extend_from_slice(&config[..200]);
    exchange(&mut server, &mut device, &request(0x41, &first));
    assert_eq!(
        exchange(&mut server, &mut device, &request(0x42, &[0; 200])),
        [vec![0x00, 0x42]]
    );
    assert_eq!(
        exchange(&mut server, &mut device, &request(0x42, &[0; 200])),
        nack42
    );
}

#[test]
fn config_clear() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    device.config = Some(board_config());
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x45]),
        [vec![0x00, 0x45]]
    );
    assert_eq!(device.config, None);
}

#[test]
fn power_commands() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x52]),
        [vec![0xFF, 0x52, 0x00, 0x00]]
    );
    device.sleep_answer = Some(DeepSleepError::NotBattery);
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x53, 0x01, 0x2C]),
        [vec![0xFF, 0x53, 0x02, 0x00]]
    );
    device.sleep_answer = None;
    assert!(exchange(&mut server, &mut device, &[0x00, 0x53, 0x01, 0x2C]).is_empty());
    assert_eq!(device.slept, Some(Some(300)));
    assert!(exchange(&mut server, &mut device, &[0x00, 0x0F]).is_empty());
    assert!(device.rebooted);
}

#[test]
fn unsupported_commands_are_refused() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    // No key configured: authentication answers NOT_CONFIG.
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x50, 0x00]),
        [vec![0x00, 0x50, 0x03]]
    );
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x76, 0x00]),
        [vec![0xFF, 0x76, 0x07, 0x00]]
    );
    for opcode in [0x51u8, 0x73, 0x75, 0x77, 0x80, 0x81, 0x82, 0x83] {
        assert_eq!(
            exchange(&mut server, &mut device, &[0x00, opcode]),
            [vec![0xFF, opcode]]
        );
    }
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x99]),
        [vec![0xFF, 0x99]]
    );
    assert!(exchange(&mut server, &mut device, &[0x00]).is_empty());
}

#[test]
fn raw_image_over_lan() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    let image = spectra_frame();
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x70]),
        [vec![0x00, 0x70]]
    );
    // device.py _direct_write_chunk_size on LAN: max_frame - 2 = 4092.
    for chunk in image.chunks(4092) {
        assert_eq!(
            exchange(&mut server, &mut device, &request(0x71, chunk)),
            [vec![0x00, 0x71]]
        );
    }
    // build_direct_write_end_with_etag: [refresh][etag:4 BE].
    assert_eq!(
        exchange(
            &mut server,
            &mut device,
            &[0x00, 0x72, 0x00, 0x12, 0x34, 0x56, 0x78]
        ),
        [vec![0x00, 0x72]]
    );
    assert_eq!(device.frame, image);
    assert_eq!(device.refreshed, Some(RefreshMode::Full));
    assert_eq!(server.displayed_etag(), None);

    let mut frames = Vec::new();
    server.refresh_finished(RefreshOutcome::Done, &mut |frame| {
        frames.push(frame.to_vec())
    });
    assert_eq!(frames, [vec![0x00, 0x73]]);
    assert_eq!(server.displayed_etag(), Some(0x1234_5678));
    // Only one notification per refresh.
    server.refresh_finished(RefreshOutcome::Done, &mut |_| panic!("second 0x73"));
}

#[test]
fn compressed_image_with_a_512_byte_window() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    let stream = include_bytes!("fixtures/spectra-800x480-wbits9.zlib");
    // build_direct_write_start_compressed: the START carries the size and as
    // much of the stream as fits in 200 bytes.
    let mut start = 192_000u32.to_le_bytes().to_vec();
    start.extend_from_slice(&stream[..194]);
    assert_eq!(
        exchange(&mut server, &mut device, &request(0x70, &start)),
        [vec![0x00, 0x70]]
    );
    for chunk in stream[194..].chunks(230) {
        assert_eq!(
            exchange(&mut server, &mut device, &request(0x71, chunk)),
            [vec![0x00, 0x71]]
        );
    }
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x72, 0x01]),
        [vec![0x00, 0x72]]
    );
    assert!(device.frame == spectra_frame());
    assert_eq!(device.refreshed, Some(RefreshMode::Fast));

    let mut frames = Vec::new();
    server.refresh_finished(RefreshOutcome::TimedOut, &mut |frame| {
        frames.push(frame.to_vec())
    });
    assert_eq!(frames, [vec![0x00, 0x74]]);
}

#[test]
fn image_errors_release_the_panel() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    // Busy panel.
    device.busy = true;
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x70]),
        [vec![0xFF, 0x70]]
    );
    device.busy = false;
    // DATA and END with nothing started.
    assert_eq!(
        exchange(&mut server, &mut device, &request(0x71, &[1])),
        [vec![0xFF, 0x71]]
    );
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x72, 0x00]),
        [vec![0xFF, 0x72]]
    );
    // Wrong declared size.
    let start = request(0x70, &191_999u32.to_le_bytes());
    assert_eq!(
        exchange(&mut server, &mut device, &start),
        [vec![0xFF, 0x70]]
    );
    assert_eq!(device.aborts, 1);
    // A short image is refused at END.
    exchange(&mut server, &mut device, &[0x00, 0x70]);
    exchange(&mut server, &mut device, &request(0x71, &[0; 4092]));
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x72, 0x00]),
        [vec![0xFF, 0x72]]
    );
    assert_eq!(device.aborts, 2);
    assert_eq!(device.refreshed, None);
    // A second START replaces the first.
    exchange(&mut server, &mut device, &[0x00, 0x70]);
    assert_eq!(
        exchange(&mut server, &mut device, &[0x00, 0x70]),
        [vec![0x00, 0x70]]
    );
    assert_eq!(device.aborts, 3);
    // Too many bytes: 46 chunks fit, the 47th runs past 192,000.
    for _ in 0..46 {
        exchange(&mut server, &mut device, &request(0x71, &[0; 4092]));
    }
    assert_eq!(
        exchange(&mut server, &mut device, &request(0x71, &[0; 4092])),
        [vec![0xFF, 0x71]]
    );
    assert_eq!(device.aborts, 4);
    // Disconnecting mid-transfer releases the claim.
    exchange(&mut server, &mut device, &[0x00, 0x70]);
    server.disconnected(&mut device);
    assert!(!device.claimed);
    assert_eq!(device.aborts, 5);
}
