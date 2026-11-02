//! µ-law companding, matching `src/pcm.rs` in the firmware.
//!
//! The device uploads µ-law because it halves the bytes on the wire and costs one
//! integer operation per sample; the bridge therefore has to decode exactly what the
//! device encoded. The two implementations are kept independent on purpose - a shared
//! bug would hide itself - and `cargo test` here checks the boundary values against
//! the ones the firmware's own tests pin.

/// Decode one µ-law byte. Mirrors `pcm::ulaw_decode`: bias removal, exponent and
/// mantissa reassembly, sign from bit 7.
pub fn decode(byte: u8) -> i16 {
    const BIAS: i32 = 0x84;
    let value = !byte;
    let magnitude = ((((value & 0x0F) as i32) << 3) + BIAS) << ((value & 0x70) >> 4);
    let sample = if value & 0x80 != 0 {
        BIAS - magnitude
    } else {
        magnitude - BIAS
    };
    sample.clamp(-32_768, 32_767) as i16
}

/// Encode one sample. Mirrors `pcm::ulaw_encode`, used for the synthesised reply.
pub fn encode(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32_635;
    // Sign is taken from the sample, then the magnitude is biased and segmented: the
    // segment is the number of halvings until the value fits 8 bits, which is how the
    // curve spends its 4 exponent bits.
    let sign = if sample < 0 { 0x80_u8 } else { 0x00 };
    let magnitude = i32::from(sample).abs().min(CLIP) + BIAS;
    let mut segment = 0_u8;
    let mut remaining = magnitude;
    while remaining > 0xFF {
        remaining >>= 1;
        segment += 1;
    }
    let mantissa = ((magnitude >> (u32::from(segment) + 3)) & 0x0F) as u8;
    !(sign | (segment << 4) | mantissa)
}

/// Whole buffer, little-endian 16-bit out.
pub fn decode_all(input: &[u8]) -> Vec<i16> {
    input.iter().copied().map(decode).collect()
}

/// Whole buffer, packed into the byte-oriented PCM the firmware plays.
pub fn decode_to_pcm(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() * 2);
    for sample in decode_all(input) {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// 16-bit little-endian frames to µ-law, the inverse of [`decode_to_pcm`].
pub fn encode_from_pcm(pcm: &[u8]) -> Vec<u8> {
    pcm.as_chunks::<2>()
        .0
        .iter()
        .map(|pair| encode(i16::from_le_bytes(*pair)))
        .collect()
}

/// Report whether a buffer is silence as far as µ-law is concerned, which is how the
/// stub recogniser tells "nothing arrived" from "audio arrived".
pub fn peak(input: &[u8]) -> i16 {
    input
        .iter()
        .map(|byte| decode(*byte).unsigned_abs() as i16)
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_and_extremes_match_the_firmware_values() {
        // The firmware pins these: 0xFF is silence, and the extremes decode to the
        // documented limits of the companding curve.
        assert_eq!(decode(0xFF), 0);
        assert_eq!(encode(0), 0xFF);
        assert_eq!(decode(0x80), 32_124);
        assert_eq!(decode(0x00), -32_124);
    }

    #[test]
    fn round_trip_is_close_relative_to_the_signal() {
        for sample in [-32_000_i16, -8_000, -100, 0, 100, 8_000, 32_000] {
            let error = (i32::from(decode(encode(sample))) - i32::from(sample)).abs();
            let allowed = (i32::from(sample.abs()) / 8).max(64);
            assert!(error <= allowed, "{sample} -> {error} > {allowed}");
        }
    }

    #[test]
    fn buffers_round_trip_and_report_level() {
        let samples: Vec<i16> = (0..64).map(|i| (i * 900) as i16).collect();
        let mut pcm = Vec::new();
        for sample in &samples {
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        let encoded = encode_from_pcm(&pcm);
        assert_eq!(encoded.len(), samples.len());
        assert!(peak(&encoded) > 1_000);
        assert_eq!(peak(&[0xFF; 32]), 0);
        assert_eq!(decode_to_pcm(&[0xFF, 0xFF]).len(), 4);
    }
}
