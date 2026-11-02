//! Pure PCM/WAV helpers, kept free of ESP-IDF imports for native tests.

pub const SAMPLE_RATE: u32 = 24_000;
pub const CHANNELS: usize = 2;
pub const BITS: u16 = 16;
pub const FRAME_BYTES: usize = CHANNELS * 2;
pub const TONE_HZ: f32 = 440.0;

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub frames: u64,
    pub rms: [u32; 2],
    pub peak: [u32; 2],
    pub correlation: f32,
    pub short_reads: u32,
}

impl Stats {
    pub fn summary(self) -> String {
        format!(
            "frames={} rms_l={} rms_r={} peak_l={} peak_r={} corr_lr={:.3} short_reads={}",
            self.frames,
            self.rms[0],
            self.rms[1],
            self.peak[0],
            self.peak[1],
            self.correlation,
            self.short_reads
        )
    }

    /// Measurement fields without the transport counter, so callers can report
    /// `short_reads` from the capture that produced them.
    pub fn measurement(self) -> String {
        format!(
            "frames={} rms_l={} rms_r={} peak_l={} peak_r={} corr_lr={:.3}",
            self.frames, self.rms[0], self.rms[1], self.peak[0], self.peak[1], self.correlation
        )
    }
}

pub struct Wav<'a> {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub data: &'a [u8],
}

pub fn parse_wav(bytes: &[u8]) -> Result<Wav<'_>, String> {
    parse_wav_at(bytes, SAMPLE_RATE)
}

/// Same parser, for a caller that expects a rate other than the firmware default.
///
/// The voice assistant runs at 16 kHz while everything else here is 24 kHz, so the
/// expected rate has to be a parameter rather than a constant: a WAV that merely
/// has the wrong rate must be rejected by the caller that cares, not silently
/// played at the wrong speed.
pub fn parse_wav_at(bytes: &[u8], expected_rate: u32) -> Result<Wav<'_>, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("ERR WAV is not a RIFF/WAVE file\r\n".into());
    }
    let mut position = 12_usize;
    let mut fmt = None;
    while position + 8 <= bytes.len() {
        let id = &bytes[position..position + 4];
        let size = u32::from_le_bytes(
            bytes[position + 4..position + 8]
                .try_into()
                .expect("4 bytes"),
        ) as usize;
        let start = position + 8;
        let end = start
            .checked_add(size)
            .ok_or_else(|| "ERR WAV chunk length overflow\r\n".to_owned())?;
        if end > bytes.len() {
            return Err("ERR WAV chunk exceeds file length\r\n".into());
        }
        if id == b"fmt " {
            if size < 16 {
                return Err("ERR WAV fmt chunk too short\r\n".into());
            }
            let format = u16::from_le_bytes([bytes[start], bytes[start + 1]]);
            let channels = u16::from_le_bytes([bytes[start + 2], bytes[start + 3]]);
            let sample_rate =
                u32::from_le_bytes(bytes[start + 4..start + 8].try_into().expect("4 bytes"));
            let bits = u16::from_le_bytes([bytes[start + 14], bytes[start + 15]]);
            if format != 1 {
                return Err(format!(
                    "ERR WAV format={format}, only PCM is supported\r\n"
                ));
            }
            if channels != 1 && channels != 2 {
                return Err(format!(
                    "ERR WAV channels={channels}, only mono/stereo is supported\r\n"
                ));
            }
            if bits != BITS {
                return Err(format!("ERR WAV bits={bits}, only 16-bit is supported\r\n"));
            }
            if sample_rate != expected_rate {
                return Err(format!(
                    "ERR WAV rate={sample_rate}, expected {expected_rate} Hz\r\n"
                ));
            }
            fmt = Some((sample_rate, channels, bits));
        } else if id == b"data" {
            let Some((sample_rate, channels, bits)) = fmt else {
                return Err("ERR WAV data precedes fmt chunk\r\n".into());
            };
            if !size.is_multiple_of(usize::from(channels) * 2) {
                return Err("ERR WAV data length is not whole frames\r\n".into());
            }
            return Ok(Wav {
                sample_rate,
                channels,
                bits,
                data: &bytes[start..end],
            });
        }
        position = end + (size & 1);
    }
    Err("ERR WAV has no data chunk\r\n".into())
}

pub fn wav_header(data_len: usize, sample_rate: u32, channels: u16) -> Result<[u8; 44], String> {
    let data_len = u32::try_from(data_len).map_err(|_| "ERR WAV data too large\r\n".to_owned())?;
    let channels_u32 = u32::from(channels);
    let block_align = u16::try_from(channels_u32 * 2)
        .map_err(|_| "ERR WAV block alignment overflow\r\n".to_owned())?;
    let byte_rate = sample_rate
        .checked_mul(channels_u32)
        .and_then(|value| value.checked_mul(2))
        .ok_or_else(|| "ERR WAV byte rate overflow\r\n".to_owned())?;
    let mut header = [0_u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(36_u32 + data_len).to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16_u32.to_le_bytes());
    header[20..22].copy_from_slice(&1_u16.to_le_bytes());
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&BITS.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_len.to_le_bytes());
    Ok(header)
}

