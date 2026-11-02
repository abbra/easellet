//! Device side of the OpenDisplay protocol (spec 2.2), without I/O.
//!
//! A transport hands [`Server::handle`] one request frame at a time and sends
//! whatever response frames come back. The firmware supplies the hardware
//! through the [`Device`] trait. Nothing here allocates, blocks or touches a
//! radio, so every byte on the wire is testable on the host.
//!
//! Layout follows the protocol: [`wire`] holds the constants, [`request`]
//! parses frames, [`config`] reads and writes the config blob, [`image`]
//! reassembles DIRECT_WRITE frames, [`lan`] is the TCP framing and [`server`]
//! ties them together. [`board`] describes the PhotoPainter, and [`mdns`]
//! answers discovery queries for it.
#![no_std]

pub mod board;
pub mod client;
pub mod config;
pub mod crc;
pub mod image;
pub mod lan;
pub mod mdns;
pub mod msd;
pub mod request;
pub mod response;
pub mod server;
pub mod wire;

pub use request::{ParseError, RefreshMode, Request};
pub use server::{Busy, Device, FirmwareVersion, Server, StorageError};
pub use wire::Opcode;
