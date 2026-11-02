//! The crate's client driving the crate's server: config, MSD, and a zlib image
//! that arrives byte for byte and ends in a refresh result.

mod common;

use common::{FakeDevice, board_config, exchange};
use miniz_oxide::deflate::compress_to_vec_zlib;
use opendisplay_proto::client::{self, ConfigReader};
use opendisplay_proto::server::RefreshOutcome;
use opendisplay_proto::wire::Opcode;
use opendisplay_proto::{RefreshMode, Server, board};

#[test]
fn the_config_reassembles_from_its_chunks() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    device.config = Some(board_config());
    let frames = exchange(
        &mut server,
        &mut device,
        &client::simple(Opcode::ConfigRead),
    );
    assert!(frames.len() > 1, "the config needs several chunks");
    let mut reader = ConfigReader::default();
    let mut blob = None;
    for frame in &frames {
        blob = reader.feed(frame).unwrap().map(<[u8]>::to_vec);
    }
    assert_eq!(blob.unwrap(), board_config());
}

#[test]
fn the_msd_record_is_read() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    let frames = exchange(&mut server, &mut device, &client::simple(Opcode::ReadMsd));
    let record = client::msd_record(&frames[0]).unwrap();
    assert_eq!(record.to_vec(), (0..16u8).collect::<Vec<_>>());
}

#[test]
fn a_zlib_image_arrives_whole_and_reports_its_refresh() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    let image: Vec<u8> = (0..board::FRAME_LEN)
        .map(|i| ((i / 977) % 7) as u8)
        .collect();
    let stream = compress_to_vec_zlib(&image, 6);
    let mut out = vec![0u8; opendisplay_proto::wire::LAN_MAX_PAYLOAD];

    let (len, mut sent) = client::write_start_zlib(image.len() as u32, &stream, &mut out);
    let answer = exchange(&mut server, &mut device, &out[..len]);
    client::expect_ack(&answer[0], Opcode::DirectWriteStart).unwrap();
    while sent < stream.len() {
        let (len, took) = client::write_data(&stream[sent..], &mut out);
        sent += took;
        let answer = exchange(&mut server, &mut device, &out[..len]);
        client::expect_ack(&answer[0], Opcode::DirectWriteData).unwrap();
    }
    let mut end = [0u8; 7];
    let len = client::write_end(RefreshMode::Full, Some(0xC0FFEE), &mut end);
    let answer = exchange(&mut server, &mut device, &end[..len]);
    client::expect_ack(&answer[0], Opcode::DirectWriteEnd).unwrap();
    assert_eq!(device.frame, image);
    assert_eq!(device.refreshed, Some(RefreshMode::Full));

    let mut outcome = None;
    server.refresh_finished(RefreshOutcome::Done, &mut |frame| {
        outcome = client::refresh_outcome(frame);
    });
    assert_eq!(outcome, Some(RefreshOutcome::Done));
}

#[test]
fn a_refusal_carries_its_error_code() {
    let (mut server, mut device) = (Server::new(), FakeDevice::new());
    device.busy = true;
    let mut out = [0u8; 64];
    let (len, _) = client::write_start_zlib(10, &[0x78, 0x9C], &mut out);
    let answer = exchange(&mut server, &mut device, &out[..len]);
    assert!(matches!(
        client::expect_ack(&answer[0], Opcode::DirectWriteStart),
        Err(client::ClientError::Refused(Opcode::DirectWriteStart, _))
    ));
}