/// Levels of a captured buffer: RMS, peak and inter-channel correlation.
///
/// Everything the console reports about a recording comes from here, so the
/// numbers have to be comparable between captures of different lengths.
pub fn stats(data: &[u8], channels: u16) -> Stats {
    let channels = usize::from(channels);
    let mut result = Stats::default();
    if channels == 0 || !data.len().is_multiple_of(channels * 2) {
        return result;
    }
    result.frames = (data.len() / (channels * 2)) as u64;
    // Integer accumulators, deliberately. This chip has no hardware double, so every
    // `f64` multiply or add here is a software call, and this loop touches every
    // captured sample: in `f64` a five second capture spent 880 ms measuring itself.
    // Sums of squares of ±32768 over ten seconds still fit in an `i64` with room to
    // spare, and integers are exact where the running float sum was not.
    let paired = channels >= 2;
    let mut sum_squares = [0_i64; 2];
    let mut sums = [0_i64; 2];
    let mut cross = 0_i64;
    for frame in data.chunks_exact(channels * 2) {
        let left = i64::from(i16::from_le_bytes([frame[0], frame[1]]));
        sum_squares[0] += left * left;
        sums[0] += left;
        let magnitude = left.unsigned_abs().min(u64::from(u32::MAX)) as u32;
        result.peak[0] = result.peak[0].max(magnitude);
        if paired {
            let right = i64::from(i16::from_le_bytes([frame[2], frame[3]]));
            sum_squares[1] += right * right;
            sums[1] += right;
            let magnitude = right.unsigned_abs().min(u64::from(u32::MAX)) as u32;
            result.peak[1] = result.peak[1].max(magnitude);
            cross += left * right;
        }
    }
    if result.frames > 0 {
        let frames = result.frames as f64;
        result.rms[0] = (sum_squares[0] as f64 / frames).sqrt() as u32;
        if paired {
            result.rms[1] = (sum_squares[1] as f64 / frames).sqrt() as u32;
            // Centred moments, normalised by the frame count: the correlation is a
            // ratio, so the constant factor cancels.
            let mean_l = sums[0] as f64 / frames;
            let mean_r = sums[1] as f64 / frames;
            let centered_l = sum_squares[0] as f64 / frames - mean_l * mean_l;
            let centered_r = sum_squares[1] as f64 / frames - mean_r * mean_r;
            let centered_cross = cross as f64 / frames - mean_l * mean_r;
            if centered_l > 0.0 && centered_r > 0.0 {
                result.correlation =
                    (centered_cross / (centered_l * centered_r).sqrt()).clamp(-1.0, 1.0) as f32;
            }
        }
    }
    result
}

/// Taps on each side of the interpolation point. Six is the smallest Lanczos width
/// that keeps the first image of a full-scale 1 kHz tone below -45 dB, which is far
/// beneath the ES7210 noise floor; widening it costs a multiply per tap for every
/// output sample the 240 MHz core has to push.
const RESAMPLE_TAPS: i32 = 6;

/// Distinct fractional sample positions the resampler caches weights for.
const RESAMPLE_PHASES: usize = 1024;

/// Lanczos kernel: `sinc(x)` tapered by `sinc(x / TAPS)`, zero at `±TAPS`.
fn lanczos(x: f32) -> f32 {
    if x == 0.0 {
        return 1.0;
    }
    let taps = RESAMPLE_TAPS as f32;
    if x.abs() >= taps {
        return 0.0;
    }
    let scaled = core::f32::consts::PI * x;
    let a = scaled.sin() / scaled;
    let b = (scaled / taps).sin() / (scaled / taps);
    a * b
}

/// Greatest common divisor, used to keep the resampling ratio as small integers.
fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let next = left % right;
        left = right;
        right = next;
    }
    left.max(1)
}

