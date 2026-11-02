//! Response frames: `[status][opcode low byte][data]`, built on the stack.

use crate::wire::{
    DeepSleepError, MAX_RESPONSE_DATA_SIZE, Opcode, PowerOffError, RESP_ACK, RESP_NACK,
};

/// A response of at most [`MAX_RESPONSE_DATA_SIZE`] bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    buf: [u8; MAX_RESPONSE_DATA_SIZE],
    len: usize,
}

impl Response {
    /// `[status][echo]` with no data yet.
    pub const fn new(status: u8, echo: u8) -> Self {
        let mut buf = [0; MAX_RESPONSE_DATA_SIZE];
        buf[0] = status;
        buf[1] = echo;
        Self { buf, len: 2 }
    }

    /// An ACK echoing `opcode`.
    pub const fn ack(opcode: Opcode) -> Self {
        Self::new(RESP_ACK, opcode.echo())
    }

    /// A bare NACK echoing `opcode`, for handlers that define no error byte.
    pub const fn nack(opcode: Opcode) -> Self {
        Self::new(RESP_NACK, opcode.echo())
    }

    /// `[0xFF][0x53][code][0x00]`.
    pub fn deep_sleep_nack(error: DeepSleepError) -> Self {
        Self::nack(Opcode::DeepSleep).with(&[error as u8, 0])
    }

    /// `[0xFF][0x52][code][0x00]`.
    pub fn power_off_nack(error: PowerOffError) -> Self {
        Self::nack(Opcode::PowerOff).with(&[error as u8, 0])
    }

    /// Append data.
    ///
    /// # Panics
    ///
    /// If the frame would exceed [`MAX_RESPONSE_DATA_SIZE`]. Every caller in
    /// this crate appends a bounded amount.
    pub fn with(mut self, data: &[u8]) -> Self {
        let end = self.len + data.len();
        self.buf[self.len..end].copy_from_slice(data);
        self.len = end;
        self
    }

    /// Room left for data.
    pub const fn remaining(&self) -> usize {
        MAX_RESPONSE_DATA_SIZE - self.len
    }

    /// The frame's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_have_the_spec_shapes() {
        assert_eq!(Response::ack(Opcode::ConfigWrite).as_bytes(), &[0x00, 0x41]);
        assert_eq!(
            Response::nack(Opcode::DirectWriteEnd).as_bytes(),
            &[0xFF, 0x72]
        );
        assert_eq!(
            Response::deep_sleep_nack(DeepSleepError::NotBattery).as_bytes(),
            &[0xFF, 0x53, 0x02, 0x00]
        );
        assert_eq!(
            Response::power_off_nack(PowerOffError::Unsupported).as_bytes(),
            &[0xFF, 0x52, 0x00, 0x00]
        );
    }
}