/// Convert mono samples from one rate to another.
///
/// This is how the assistant meets the bridge: the codec runs at 24 kHz because that
/// is what the WAV recorder, the cue beeps and the tone diagnostics were verified at,
/// while speech travelling to and from the LAN bridge is 16 kHz. Converting here keeps
/// the I²S and codec configuration - the part that needed schematic reading and a
/// scope of register writes to get right - out of the change.
///
/// Downsampling stretches the kernel by the step size, which turns it into the
/// low-pass that has to come before the sampler drops samples; without that stretch a
/// 10 kHz component would fold down to 6 kHz as a whistle that is not in the recording.
/// Upsampling leaves the kernel alone, so nothing above the source Nyquist is
/// invented. Taps are renormalised per output sample, so a non-integer ratio does not
/// ripple the DC level, and samples past either edge simply do not contribute.
pub fn resample(input: &[i16], from: u32, to: u32) -> Result<Vec<i16>, String> {
    if from == 0 || to == 0 || from > 96_000 || to > 96_000 {
        return Err(format!(
            "ERR resample rates {from}->{to} are not usable\r\n"
        ));
    }
    if from == to || input.is_empty() {
        return Ok(input.to_vec());
    }
    // Every position below is integer arithmetic, deliberately. This chip emulates
    // `f64` in software, and one double per output sample - the position, its floor,
    // the fraction, the phase - cost 700 ms for five seconds of speech, against 3 ms
    // of actual filtering. The ratio is rational, so carrying the phase as a
    // remainder over a reduced denominator is not an approximation of the float
    // position, it is the exact same number.
    let divisor = gcd(u64::from(from), u64::from(to));
    let numerator = u64::from(from) / divisor;
    let denominator = u64::from(to) / divisor;
    // Nearest whole number of output samples: 1 s at 24 kHz is 24000 samples in and
    // exactly 16000 out, so a second of speech neither grows nor shrinks.
    let length = ((input.len() as u64 * denominator + numerator / 2) / numerator).max(1) as usize;
    let stretch = if numerator > denominator {
        numerator as f32 / denominator as f32
    } else {
        1.0
    };
    let last = input.len() as isize - 1;
    let low = -RESAMPLE_TAPS + 1;
    // Weights are computed once per distinct fractional position, not once per sample.
    // The kernel needs a `sin` per tap, and a `sin` is software on this chip: computing
    // them per sample made a 3 s answer spend seconds before the first byte of playback.
    // For a rational ratio there are only a handful of distinct positions - 24k to 16k
    // alternates between a half and a whole sample - so the table stays tiny. Bucket
    // width is a thousandth of a sample, far below anything a 16 kHz path can express.
    let width = (2 * RESAMPLE_TAPS) as usize;
    let mut rows = vec![f32::NAN; RESAMPLE_PHASES * width];
    let mut sums = vec![0.0_f32; RESAMPLE_PHASES];
    let mut output = Vec::with_capacity(length);
    let mut base = 0_isize;
    let mut remainder = 0_u64;
    for _ in 0..length {
        let fraction = remainder as f32 / denominator as f32;
        let phase = ((remainder * RESAMPLE_PHASES as u64) / denominator) as usize;
        let first = base + low as isize;
        let sample = if first >= 0 && base + RESAMPLE_TAPS as isize <= last {
            let row = phase * width;
            if rows[row].is_nan() {
                let mut sum = 0.0_f32;
                for (offset, tap) in (low..=RESAMPLE_TAPS).enumerate() {
                    let weight = lanczos((tap as f32 - fraction) / stretch);
                    rows[row + offset] = weight;
                    sum += weight;
                }
                sums[phase] = sum;
            }
            let window = &input[first as usize..first as usize + width];
            let mut total = 0.0_f32;
            for (value, weight) in window.iter().zip(&rows[row..row + width]) {
                total += f32::from(*value) * weight;
            }
            let sum = sums[phase];
            if sum == 0.0 { 0.0 } else { total / sum }
        } else {
            // At either edge some taps fall outside the buffer, so the weights have to
            // be renormalised for this sample alone. Dying speech is worth a few
            // multiplications: correctness matters more than the last microsecond.
            let mut total = 0.0_f32;
            let mut weight_sum = 0.0_f32;
            for tap in low..=RESAMPLE_TAPS {
                let source = base + tap as isize;
                if source < 0 || source > last {
                    continue;
                }
                let weight = lanczos((tap as f32 - fraction) / stretch);
                if weight == 0.0 {
                    continue;
                }
                total += f32::from(input[source as usize]) * weight;
                weight_sum += weight;
            }
            if weight_sum == 0.0 {
                0.0
            } else {
                total / weight_sum
            }
        };
        output.push(sample.round().clamp(-32768.0, 32767.0) as i16);
        // Advance by one source-frame fraction; `while` rather than `if` because an
        // upsampling ratio crosses several source frames in a single output step.
        remainder += numerator;
        while remainder >= denominator {
            remainder -= denominator;
            base += 1;
        }
    }
    Ok(output)
}

/// Average the channels of interleaved frames down to one stream.
///
/// Both onboard microphones sit a few millimetres apart and hear the same room, so
/// averaging keeps the signal and drops uncorrelated noise by 3 dB. Summing in `i32`
/// means two full-scale samples cannot overflow before the divide.
pub fn to_mono(data: &[u8], channels: usize) -> Result<Vec<i16>, String> {
    if channels == 0 || !data.len().is_multiple_of(channels * 2) {
        return Err(format!(
            "ERR pcm length {} is not whole {channels}-channel frames\r\n",
            data.len()
        ));
    }
    Ok(data
        .chunks_exact(channels * 2)
        .map(|frame| {
            let sum: i32 = (0..channels)
                .map(|channel| {
                    i32::from(i16::from_le_bytes([
                        frame[channel * 2],
                        frame[channel * 2 + 1],
                    ]))
                })
                .sum();
            (sum / channels as i32) as i16
        })
        .collect())
}

/// Interleave one stream into `channels` identical channels as little-endian bytes.
pub fn to_frames(samples: &[i16], channels: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * channels * 2);
    for &sample in samples {
        let le = sample.to_le_bytes();
        for _ in 0..channels {
            bytes.extend_from_slice(&le);
        }
    }
    bytes
}

/// Amplitude in LSBs of one frequency component per channel, using the
/// Goertzel algorithm. This detects the test tone in a capture even when the
/// broadband noise floor is louder than the tone.
pub fn tone_amplitudes(data: &[u8], channels: u16, freq: f32, sample_rate: u32) -> [u32; 2] {
    let mut amplitudes = [0_u32; 2];
    let channels = usize::from(channels).max(1);
    if channels > 2 || !data.len().is_multiple_of(channels * 2) {
        return amplitudes;
    }
    let frames = data.len() / (channels * 2);
    if frames == 0 || sample_rate == 0 {
        return amplitudes;
    }
    let omega = 2.0 * std::f64::consts::PI * f64::from(freq) / f64::from(sample_rate);
    let coeff = 2.0 * omega.cos();
    for (channel, amplitude) in amplitudes.iter_mut().enumerate() {
        if channel >= channels {
            break;
        }
        let mut previous = 0.0_f64;
        let mut before = 0.0_f64;
        for frame in data.chunks_exact(channels * 2) {
            let sample = f64::from(i16::from_le_bytes([
                frame[channel * 2],
                frame[channel * 2 + 1],
            ]));
            let current = sample + coeff * previous - before;
            before = previous;
            previous = current;
        }
        let power = previous * previous + before * before - coeff * previous * before;
        let magnitude = 2.0 * power.max(0.0).sqrt() / frames as f64;
        *amplitude = magnitude.round().min(f64::from(u32::MAX)) as u32;
    }
    amplitudes
}

/// Coarse "was anything said" test, run before paying for a round trip.
///
/// This is deliberately not voice-activity detection: it asks only whether the
/// capture carries more energy than an idle room. Measured on the board at 30 dB
/// microphone gain, a take where nothing was said came back as `peak=220 rms=21`,
/// while the same speaker a metre away produced `peak=1711..3158 rms=157..247`.
/// Both conditions are required: a click spikes the peak without speech, and
/// mains hum lifts the average without any transient in it.
pub const SPEECH_MIN_PEAK: u32 = 300;
pub const SPEECH_MIN_RMS: u32 = 60;

pub fn heard_speech(measured: &Stats) -> bool {
    measured.peak[0].max(measured.peak[1]) >= SPEECH_MIN_PEAK
        && measured.rms[0].max(measured.rms[1]) >= SPEECH_MIN_RMS
}

/// Scale `data` in place so its loudest sample reaches `target_db` below full
/// scale, never applying more than `max_gain_db`. Quiet recordings are the norm
/// on this board: the analog microphones deliver around -50 dBFS for speech, so
/// the ES8311 would otherwise have to run above unity and clip a hotter file.
/// Returns the gain actually applied, in dB.
pub fn normalize(data: &mut [u8], target_db: f32, max_gain_db: f32) -> f32 {
    let peak = data
        .chunks_exact(2)
        .map(|sample| i32::from(i16::from_le_bytes([sample[0], sample[1]])))
        .map(i32::abs)
        .max()
        .unwrap_or(0);
    if peak == 0 {
        return 0.0;
    }
    let target = 32767.0 * 10_f32.powf(target_db / 20.0);
    let gain = (target / peak as f32).min(10_f32.powf(max_gain_db / 20.0));
    if gain <= 1.0 {
        return 0.0;
    }
    for sample in data.chunks_exact_mut(2) {
        let value = f32::from(i16::from_le_bytes([sample[0], sample[1]])) * gain;
        let scaled = value.clamp(-32768.0, 32767.0) as i16;
        sample.copy_from_slice(&scaled.to_le_bytes());
    }
    // The caller only needs a human-readable figure, so round to 0.1 dB.
    (200.0 * gain.log10()).round() / 10.0
}

/// G.711 µ-law companding, as used by the Google speech endpoints.
///
/// Both the upload and the download side speak µ-law rather than raw PCM: it
/// halves the bytes on the wire for the same 8 kHz-of-bandwidth voice, which on
/// a batch-recognition upload is the difference between a 160 KB and a 320 KB
/// request body. The law is logarithmic, so the error is proportional to the
/// signal instead of absolute - -44 dB signal-to-noise across the range, which is
/// far below anything the ES7210 or the network contributes.
const ULAW_BIAS: i32 = 0x84;
const ULAW_CLIP: i32 = 32_635;

/// Compress one sample to its µ-law byte.
pub fn ulaw_encode(sample: i16) -> u8 {
    let sign = if sample < 0 { 0x80_u8 } else { 0x00 };
    let magnitude = (i32::from(sample).abs()).min(ULAW_CLIP) + ULAW_BIAS;
    // Find the segment: the highest set bit tells how coarse the step is.
    let mut segment = 0_u8;
    let mut remaining = magnitude;
    while remaining > 0xFF {
        remaining >>= 1;
        segment += 1;
    }
    let mantissa = ((magnitude >> (u32::from(segment) + 3)) & 0x0F) as u8;
    !(sign | (segment << 4) | mantissa)
}

/// Expand one µ-law byte back to a sample, the inverse of [`ulaw_encode`].
pub fn ulaw_decode(encoded: u8) -> i16 {
    let value = !encoded;
    let magnitude = (((value & 0x0F) as i32) << 3) + ULAW_BIAS;
    let magnitude = magnitude << ((value & 0x70) >> 4);
    let sample = if value & 0x80 != 0 {
        ULAW_BIAS - magnitude
    } else {
        magnitude - ULAW_BIAS
    };
    sample.clamp(-32_768, 32_767) as i16
}

/// Compress a whole byte buffer, converting little-endian `i16` samples.
///
/// Fails on an odd length instead of dropping the trailing byte: a silently lost
/// half sample would surface as a click at the end of a recognised utterance.
pub fn ulaw_encode_all(pcm: &[u8]) -> Result<Vec<u8>, String> {
    if !pcm.len().is_multiple_of(2) {
        let len = pcm.len();
        return Err(format!(
            "ERR ulaw: {len} bytes is not a whole number of samples\r\n"
        ));
    }
    Ok(pcm
        .chunks_exact(2)
        .map(|pair| ulaw_encode(i16::from_le_bytes([pair[0], pair[1]])))
        .collect())
}

/// Expand a µ-law buffer into little-endian `i16` samples.
pub fn ulaw_decode_all(encoded: &[u8]) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(encoded.len() * 2);
    for byte in encoded {
        pcm.extend_from_slice(&ulaw_decode(*byte).to_le_bytes());
    }
    pcm
}

pub fn es8311_volume_reg(db: f32) -> u8 {
    // ES8311 DAC volume map: 0x00 = -95.5 dB, 0xFF = +32.0 dB.
    let db = db.clamp(-95.5, 32.0);
    ((db + 95.5) * (255.0 / 127.5)) as u8
}

pub fn es7210_gain_reg(db: f32) -> u8 {
    // Vendor table: 0 dB, then 3 dB steps to 33 dB, then 34.5/36/37.5.
    let db = db + 1.0;
    if db < 33.0 {
        if db < 3.0 { 0 } else { (db / 3.0) as u8 }
    } else if db < 34.5 {
        10
    } else if db < 36.0 {
        12
    } else if db < 37.0 {
        13
    } else {
        14
    }
}

pub fn tone_pcm(ms: u32) -> Vec<u8> {
    tone_at(TONE_HZ, ms, 0.40)
}

/// A short 880 Hz burst with fades, used to stretch a beep into a cue.
fn tone_at(freq: f32, ms: u32, gain: f32) -> Vec<u8> {
    let frames = (u64::from(SAMPLE_RATE) * u64::from(ms) / 1_000) as usize;
    let fade = (SAMPLE_RATE as usize * 20 / 1_000).max(1);
    let amplitude = f32::from(i16::MAX) * gain;
    let mut pcm = Vec::with_capacity(frames * FRAME_BYTES);
    for frame in 0..frames {
        let envelope = if frame < fade {
            frame as f32 / fade as f32
        } else if frame + fade >= frames {
            (frames - frame) as f32 / fade as f32
        } else {
            1.0
        };
        let phase = std::f32::consts::TAU * freq * frame as f32 / SAMPLE_RATE as f32;
        let sample = (phase.sin() * amplitude * envelope) as i16;
        pcm.extend_from_slice(&sample.to_le_bytes());
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    pcm
}

fn silence_pcm(ms: u32) -> Vec<u8> {
    vec![0; (u64::from(SAMPLE_RATE) * u64::from(ms) / 1_000) as usize * FRAME_BYTES]
}

/// The record cue: two 880 Hz beeps, so "speak after the second beep" is a rule
/// the user can follow without a screen.
pub fn cue_pcm() -> Vec<u8> {
    let mut cue = tone_at(880.0, CUES[0].0, CUES[0].1);
    cue.extend_from_slice(&silence_pcm(CUE_GAP_MS));
    cue.extend_from_slice(&tone_at(880.0, CUES[1].0, CUES[1].1));
    cue
}

/// Short first beep, slightly longer and louder second beep.
const CUES: [(u32, f32); 2] = [(120, 0.35), (200, 0.45)];
const CUE_GAP_MS: u32 = 110;

pub fn cue_len_ms() -> u32 {
    CUES[0].0 + CUE_GAP_MS + CUES[1].0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn peak_of(data: &[u8]) -> i32 {
        data.chunks_exact(2)
            .map(|s| i32::from(i16::from_le_bytes([s[0], s[1]])))
            .map(i32::abs)
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn speech_gate_separates_an_idle_room_from_speech() {
        // The two shapes measured on hardware, plus the two ways a gate that
        // only looked at one number would be fooled.
        let idle = Stats {
            peak: [220, 199],
            rms: [21, 21],
            ..Stats::default()
        };
        assert!(!heard_speech(&idle), "silence must not be uploaded");
        let speech = Stats {
            peak: [1711, 1632],
            rms: [192, 174],
            ..Stats::default()
        };
        assert!(heard_speech(&speech), "speech must pass");
        let distant = Stats {
            peak: [359, 340],
            rms: [62, 58],
            ..Stats::default()
        };
        assert!(heard_speech(&distant), "speech across the room must pass");
        let click = Stats {
            peak: [4000, 3900],
            rms: [12, 11],
            ..Stats::default()
        };
        assert!(!heard_speech(&click), "a click is not speech");
        let hum = Stats {
            peak: [280, 270],
            rms: [200, 190],
            ..Stats::default()
        };
        assert!(!heard_speech(&hum), "hum without transients is not speech");
    }

    #[test]
    fn normalize_lifts_quiet_capture_to_the_target() {
        // Speech sits near -50 dBFS, which is what made playback sound low.
        let mut data = samples(&[1000, -1000, 500, -500]);
        let gain = normalize(&mut data, -6.0, 18.0);
        assert!((gain - 18.0).abs() < 0.1, "gain {gain}");
        // 18 dB is 7.94x, so the capped 1000-count peak lands at 7943.
        assert_eq!(peak_of(&data), 7943);
    }

    #[test]
    fn normalize_never_attenuates_or_clips() {
        let mut loud = samples(&[30000, -32000]);
        assert_eq!(normalize(&mut loud, -6.0, 18.0), 0.0);
        assert_eq!(peak_of(&loud), 32000);

        // The max gain cap keeps a very quiet file from wrapping.
        let mut tiny = samples(&[-32768, 100]);
        assert_eq!(normalize(&mut tiny, -6.0, 0.0), 0.0);
        assert_eq!(peak_of(&tiny), 32768);
    }

    #[test]
    fn normalize_leaves_silence_alone() {
        let mut silent = samples(&[0, 0, 0, 0]);
        assert_eq!(normalize(&mut silent, -6.0, 18.0), 0.0);
        assert_eq!(peak_of(&silent), 0);
    }

    #[test]
    fn volume_and_gain_maps() {
        assert_eq!(es8311_volume_reg(-95.5), 0);
        assert_eq!(es8311_volume_reg(32.0), 255);
        assert_eq!(es8311_volume_reg(-24.0), 143);
        assert_eq!(es7210_gain_reg(0.0), 0);
        assert_eq!(es7210_gain_reg(24.0), 8);
        assert_eq!(es7210_gain_reg(30.0), 10);
        assert_eq!(es7210_gain_reg(37.5), 14);
    }

    #[test]
    fn wav_round_trip_and_stereo_stats() {
        let samples = [1000_i16, -1000, 2000, -2000];
        let mut data = Vec::new();
        for sample in samples {
            data.extend_from_slice(&sample.to_le_bytes());
        }
        let header = wav_header(data.len(), SAMPLE_RATE, 2).unwrap();
        let mut file = header.to_vec();
        file.extend_from_slice(&data);
        let wav = parse_wav(&file).unwrap();
        assert_eq!(wav.sample_rate, SAMPLE_RATE);
        assert_eq!(wav.channels, 2);
        assert_eq!(wav.bits, 16);
        assert_eq!(wav.data, data);

        let measured = stats(&data, 2);
        assert_eq!(measured.frames, 2);
        assert_eq!(measured.rms, [1581, 1581]);
        assert_eq!(measured.peak, [2000, 2000]);
        assert!((measured.correlation + 1.0).abs() < 0.001);
    }

    #[test]
    fn wav_rejects_wrong_rate_and_truncation() {
        let header = wav_header(0, SAMPLE_RATE, 1).unwrap();
        let mut file = header.to_vec();
        file[24..28].copy_from_slice(&16_000_u32.to_le_bytes());
        assert!(parse_wav(&file).is_err());
        let mut truncated = header.to_vec();
        truncated[40..44].copy_from_slice(&4_u32.to_le_bytes());
        assert!(parse_wav(&truncated).is_err());
    }

    #[test]
    fn summary_reports_transport_counter_separately() {
        let mut measured = stats(&[0_u8; 8], 2);
        measured.short_reads = 3;
        assert!(measured.summary().ends_with("short_reads=3"));
        assert!(!measured.measurement().contains("short_reads"));
    }

    #[test]
    fn tone_amplitude_finds_only_the_played_frequency() {
        let frames = 4_800_usize;
        let mut tone = Vec::with_capacity(frames * FRAME_BYTES);
        let mut silence = Vec::with_capacity(frames * FRAME_BYTES);
        for frame in 0..frames {
            let phase = 2.0 * std::f64::consts::PI * f64::from(TONE_HZ) * frame as f64
                / f64::from(SAMPLE_RATE);
            let sample = (phase.sin() * 1000.0).round() as i16;
            for value in [sample, 0_i16] {
                tone.extend_from_slice(&value.to_le_bytes());
            }
            for value in [0_i16, 0_i16] {
                silence.extend_from_slice(&value.to_le_bytes());
            }
        }
        let found = tone_amplitudes(&tone, 2, TONE_HZ, SAMPLE_RATE);
        assert!((found[0] as i64 - 1000).abs() < 5, "left tone {}", found[0]);
        assert_eq!(found[1], 0);
        assert_eq!(tone_amplitudes(&silence, 2, TONE_HZ, SAMPLE_RATE), [0, 0]);
        let other = tone_amplitudes(&tone, 2, 1_000.0, SAMPLE_RATE);
        assert!(other[0] < 100, "1000 Hz leaked {}", other[0]);
    }

    #[test]
    fn tone_is_stereo_and_faded() {
        let pcm = tone_pcm(100);
        assert_eq!(pcm.len(), 2400 * FRAME_BYTES);
        assert_eq!(i16::from_le_bytes([pcm[0], pcm[1]]), 0);
        let last = pcm.len() - 4;
        assert!(i16::from_le_bytes([pcm[last], pcm[last + 1]]).abs() < 100);
    }

    #[test]
    fn cue_is_two_beeps_around_a_silent_gap() {
        let cue = cue_pcm();
        let expected = (u64::from(SAMPLE_RATE) * u64::from(cue_len_ms()) / 1_000) as usize;
        assert_eq!(cue.len(), expected * FRAME_BYTES);

        let frame = |index: usize| i16::from_le_bytes([cue[index * 4], cue[index * 4 + 1]]);
        // First beep is present, the gap is silent, the second beep follows.
        let first_beep_peak = (0..120 * 24).map(frame).map(i16::abs).max().unwrap_or(0);
        assert!(first_beep_peak > 8_000, "first beep peak {first_beep_peak}");
        let gap_start = 120 * 24 + 30 * 24;
        let gap_peak = (gap_start..gap_start + 40 * 24)
            .map(frame)
            .map(i16::abs)
            .max()
            .unwrap_or(0);
        assert_eq!(gap_peak, 0, "gap should be silent");
        let second_start = (cue_len_ms() - 60) as usize * 24;
        let second_beep_peak = (second_start..second_start + 40 * 24)
            .map(frame)
            .map(i16::abs)
            .max()
            .unwrap_or(0);
        assert!(
            second_beep_peak > 8_000,
            "second beep peak {second_beep_peak}"
        );
    }

    #[test]
    fn ulaw_maps_silence_and_signs() {
        // Silence lands on the all-ones byte, which is what "no signal" looks
        // like in every µ-law dump.
        assert_eq!(ulaw_encode(0), 0xFF);
        assert_eq!(ulaw_decode(0xFF), 0);
        // Sign is preserved through the lossy step.
        assert!(ulaw_decode(ulaw_encode(20_000)) > 0);
        assert!(ulaw_decode(ulaw_encode(-20_000)) < 0);
    }

    #[test]
    fn ulaw_error_stays_relative_to_signal() {
        // A logarithmic law must be accurate near silence and only relatively
        // accurate when loud; an absolute bound would be wrong at both ends.
        let mut worst_quiet: i32 = 0;
        for sample in -40..=40_i16 {
            let error = (i32::from(ulaw_decode(ulaw_encode(sample))) - i32::from(sample)).abs();
            worst_quiet = worst_quiet.max(error);
            assert!(error <= 8, "sample {sample} error {error}");
        }
        for sample in i16::MIN..=i16::MAX {
            let error = (i32::from(ulaw_decode(ulaw_encode(sample))) - i32::from(sample)).abs();
            let bound = i32::from(sample).abs() / 8 + 16;
            assert!(
                error <= bound,
                "sample {sample} error {error} bound {bound}"
            );
        }
        assert!(worst_quiet >= 0, "quiet band checked");
    }

    #[test]
    fn ulaw_round_trip_is_idempotent_and_monotonic() {
        let mut previous = i16::MIN;
        for sample in i16::MIN..=i16::MAX {
            let once = ulaw_encode(sample);
            let twice = ulaw_encode(ulaw_decode(once));
            // The law has two zero codes: 0x7F and 0xFF both expand to silence, and
            // only 0xFF encodes back to 0xFF. That single asymmetry is inherent, so
            // every value is stable except the negative zero.
            if once == 0x7F {
                assert_eq!(twice, 0xFF, "negative zero canonicalises");
            } else {
                assert_eq!(twice, once, "sample {sample}");
            }
            let decoded = ulaw_decode(once);
            assert!(
                decoded >= previous,
                "decode must not decrease: {sample} gave {decoded} after {previous}"
            );
            previous = decoded;
        }
    }

    #[test]
    fn ulaw_buffer_helpers_round_trip() {
        let pcm: Vec<u8> = [0_i16, 1_000, -1_000, 30_000, -30_000]
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let encoded = ulaw_encode_all(&pcm).expect("even length");
        assert_eq!(encoded.len(), pcm.len() / 2, "two bytes become one");
        let decoded = ulaw_decode_all(&encoded);
        assert_eq!(decoded.len(), pcm.len());
        // Every recovered sample is within the law's step of the original.
        for (before, after) in pcm.chunks_exact(2).zip(decoded.chunks_exact(2)) {
            let original = i16::from_le_bytes([before[0], before[1]]);
            let recovered = i16::from_le_bytes([after[0], after[1]]);
            let error = (i32::from(recovered) - i32::from(original)).abs();
            assert!(
                error <= i32::from(original).abs() / 8 + 16,
                "{original} -> {recovered}"
            );
        }
        assert!(ulaw_encode_all(&[0_u8]).is_err(), "odd length must fail");
    }

    /// One mono sine wave, the material every resampler test is made of.
    fn sine(freq: f32, rate: u32, ms: u32, amplitude: f32) -> Vec<i16> {
        let frames = (u64::from(rate) * u64::from(ms) / 1_000) as usize;
        (0..frames)
            .map(|frame| {
                let phase =
                    2.0 * std::f64::consts::PI * f64::from(freq) * frame as f64 / f64::from(rate);
                (phase.sin() * f64::from(amplitude)).round() as i16
            })
            .collect()
    }

    fn mono_amplitude(samples: &[i16], rate: u32, freq: f32) -> u32 {
        tone_amplitudes(&to_frames(samples, 1), 1, freq, rate)[0]
    }

    #[test]
    fn resample_keeps_durations_and_passes_equal_rates_through() {
        let second = sine(440.0, SAMPLE_RATE, 1_000, 8_000.0);
        let down = resample(&second, 24_000, 16_000).unwrap();
        assert_eq!(down.len(), 16_000, "one second stays one second");
        let up = resample(&down, 16_000, 24_000).unwrap();
        assert_eq!(up.len(), 24_000);
        assert_eq!(resample(&second, 24_000, 24_000).unwrap(), second);
        assert!(resample(&[], 24_000, 16_000).unwrap().is_empty());
        // A three-sample input is one 8 kHz sample, not zero samples.
        assert_eq!(resample(&[7, 7, 7], 24_000, 8_000).unwrap().len(), 1);
        assert!(resample(&second, 0, 16_000).is_err());
        assert!(resample(&second, 24_000, 0).is_err());
    }

    #[test]
    fn resample_moves_the_rate_without_moving_the_pitch() {
        let tone = sine(440.0, 24_000, 500, 10_000.0);
        let down = resample(&tone, 24_000, 16_000).unwrap();
        // The 440 Hz component has to survive at 16 kHz with its amplitude intact;
        // scaling the timeline instead of the rate would move it to 293 Hz.
        let amplitude = mono_amplitude(&down, 16_000, 440.0);
        assert!(
            (8_500..=11_500).contains(&amplitude),
            "amplitude {amplitude}"
        );
        // The pitch has not moved: nothing at all should appear at 440 × 2/3.
        assert!(mono_amplitude(&down, 16_000, 293.3) < 500);
        let up = resample(&sine(440.0, 16_000, 500, 10_000.0), 16_000, 24_000).unwrap();
        let amplitude = mono_amplitude(&up, 24_000, 440.0);
        assert!(
            (8_500..=11_500).contains(&amplitude),
            "amplitude {amplitude}"
        );
    }

    #[test]
    fn resample_keeps_a_flat_level_flat() {
        // Renormalisation is what stops a ratio that is not a whole number from
        // rippling the level: every output sample divides by its own weight sum, so a
        // constant input has to come back constant. A cached weight row is only correct
        // if its sum is cached with it.
        for (from, to) in [(24_000, 16_000), (16_000, 24_000)] {
            let flat = vec![10_000_i16; 4_800];
            let converted = resample(&flat, from, to).unwrap();
            assert_eq!(converted.len(), 4_800 * to as usize / from as usize);
            let min = *converted.iter().min().unwrap();
            let max = *converted.iter().max().unwrap();
            assert!(
                (9_999..=10_001).contains(&min) && (9_999..=10_001).contains(&max),
                "{from}->{to} rippled {min}..{max}"
            );
        }
    }

    #[test]
    fn resample_filters_out_content_above_the_new_nyquist() {
        // 10 kHz is inside 24 kHz but above the 8 kHz Nyquist of 16 kHz, so it folds
        // to 6 kHz unless the kernel is stretched to low-pass first.
        let tone = sine(10_000.0, 24_000, 500, 12_000.0);
        let down = resample(&tone, 24_000, 16_000).unwrap();
        let before = f64::from(stats(&to_frames(&tone, 1), 1).rms[0]);
        let after = f64::from(stats(&to_frames(&down, 1), 1).rms[0]);
        assert!(
            after < before * 0.25,
            "aliased energy should be suppressed: before {before:.0} after {after:.0}"
        );
    }

    #[test]
    fn mono_downmix_averages_and_counts_whole_frames() {
        let stereo: Vec<u8> = [100_i16, -300, 1_000, 2_000, -7, -7]
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        assert_eq!(to_mono(&stereo, 2).unwrap(), vec![-100, 1_500, -7]);
        assert!(to_mono(&stereo[..4], 3).is_err());
        assert!(to_mono(&stereo, 0).is_err());
        assert_eq!(
            to_frames(&[-2, 3], 2),
            vec![0xfe, 0xff, 0xfe, 0xff, 3, 0, 3, 0]
        );
    }
}
