//! ES8311 speaker codec and ES7210 microphone ADC on the shared I2C bus.
//!
//! Register sequences are transcribed from the vendor `esp_codec_dev` drivers
//! shipped with the Arduino audio example. Only the fixed 24 kHz / 16-bit
//! stereo standard-I2S path is exposed. The speaker PA is enabled only while
//! playing, at a deliberately conservative digital volume.

use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use esp_idf_svc::hal::{
    delay::TickType,
    gpio::{Gpio7, Gpio14, Gpio15, Gpio16, Gpio17, Gpio18, Output, PinDriver},
    i2c::I2cDriver,
    i2s::{
        I2S0, I2sBiDir, I2sDriver,
        config::{
            Config as I2sConfig, DataBitWidth, SlotMode, StdClkConfig, StdConfig, StdGpioConfig,
            StdSlotConfig,
        },
    },
};
use esp_idf_svc::sys::TickType_t;

use crate::pcm::{
    BITS, CHANNELS, FRAME_BYTES, SAMPLE_RATE, Stats, TONE_HZ, cue_len_ms, cue_pcm, es7210_gain_reg,
    es8311_volume_reg, heard_speech, normalize, parse_wav, stats, to_frames, tone_amplitudes,
    tone_pcm, wav_header,
};

const MCLK_MULTIPLE: u32 = 256;
const CAPTURE_CHUNK: usize = 4096;
/// One monitor read. The console loop wakes every 20 ms, which at 24 kHz stereo
/// is 1 920 bytes, so this leaves a 4x margin for a late loop iteration.
const MONITOR_CHUNK: usize = 8192;
/// Upper bound on what one monitor tick will drain. The I²S ring is a handful of
/// 10 ms DMA buffers, so anything approaching this is already stale; the cap only
/// keeps a pathological backlog from holding the console loop for seconds.
const MONITOR_DRAIN_CAP: usize = 65_536;
const PLAY_CHUNK: usize = 4096;
/// Matches the I2S DMA frame count, so a duplex step is one DMA buffer long.
const DUPLEX_FRAMES: usize = 240;
/// The microphone inputs are AC coupled, so starting the ADC re-biases them
/// with a slow transient that swamps real sound. Discard this much first.
const SETTLE_FRAMES: usize = 24_000;
// Frames dropped after the cue so the beep itself is not part of the utterance.
// One DMA ring's worth plus the write-to-read skew: 100 ms at 24 kHz. Anything
// larger starts eating speech again.
const CUE_TAIL_FRAMES: usize = 2_400;
// The assistant cannot afford the full settle second *before* the beep: it turns
// the button press into a second of silence and the speaker presses again. 300 ms
// is enough for the ADC to cover the cue, and the cue itself keeps the reader
// busy for another 640 ms before speech is expected.
const UTTERANCE_SETTLE_FRAMES: usize = 7_200;
const I2C_TIMEOUT_MS: u32 = 200;
const MAX_RECORD_SECONDS: u32 = 10;
const MIN_RECORD_SECONDS: u32 = 1;
/// One assistant turn. Ten seconds is a long sentence and much longer than the
/// recognition budget wants to wait for; the bridge refuses past 60 s.
const MAX_UTTERANCE_MS: u32 = 10_000;
const MIN_UTTERANCE_MS: u32 = 500;
const MAX_LEVEL_MS: u32 = 5_000;
const MIN_LEVEL_MS: u32 = 100;
const MAX_TONE_MS: u32 = 2_000;
const MIN_TONE_MS: u32 = 100;
/// Recordings are normalized to this peak and playback then stays at unity on
/// the ES8311, so a hot WAV cannot clip the DAC.
/// Gap between the second cue beep and the start of capture, so the beep and the
/// speaker tail are not recorded.
const CUE_SETTLE_MS: u32 = 300;
const RECORD_PEAK_DB: f32 = -6.0;
// Sized for speech across a room: the microphones lose about 6 dB per doubling
// of distance, and a take from 1 m lands ~12 dB below the near-field case.
const RECORD_MAX_GAIN_DB: f32 = 24.0;
const TEST_DIR: &str = "EASELLET";
const TEST_WAV: &str = "EASELLET/REC.WAV";

const ES8311_ADDR: u8 = 0x18;
const ES7210_ADDR: u8 = 0x40;
// Unity DAC volume is the vendor default; the small external speaker is quiet
// enough that anything below unity sounds muffled. Both levels are adjustable
// at runtime (`audio volume`, `audio gain`), so this is only the boot value.
const DEFAULT_VOLUME_DB: f32 = 0.0;
// Vendor parity for this board: the reference firmware runs capture at 30 dB.
const DEFAULT_MIC_GAIN_DB: f32 = 30.0;
const MIN_VOLUME_DB: f32 = -24.0;
const MAX_VOLUME_DB: f32 = 12.0;
const MIN_MIC_GAIN_DB: f32 = 0.0;
const MAX_MIC_GAIN_DB: f32 = 37.5;

fn timeout() -> TickType_t {
    TickType::new_millis(u64::from(I2C_TIMEOUT_MS)).ticks()
}

/// Wall milliseconds, saturating rather than wrapping: a stalled stage should
/// report a large number, never a small one.
fn elapsed_ms(since: Instant) -> u32 {
    u32::try_from(since.elapsed().as_millis()).unwrap_or(u32::MAX)
}

fn write_reg(i2c: &mut I2cDriver<'_>, addr: u8, reg: u8, value: u8) -> Result<(), String> {
    i2c.write(addr, &[reg, value], timeout())
        .map_err(|error| format!("ERR audio codec {addr:02x} reg {reg:02x} write: {error}\r\n"))
}

fn read_reg(i2c: &mut I2cDriver<'_>, addr: u8, reg: u8) -> Result<u8, String> {
    let mut value = [0_u8; 1];
    i2c.write_read(addr, &[reg], &mut value, timeout())
        .map_err(|error| format!("ERR audio codec {addr:02x} reg {reg:02x} read: {error}\r\n"))?;
    Ok(value[0])
}

fn update_reg(
    i2c: &mut I2cDriver<'_>,
    addr: u8,
    reg: u8,
    mask: u8,
    value: u8,
) -> Result<(), String> {
    let current = read_reg(i2c, addr, reg)?;
    write_reg(i2c, addr, reg, (current & !mask) | (mask & value))
}

fn sequence(i2c: &mut I2cDriver<'_>, addr: u8, values: &[(u8, u8)]) -> Result<(), String> {
    for &(reg, value) in values {
        write_reg(i2c, addr, reg, value)?;
    }
    Ok(())
}

fn es8311_open(i2c: &mut I2cDriver<'_>) -> Result<(), String> {
    sequence(
        i2c,
        ES8311_ADDR,
        &[
            (0x44, 0x08),
            // Vendor repeats this first write for ES8311 I2C reliability.
            (0x44, 0x08),
            (0x01, 0x30),
            (0x02, 0x00),
            (0x03, 0x10),
            (0x16, 0x24),
            (0x04, 0x10),
            (0x05, 0x00),
            (0x0B, 0x00),
            (0x0C, 0x00),
            (0x10, 0x1F),
            (0x11, 0x7F),
            (0x00, 0x80),
        ],
    )?;

    // Slave mode, external MCLK, non-inverted MCLK and SCLK.
    update_reg(i2c, ES8311_ADDR, 0x00, 0x40, 0x00)?;
    write_reg(i2c, ES8311_ADDR, 0x01, 0x3F)?;
    update_reg(i2c, ES8311_ADDR, 0x06, 0x20, 0x00)?;
    sequence(
        i2c,
        ES8311_ADDR,
        &[(0x13, 0x10), (0x1B, 0x0A), (0x1C, 0x6A), (0x44, 0x58)],
    )
}

fn es8311_set_fs(i2c: &mut I2cDriver<'_>) -> Result<(), String> {
    // 16-bit samples in standard I2S on both serial ports.
    let dac = read_reg(i2c, ES8311_ADDR, 0x09)?;
    let adc = read_reg(i2c, ES8311_ADDR, 0x0A)?;
    write_reg(i2c, ES8311_ADDR, 0x09, (dac & 0xFC) | 0x0C)?;
    write_reg(i2c, ES8311_ADDR, 0x0A, (adc & 0xFC) | 0x0C)?;

    // 6.144 MHz MCLK / 24 kHz coefficient row: pre_div=1, pre_multi=1,
    // adc_div=1, dac_div=1, osr=0x10, lrck=0x00ff, bclk_div=4.
    let reg02 = read_reg(i2c, ES8311_ADDR, 0x02)? & 0x07;
    write_reg(i2c, ES8311_ADDR, 0x02, reg02)?;
    write_reg(i2c, ES8311_ADDR, 0x05, 0x00)?;
    let reg03 = (read_reg(i2c, ES8311_ADDR, 0x03)? & 0x80) | 0x10;
    write_reg(i2c, ES8311_ADDR, 0x03, reg03)?;
    let reg04 = (read_reg(i2c, ES8311_ADDR, 0x04)? & 0x80) | 0x10;
    write_reg(i2c, ES8311_ADDR, 0x04, reg04)?;
    let reg07 = read_reg(i2c, ES8311_ADDR, 0x07)? & 0xC0;
    write_reg(i2c, ES8311_ADDR, 0x07, reg07)?;
    write_reg(i2c, ES8311_ADDR, 0x08, 0xFF)?;
    let reg06 = (read_reg(i2c, ES8311_ADDR, 0x06)? & 0xE0) | 0x03;
    write_reg(i2c, ES8311_ADDR, 0x06, reg06)?;
    Ok(())
}

fn es8311_start(i2c: &mut I2cDriver<'_>, volume_db: f32) -> Result<(), String> {
    write_reg(i2c, ES8311_ADDR, 0x00, 0x80)?;
    write_reg(i2c, ES8311_ADDR, 0x01, 0x3F)?;

    let mut dac = read_reg(i2c, ES8311_ADDR, 0x09)?;
    let mut adc = read_reg(i2c, ES8311_ADDR, 0x0A)?;
    dac &= !0x40;
    adc &= !0x40;
    adc |= 0x40;
    // DAC-only mode: the ES7210 handles both microphones.
    dac &= !0x40;
    write_reg(i2c, ES8311_ADDR, 0x09, dac)?;
    write_reg(i2c, ES8311_ADDR, 0x0A, adc)?;

    sequence(
        i2c,
        ES8311_ADDR,
        &[
            (0x17, 0xBF),
            (0x0E, 0x02),
            (0x12, 0x00),
            (0x14, 0x1A),
            (0x0D, 0x01),
            (0x15, 0x40),
            (0x37, 0x08),
            (0x45, 0x00),
        ],
    )?;

    let mute = read_reg(i2c, ES8311_ADDR, 0x31)? & 0x9F;
    write_reg(i2c, ES8311_ADDR, 0x31, mute)?;
    write_reg(i2c, ES8311_ADDR, 0x32, es8311_volume_reg(volume_db))
}

fn es8311_suspend(i2c: &mut I2cDriver<'_>) -> Result<(), String> {
    sequence(
        i2c,
        ES8311_ADDR,
        &[
            (0x32, 0x00),
            (0x17, 0x00),
            (0x0E, 0xFF),
            (0x12, 0x02),
            (0x14, 0x00),
            (0x0D, 0xFA),
            (0x15, 0x00),
            (0x02, 0x10),
            (0x00, 0x00),
            (0x00, 0x1F),
            (0x01, 0x30),
            (0x01, 0x00),
            (0x45, 0x00),
            (0x0D, 0xFC),
            (0x02, 0x00),
        ],
    )
}

fn es7210_mic_select(i2c: &mut I2cDriver<'_>, gain: u8) -> Result<(), String> {
    // Disable all mic gain stages, then both bias/ADC power groups.
    for reg in 0x43..=0x46 {
        update_reg(i2c, ES7210_ADDR, reg, 0x10, 0x00)?;
    }
    write_reg(i2c, ES7210_ADDR, 0x4B, 0xFF)?;
    write_reg(i2c, ES7210_ADDR, 0x4C, 0xFF)?;

    // The two onboard analog microphones are wired to MIC1 and MIC2. MIC3 is
    // the speaker echo reference (OUTP/OUTN through R48/R53), so it must not
    // occupy the right slot. MIC1 and MIC2 share the MIC12 bias/ADC group.
    update_reg(i2c, ES7210_ADDR, 0x01, 0x0B, 0x00)?;
    write_reg(i2c, ES7210_ADDR, 0x4B, 0x00)?;
    for reg in [0x43_u8, 0x44] {
        update_reg(i2c, ES7210_ADDR, reg, 0x10, 0x10)?;
        update_reg(i2c, ES7210_ADDR, reg, 0x0F, gain)?;
    }

    // Standard I2S, not TDM.
    write_reg(i2c, ES7210_ADDR, 0x12, 0x00)
}

fn es7210_open(i2c: &mut I2cDriver<'_>, gain_db: f32) -> Result<u8, String> {
    sequence(
        i2c,
        ES7210_ADDR,
        &[
            (0x00, 0xFF),
            (0x00, 0x41),
            (0x01, 0x3F),
            (0x09, 0x30),
            (0x0A, 0x30),
            (0x23, 0x2A),
            (0x22, 0x0A),
            (0x20, 0x0A),
            (0x21, 0x2A),
        ],
    )?;
    // Slave mode.
    update_reg(i2c, ES7210_ADDR, 0x08, 0x01, 0x00)?;
    sequence(
        i2c,
        ES7210_ADDR,
        &[
            (0x40, 0x43),
            (0x41, 0x70),
            (0x42, 0x70),
            (0x07, 0x20),
            (0x02, 0xC1),
        ],
    )?;
    es7210_mic_select(i2c, es7210_gain_reg(gain_db))?;
    // The vendor driver latches this register here and replays it on every
    // start; after es7210_stop it holds 0x7F, which keeps the ADC clock gated.
    read_reg(i2c, ES7210_ADDR, 0x01)
}

fn es7210_set_fs(i2c: &mut I2cDriver<'_>) -> Result<(), String> {
    // 16-bit samples, standard I2S. The ES7210 is the I2S slave, so its
    // internal sample-rate dividers stay at their reset configuration.
    let mut iface = read_reg(i2c, ES7210_ADDR, 0x11)? & 0x1F;
    iface |= 0x60;
    iface &= 0xFC;
    write_reg(i2c, ES7210_ADDR, 0x11, iface)
}

fn es7210_start(i2c: &mut I2cDriver<'_>, clock_reg: u8, gain_db: f32) -> Result<(), String> {
    let clock_off = clock_reg;
    let gain = es7210_gain_reg(gain_db);
    sequence(
        i2c,
        ES7210_ADDR,
        &[
            (0x01, clock_off),
            (0x06, 0x00),
            (0x40, 0x43),
            (0x47, 0x08),
            (0x48, 0x08),
            (0x49, 0x08),
            (0x4A, 0x08),
        ],
    )?;
    es7210_mic_select(i2c, gain)?;
    sequence(
        i2c,
        ES7210_ADDR,
        &[(0x40, 0x43), (0x00, 0x71), (0x00, 0x41)],
    )
}

fn es7210_stop(i2c: &mut I2cDriver<'_>) -> Result<(), String> {
    sequence(
        i2c,
        ES7210_ADDR,
        &[
            (0x47, 0xFF),
            (0x48, 0xFF),
            (0x49, 0xFF),
            (0x4A, 0xFF),
            (0x4B, 0xFF),
            (0x4C, 0xFF),
            (0x40, 0xC0),
            (0x01, 0x7F),
            (0x06, 0x07),
        ],
    )
}

fn resolve(relative: &str) -> Result<PathBuf, String> {
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative.contains('\0')
    {
        return Err("ERR audio path must be relative and must not contain backslashes\r\n".into());
    }
    for component in relative.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err("ERR audio path contains an empty, '.' or '..' component\r\n".into());
        }
    }
    Ok(Path::new("/sdcard").join(relative))
}

fn duration_for_frames(frames: usize) -> Duration {
    Duration::from_secs_f64(frames as f64 / f64::from(SAMPLE_RATE))
}

pub fn level_bounds(ms: u32) -> bool {
    (MIN_LEVEL_MS..=MAX_LEVEL_MS).contains(&ms)
}

pub fn record_bounds(seconds: u32) -> bool {
    (MIN_RECORD_SECONDS..=MAX_RECORD_SECONDS).contains(&seconds)
}

pub fn utterance_bounds(ms: u32) -> bool {
    (MIN_UTTERANCE_MS..=MAX_UTTERANCE_MS).contains(&ms)
}

/// A captured turn, before any rate conversion or encoding.
#[derive(Debug)]
pub struct Utterance {
    /// 24 kHz stereo little-endian, peak-normalised.
    pub pcm: Vec<u8>,
    /// Levels before normalisation, so a turn can be judged on what the
    /// microphones actually delivered.
    pub measured: Stats,
    /// Gain the normalisation applied, for the console report.
    pub norm_db: f32,
    /// Cue playback plus the beep tail dropped after it, in wall milliseconds.
    pub cue_ms: u32,
    /// Capture alone, in wall milliseconds. It covers the warm-up settle second as
    /// well as the audio, which is why it exceeds the requested duration. The
    /// settle now runs *before* the cue, so none of it is time the speaker was
    /// using.
    pub capture_ms: u32,
    /// The codec start and settle window inside `capture_ms`.
    pub setup_ms: u32,
    /// The read loop inside `capture_ms`. Comparing it against the requested
    /// duration is the cheapest check that the microphones really delivered
    /// audio at the rate the header claims.
    pub read_ms: u32,
}

pub fn tone_bounds(ms: u32) -> bool {
    (MIN_TONE_MS..=MAX_TONE_MS).contains(&ms)
}

pub struct Audio {
    i2s: I2sDriver<'static, I2sBiDir>,
    pa: PinDriver<'static, Output>,
    pa_on: bool,
    running: bool,
    es7210_clock_reg: u8,
    /// Live playback level and microphone gain, adjustable through the console.
    volume_db: f32,
    gain_db: f32,
    last: Option<Stats>,
    /// Wall time the last capture spent starting the codec and discarding its settle
    /// window, and the time its read loop took. Split because the two answer different
    /// questions: a slow start is a codec cost, a slow read is a rate problem.
    capture_setup_ms: u32,
    capture_read_ms: u32,
    /// The always-listening path leaves the ADC open between console ticks; a
    /// turn closes it, because its own playback would otherwise be recorded.
    monitoring: bool,
    monitor_setup_ms: u32,
}

impl Audio {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        i2c: &mut I2cDriver<'_>,
        i2s: I2S0<'static>,
        mclk: Gpio14<'static>,
        bclk: Gpio15<'static>,
        ws: Gpio16<'static>,
        dout: Gpio17<'static>,
        din: Gpio18<'static>,
        pa_pin: Gpio7<'static>,
    ) -> Result<Self, String> {
        let mut pa = PinDriver::output(pa_pin)
            .map_err(|error| format!("ERR audio PA GPIO init: {error}\r\n"))?;
        pa.set_low()
            .map_err(|error| format!("ERR audio PA GPIO low: {error}\r\n"))?;

        // The I2S channel must exist before the codecs are programmed: creating
        // it starts MCLK, and the ES8311 clock-manager registers (0x02..0x08)
        // only latch while MCLK is running. The drivers keep DAC, ADC and PA
        // off until a command starts them.
        let config = StdConfig::new(
            I2sConfig::default().frames_per_buffer(240).auto_clear(true),
            StdClkConfig::from_sample_rate_hz(SAMPLE_RATE),
            StdSlotConfig::philips_slot_default(DataBitWidth::Bits16, SlotMode::Stereo),
            StdGpioConfig::default(),
        );
        let i2s = I2sDriver::new_std_bidir(i2s, &config, bclk, din, dout, Some(mclk), ws)
            .map_err(|error| format!("ERR audio I2S init: {error}\r\n"))?;

        es8311_open(i2c)?;
        es8311_set_fs(i2c)?;
        let es7210_clock_reg = es7210_open(i2c, DEFAULT_MIC_GAIN_DB)?;
        es7210_set_fs(i2c)?;

        Ok(Self {
            i2s,
            pa,
            pa_on: false,
            running: false,
            es7210_clock_reg,
            volume_db: DEFAULT_VOLUME_DB,
            gain_db: DEFAULT_MIC_GAIN_DB,
            last: None,
            capture_setup_ms: 0,
            capture_read_ms: 0,
            monitoring: false,
            monitor_setup_ms: 0,
        })
    }

    pub fn status(&self) -> String {
        let last = self.last.map_or_else(|| "none".to_owned(), Stats::summary);
        format!(
            "OK audio=es8311+es7210 rate={SAMPLE_RATE} bits={BITS} channels={CHANNELS} i2s=std mclk_x{MCLK_MULTIPLE} pa={} volume_db={} mic_gain_db={} last=\"{last}\"\r\n",
            if self.pa_on { "on" } else { "off" },
            self.volume_db,
            self.gain_db
        )
    }

    /// Set the ES8311 DAC volume live. `tenths` is decibels times ten, so the
    /// console can stay integer-only.
    pub fn set_volume(&mut self, i2c: &mut I2cDriver<'_>, tenths: i16) -> Result<String, String> {
        let db = f32::from(tenths) / 10.0;
        if !(MIN_VOLUME_DB..=MAX_VOLUME_DB).contains(&db) {
            return Err(format!(
                "ERR audio volume db must be {MIN_VOLUME_DB}..={MAX_VOLUME_DB}\r\n"
            ));
        }
        let reg = es8311_volume_reg(db);
        write_reg(i2c, ES8311_ADDR, 0x32, reg)?;
        self.volume_db = db;
        Ok(format!("OK audio volume db={db} reg=0x{reg:02x}\r\n"))
    }

    /// Current DAC attenuation, so settings can be persisted from live state.
    pub fn volume_db(&self) -> f32 {
        self.volume_db
    }

    /// Current microphone gain, so settings can be persisted from live state.
    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// Remember settings-file defaults without touching the codecs.
    ///
    /// The DAC and ADC stay powered down until an audio command, so there is no
    /// register to write yet; `start()` applies these values when it runs. That
    /// also means a corrupt settings file cannot write to the codecs at boot.
    pub fn set_defaults(
        &mut self,
        volume_tenths: Option<i16>,
        gain_tenths: Option<i16>,
    ) -> Result<(), String> {
        if let Some(tenths) = volume_tenths {
            let db = f32::from(tenths) / 10.0;
            if !(MIN_VOLUME_DB..=MAX_VOLUME_DB).contains(&db) {
                return Err(format!(
                    "ERR audio volume db must be {MIN_VOLUME_DB}..={MAX_VOLUME_DB}\r\n"
                ));
            }
            self.volume_db = db;
        }
        if let Some(tenths) = gain_tenths {
            let db = f32::from(tenths) / 10.0;
            if !(MIN_MIC_GAIN_DB..=MAX_MIC_GAIN_DB).contains(&db) {
                return Err(format!(
                    "ERR audio gain db must be {MIN_MIC_GAIN_DB}..={MAX_MIC_GAIN_DB}\r\n"
                ));
            }
            self.gain_db = db;
        }
        Ok(())
    }

    /// Set the ES7210 microphone gain live and remember it for the next start.
    pub fn set_gain(&mut self, i2c: &mut I2cDriver<'_>, tenths: i16) -> Result<String, String> {
        let db = f32::from(tenths) / 10.0;
        if !(MIN_MIC_GAIN_DB..=MAX_MIC_GAIN_DB).contains(&db) {
            return Err(format!(
                "ERR audio gain db must be {MIN_MIC_GAIN_DB}..={MAX_MIC_GAIN_DB}\r\n"
            ));
        }
        es7210_mic_select(i2c, es7210_gain_reg(db))?;
        self.gain_db = db;
        Ok(format!(
            "OK audio gain db={db} reg=0x{:02x}\r\n",
            es7210_gain_reg(db)
        ))
    }

    /// ESP-IDF constitutes this port as full-duplex and makes the later
    /// initialized channel (TX) the clock consumer, so both directions must be
    /// enabled for the shared clock to run.
    fn start(&mut self) -> Result<(), String> {
        if self.running {
            return Ok(());
        }
        self.i2s
            .rx_enable()
            .map_err(|error| format!("ERR audio I2S RX enable: {error}\r\n"))?;
        if let Err(error) = self.i2s.tx_enable() {
            let _ = self.i2s.rx_disable();
            return Err(format!("ERR audio I2S TX enable: {error}\r\n"));
        }
        self.running = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), String> {
        if !self.running {
            return Ok(());
        }
        let tx = self
            .i2s
            .tx_disable()
            .map_err(|error| format!("ERR audio I2S TX disable: {error}\r\n"));
        let rx = self
            .i2s
            .rx_disable()
            .map_err(|error| format!("ERR audio I2S RX disable: {error}\r\n"));
        if tx.is_ok() && rx.is_ok() {
            self.running = false;
        }
        tx.and(rx)
    }

    /// Read back the registers that decide whether the DAC and ADC data paths
    /// are actually live, instead of trusting that I2C writes were accepted.
    pub fn regs(&self, i2c: &mut I2cDriver<'_>) -> Result<String, String> {
        const ES8311: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x12, 0x14, 0x17,
            0x31, 0x32,
        ];
        const ES7210: [u8; 20] = [
            0x00, 0x01, 0x02, 0x06, 0x07, 0x08, 0x11, 0x12, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45,
            0x46, 0x47, 0x49, 0x4A, 0x4B, 0x4C,
        ];
        let mut reply = String::new();
        for (name, addr, list) in [
            ("es8311", ES8311_ADDR, &ES8311[..]),
            ("es7210", ES7210_ADDR, &ES7210[..]),
        ] {
            reply.push_str(name);
            for reg in list {
                let value = read_reg(i2c, addr, *reg)
                    .map_or_else(|_| "??".to_owned(), |value| format!("{value:02x}"));
                reply.push_str(&format!(" {reg:02x}={value}"));
            }
            reply.push_str("\r\n");
        }
        Ok(format!("OK audio regs running={}\r\n{reply}", self.running))
    }

    /// Read and throw away input frames, used to get past the AC-coupling
    /// transient of a freshly started ADC.
    fn drain(&mut self, frames: usize) -> Result<u32, String> {
        let mut remaining = frames
            .checked_mul(FRAME_BYTES)
            .ok_or_else(|| "ERR audio settle size overflow\r\n".to_owned())?;
        let mut scratch = vec![0_u8; CAPTURE_CHUNK];
        let mut short_reads = 0_u32;
        let deadline = Instant::now() + Duration::from_millis(1_500) + duration_for_frames(frames);
        while remaining > 0 && Instant::now() < deadline {
            let end = scratch.len().min(remaining);
            match self
                .i2s
                .read(&mut scratch[..end], TickType::new_millis(200).ticks())
            {
                Ok(0) => short_reads = short_reads.saturating_add(1),
                Ok(count) if count.is_multiple_of(FRAME_BYTES) => {
                    remaining = remaining.saturating_sub(count.min(remaining));
                }
                Ok(count) => {
                    return Err(format!(
                        "ERR audio settle read returned {count} bytes, not a whole frame\r\n"
                    ));
                }
                Err(error) => return Err(format!("ERR audio settle read: {error}\r\n")),
            }
        }
        Ok(short_reads)
    }

    /// Start the reader and the ADC, then throw the settle window away.
    ///
    /// Split out of `capture` because the assistant has to warm the ADC *before*
    /// the cue. The settle is a full second of audio, and doing it after the beep
    /// cost the first word of every utterance: «Сколько будет 458 умножить на 714»
    /// arrived as «будет 458 умножить на 714».
    fn start_reader(
        &mut self,
        i2c: &mut I2cDriver<'_>,
        settle_frames: usize,
    ) -> Result<u32, String> {
        let started = Instant::now();
        self.start()?;
        // Let MCLK/BCLK settle before starting the ADC.
        thread::sleep(Duration::from_millis(30));
        if let Err(error) = es7210_start(i2c, self.es7210_clock_reg, self.gain_db) {
            let _ = self.stop();
            return Err(error);
        }
        thread::sleep(Duration::from_millis(50));
        let settled = match self.drain(settle_frames) {
            Ok(short_reads) => short_reads,
            Err(error) => {
                let _ = es7210_stop(i2c);
                let _ = self.stop();
                return Err(error);
            }
        };
        self.capture_setup_ms = elapsed_ms(started);
        Ok(settled)
    }

    /// Read `frames` whole stereo frames. Leaves the reader running.
    fn read_frames(&mut self, frames: usize, mut short_reads: u32) -> Result<Vec<u8>, String> {
        let bytes_needed = frames
            .checked_mul(FRAME_BYTES)
            .ok_or_else(|| "ERR audio capture size overflow\r\n".to_owned())?;
        let mut data = vec![0_u8; bytes_needed];
        let mut offset = 0_usize;
        let deadline = Instant::now() + Duration::from_millis(1_500) + duration_for_frames(frames);

        let reading = Instant::now();
        let mut read_error = None;
        while offset < data.len() && Instant::now() < deadline {
            let mut end = data.len().min(offset + CAPTURE_CHUNK);
            // Keep every read aligned to a whole stereo frame.
            end -= (end - offset) % FRAME_BYTES;
            match self
                .i2s
                .read(&mut data[offset..end], TickType::new_millis(200).ticks())
            {
                Ok(0) => short_reads = short_reads.saturating_add(1),
                Ok(count) if count.is_multiple_of(FRAME_BYTES) => {
                    offset += count.min(data.len() - offset);
                }
                Ok(count) => {
                    read_error = Some(format!(
                        "ERR audio I2S read returned {count} bytes, not a whole frame\r\n"
                    ));
                    break;
                }
                Err(error) => {
                    read_error = Some(format!("ERR audio I2S read: {error}\r\n"));
                    break;
                }
            }
        }

        if let Some(error) = read_error {
            return Err(error);
        }
        if offset != data.len() {
            return Err(format!(
                "ERR audio capture timed out: got={offset} wanted={} short_reads={short_reads}\r\n",
                data.len()
            ));
        }
        self.capture_read_ms = elapsed_ms(reading);
        let mut measured = stats(&data, CHANNELS as u16);
        measured.short_reads = short_reads;
        self.last = Some(measured);
        Ok(data)
    }

    /// Stop the ADC and the clocks after a capture.
    ///
    /// The monitor claim is cleared with the driver: a console command such as
    /// `audio level` or `audio off` closes the codec underneath the idle watcher,
    /// and leaving `monitoring` set made the next tick read a stopped driver
    /// (`ESP_ERR_INVALID_STATE`), which the watcher then treated as a fatal error
    /// and disarmed itself. Clearing it here means `voice_poll` simply reopens the
    /// microphone on the next tick.
    fn stop_reader(&mut self, i2c: &mut I2cDriver<'_>) -> Result<(), String> {
        self.monitoring = false;
        let adc = es7210_stop(i2c);
        let clocks = self.stop();
        adc?;
        clocks
    }

    /// Open the microphone path and leave it running.
    ///
    /// The console loop already wakes every 20 ms, so the always-listening mode
    /// needs no thread and no audio callback: the loop drains the ring itself
    /// through `monitor_read`. The settle window is the utterance one, because
    /// the start-up artefact is the same on either path.
    pub fn monitor_start(&mut self, i2c: &mut I2cDriver<'_>) -> Result<(), String> {
        if self.monitoring {
            return Ok(());
        }
        let started = Instant::now();
        self.start_reader(i2c, UTTERANCE_SETTLE_FRAMES)?;
        self.monitoring = true;
        self.monitor_setup_ms = elapsed_ms(started);
        Ok(())
    }

    /// Drain what the DMA ring holds right now.
    ///
    /// Whole frames only. An empty vector means nothing arrived since the last
    /// call, which is normal when the loop runs faster than the audio.
    pub fn monitor_read(&mut self) -> Result<Vec<u8>, String> {
        if !self.monitoring {
            return Err("ERR audio monitor: not running\r\n".to_owned());
        }
        // A zero-tick read means "take whatever is there now", and one call
        // returns at most one DMA buffer. The ring holds several, and this loop
        // wakes half as often as a buffer is filled, so a single call per tick
        // would consume audio at half speed and the ring would drop the oldest
        // frames - which is exactly the beginning of a sentence. Drain until the
        // ring is empty instead.
        let mut chunk = vec![0_u8; MONITOR_CHUNK];
        let mut data = Vec::with_capacity(MONITOR_CHUNK);
        loop {
            match self.i2s.read(&mut chunk, 0) {
                Ok(0) => break,
                Ok(count) => {
                    let whole = count.min(chunk.len());
                    let whole = whole - whole % FRAME_BYTES;
                    if whole == 0 {
                        break;
                    }
                    data.extend_from_slice(&chunk[..whole]);
                    if data.len() >= MONITOR_DRAIN_CAP {
                        break;
                    }
                }
                // An empty ring is reported as a timeout rather than zero bytes,
                // and that is the normal case between frames.
                Err(error) if error.code() == esp_idf_svc::sys::ESP_ERR_TIMEOUT => break,
                Err(error) => return Err(format!("ERR audio monitor read: {error}\r\n")),
            }
        }
        Ok(data)
    }

    pub fn monitor_stop(&mut self, i2c: &mut I2cDriver<'_>) -> Result<(), String> {
        if !self.monitoring {
            return Ok(());
        }
        self.monitoring = false;
        self.stop_reader(i2c)
    }

    #[must_use]
    pub fn monitoring(&self) -> bool {
        self.monitoring
    }

    /// Wall time the monitor took to open, so a slow start is visible.
    #[must_use]
    pub fn monitor_setup_ms(&self) -> u32 {
        self.monitor_setup_ms
    }

    fn capture(&mut self, i2c: &mut I2cDriver<'_>, frames: usize) -> Result<Vec<u8>, String> {
        let short_reads = self.start_reader(i2c, SETTLE_FRAMES)?;
        let read = self.read_frames(frames, short_reads);
        let stopped = self.stop_reader(i2c);
        let data = read?;
        stopped?;
        Ok(data)
    }

    /// Frames per second the last read loop sustained.
    ///
    /// `capture` asks the DMA ring for a fixed number of frames and gives it 1.5 s of
    /// slack over the nominal duration, so a loop that runs long is invisible in the
    /// data. Dividing the frames by the time the loop actually took makes the
    /// difference visible: 24 000 means the microphones delivered what the header
    /// claims, and a reading near 20 000 means every utterance arrives time-stretched.
    fn read_rate_hz(&self, frames: usize) -> u32 {
        if self.capture_read_ms == 0 {
            return 0;
        }
        u32::try_from(frames as u64 * 1_000 / u64::from(self.capture_read_ms)).unwrap_or(0)
    }

    pub fn level(&mut self, i2c: &mut I2cDriver<'_>, ms: u32) -> Result<String, String> {
        if !level_bounds(ms) {
            return Err(format!(
                "ERR audio level ms must be {MIN_LEVEL_MS}..={MAX_LEVEL_MS}\r\n"
            ));
        }
        let frames = usize::try_from(u64::from(SAMPLE_RATE) * u64::from(ms) / 1_000)
            .map_err(|_| "ERR audio level duration overflow\r\n".to_owned())?;
        let data = self.capture(i2c, frames)?;
        let measured = self.last.unwrap_or_else(|| stats(&data, CHANNELS as u16));
        Ok(format!(
            "OK audio level ms={ms} {} short_reads={} setup_ms={} read_ms={} read_rate_hz={}\r\n",
            measured.measurement(),
            measured.short_reads,
            self.capture_setup_ms,
            self.capture_read_ms,
            self.read_rate_hz(frames)
        ))
    }

    pub fn record(&mut self, i2c: &mut I2cDriver<'_>, seconds: u32) -> Result<String, String> {
        if !record_bounds(seconds) {
            return Err(format!(
                "ERR audio record seconds must be {MIN_RECORD_SECONDS}..={MAX_RECORD_SECONDS}\r\n"
            ));
        }
        let frames = usize::try_from(u64::from(SAMPLE_RATE) * u64::from(seconds))
            .map_err(|_| "ERR audio record duration overflow\r\n".to_owned())?;
        // Record headlessly has no screen to say "start talking", so cue the
        // user with two beeps and let the speaker tail die down first.
        self.play_pcm(i2c, &cue_pcm())?;
        thread::sleep(Duration::from_millis(u64::from(CUE_SETTLE_MS)));
        let mut data = self.capture(i2c, frames)?;
        let measured = self.last.unwrap_or_else(|| stats(&data, CHANNELS as u16));
        let gain_db = normalize(&mut data, RECORD_PEAK_DB, RECORD_MAX_GAIN_DB);
        let normalized = stats(&data, CHANNELS as u16);

        let directory = Path::new("/sdcard").join(TEST_DIR);
        fs::create_dir_all(&directory)
            .map_err(|error| format!("ERR audio create {}: {error}\r\n", directory.display()))?;
        let path = resolve(TEST_WAV)?;
        let header = wav_header(data.len(), SAMPLE_RATE, CHANNELS as u16)?;
        let mut file = File::create(&path)
            .map_err(|error| format!("ERR audio create {}: {error}\r\n", path.display()))?;
        file.write_all(&header)
            .and_then(|()| file.write_all(&data))
            .map_err(|error| format!("ERR audio write {}: {error}\r\n", path.display()))?;
        Ok(format!(
            "OK audio recorded cue=double_beep cue_ms={} path={} bytes={} seconds={seconds} {} norm_db={gain_db} {} short_reads={}\r\n",
            cue_len_ms(),
            path.display(),
            data.len(),
            measured.measurement(),
            normalized.measurement(),
            measured.short_reads
        ))
    }

    /// Capture one spoken turn for the assistant: the same cue and settle as
    /// `record`, but no file, no WAV header and no fixed 10 s ceiling.
    ///
    /// The peak normalisation is applied here rather than after rate conversion
    /// because the recording is still 24 kHz stereo at this point, which is the
    /// material `normalize` was written for; the microphone delivers speech around
    /// -50 dBFS and recognition needs it well above the quantisation floor.
    pub fn utterance(&mut self, i2c: &mut I2cDriver<'_>, ms: u32) -> Result<Utterance, String> {
        if !utterance_bounds(ms) {
            return Err(format!(
                "ERR audio utterance ms must be {MIN_UTTERANCE_MS}..={MAX_UTTERANCE_MS}\r\n"
            ));
        }
        let frames = usize::try_from(u64::from(SAMPLE_RATE) * u64::from(ms) / 1_000)
            .map_err(|_| "ERR audio utterance duration overflow\r\n".to_owned())?;
        // Warm the ADC *before* the cue, and keep it warm across the beep. Starting
        // the codec behind the beep and then discarding a second of settle audio was
        // the reason every utterance lost its first word.
        let warmed = Instant::now();
        let short_reads = self.start_reader(i2c, UTTERANCE_SETTLE_FRAMES)?;
        let setup_ms = elapsed_ms(warmed);

        let cued = Instant::now();
        if let Err(error) = self.cue_duplex(i2c, &cue_pcm()) {
            let _ = self.stop_reader(i2c);
            return Err(error);
        }
        let tail = match self.drain(CUE_TAIL_FRAMES) {
            Ok(short_reads) => short_reads,
            Err(error) => {
                let _ = self.stop_reader(i2c);
                return Err(error);
            }
        };
        let cue_ms = elapsed_ms(cued);

        let captured = Instant::now();
        let read = self.read_frames(frames, short_reads.saturating_add(tail));
        let read_ms = elapsed_ms(captured);
        let stopped = self.stop_reader(i2c);
        let mut data = read?;
        stopped?;
        let capture_ms = setup_ms.saturating_add(read_ms);

        let measured = self.last.unwrap_or_else(|| stats(&data, CHANNELS as u16));
        if !heard_speech(&measured) {
            // Nothing was said. Refusing here costs nothing and keeps a silent
            // turn from becoming a confident invention on the far side; the
            // numbers are in the message so the threshold can be re-tuned from
            // evidence rather than by guess.
            return Err(format!(
                "ERR audio utterance: microphone heard nothing (peak_l={} peak_r={} rms_l={} rms_r={}); nothing sent\r\n",
                measured.peak[0], measured.peak[1], measured.rms[0], measured.rms[1]
            ));
        }
        let norm_db = normalize(&mut data, RECORD_PEAK_DB, RECORD_MAX_GAIN_DB);
        Ok(Utterance {
            pcm: data,
            measured,
            norm_db,
            cue_ms,
            capture_ms,
            setup_ms,
            read_ms,
        })
    }

    /// Wrap mono samples the voice detector captured as an utterance.
    ///
    /// The detector works in mono — loudness carries no stereo information — while
    /// everything downstream speaks interleaved stereo frames, so this widens the
    /// samples and reuses the same normalisation a cued capture applies. The
    /// timings are zero because no single codec start produced them: the audio
    /// arrived from the always-listening stream a frame at a time.
    pub fn utterance_from_mono(&mut self, mono: &[i16]) -> Utterance {
        let mut pcm = to_frames(mono, CHANNELS);
        let measured = stats(&pcm, CHANNELS as u16);
        let norm_db = normalize(&mut pcm, RECORD_PEAK_DB, RECORD_MAX_GAIN_DB);
        self.last = Some(stats(&pcm, CHANNELS as u16));
        Utterance {
            pcm,
            measured,
            norm_db,
            cue_ms: 0,
            capture_ms: 0,
            setup_ms: 0,
            read_ms: 0,
        }
    }

    /// Play already-interleaved 24 kHz stereo PCM: the downlink half of a turn.
    pub fn play_speech(&mut self, i2c: &mut I2cDriver<'_>, pcm: &[u8]) -> Result<(), String> {
        self.play_pcm(i2c, pcm)
    }

    /// Play the cue while reading and discarding what the microphone hears.
    ///
    /// `play_pcm` stops the driver when it finishes, which is exactly why the cue
    /// could not simply be moved after a warm-up: stopping and restarting the
    /// reader rebias the ADC and throws away the settle window again. This variant
    /// writes and reads in alternating chunks, like `duplex`, so the beep is never
    /// left in the DMA ring to be mistaken for speech and the ADC stays warm across
    /// it. Deliberately not shared with `play_pcm`: that path is proven on hardware
    /// and does not need to grow a mode.
    fn cue_duplex(&mut self, i2c: &mut I2cDriver<'_>, pcm: &[u8]) -> Result<(), String> {
        if pcm.is_empty() || !pcm.len().is_multiple_of(FRAME_BYTES) {
            return Err("ERR audio PCM length is not whole stereo frames\r\n".into());
        }
        // The reader is already running; only the speaker codec needs starting.
        thread::sleep(Duration::from_millis(30));
        es8311_start(i2c, self.volume_db)?;
        thread::sleep(Duration::from_millis(30));

        let chunk = DUPLEX_FRAMES * FRAME_BYTES;
        let mut scratch = vec![0_u8; chunk];
        let mut offset = 0_usize;
        let mut cue_error = self
            .pa
            .set_high()
            .map_err(|error| format!("ERR audio PA on: {error}\r\n"))
            .err();
        if cue_error.is_none() {
            self.pa_on = true;
            thread::sleep(Duration::from_millis(5));
            while offset < pcm.len() {
                let end = pcm.len().min(offset + chunk);
                let length = end - offset;
                if let Err(error) = self
                    .i2s
                    .write(&pcm[offset..end], TickType::new_millis(200).ticks())
                {
                    cue_error = Some(format!("ERR audio cue write: {error}\r\n"));
                    break;
                }
                // Reading the same number of frames keeps the two directions on
                // one clock and bounds how far behind the ring can fall.
                if let Err(error) = self
                    .i2s
                    .read(&mut scratch[..length], TickType::new_millis(200).ticks())
                {
                    cue_error = Some(format!("ERR audio cue read: {error}\r\n"));
                    break;
                }
                offset = end;
            }
            // Let the DMA drain before cutting PA power and clocks.
            thread::sleep(Duration::from_millis(100));
        }

        let pa_off = self
            .pa
            .set_low()
            .map_err(|error| format!("ERR audio PA off: {error}\r\n"));
        if pa_off.is_ok() {
            self.pa_on = false;
        }
        let suspended = es8311_suspend(i2c);
        if let Some(error) = cue_error {
            return Err(error);
        }
        pa_off?;
        suspended?;
        Ok(())
    }

    fn play_pcm(&mut self, i2c: &mut I2cDriver<'_>, pcm: &[u8]) -> Result<(), String> {
        if pcm.is_empty() || !pcm.len().is_multiple_of(FRAME_BYTES) {
            return Err("ERR audio PCM length is not whole stereo frames\r\n".into());
        }
        // The same I2S port carries both directions, so playback closes the
        // microphone. Leaving `monitoring` set made the next idle tick read a
        // stopped driver (`ESP_ERR_INVALID_STATE`) and the watcher disarm itself
        // after `ai ask`; clearing it here lets `voice_poll` reopen the mic after
        // the answer, and keeps the frame from hearing its own reply.
        if self.monitoring {
            self.monitor_stop(i2c)?;
        }
        self.start()?;
        thread::sleep(Duration::from_millis(30));
        if let Err(error) = es8311_start(i2c, self.volume_db) {
            let _ = self.stop();
            return Err(error);
        }
        thread::sleep(Duration::from_millis(30));

        let mut offset = 0_usize;
        let mut play_error = self
            .pa
            .set_high()
            .map_err(|error| format!("ERR audio PA on: {error}\r\n"))
            .err();
        if play_error.is_none() {
            self.pa_on = true;
            thread::sleep(Duration::from_millis(5));
            while offset < pcm.len() {
                let end = pcm.len().min(offset + PLAY_CHUNK);
                match self
                    .i2s
                    .write(&pcm[offset..end], TickType::new_millis(200).ticks())
                {
                    Ok(0) => break,
                    Ok(count) => offset += count.min(pcm.len() - offset),
                    Err(error) => {
                        play_error = Some(format!("ERR audio I2S write: {error}\r\n"));
                        break;
                    }
                }
            }
            // Let the DMA drain before removing PA power and clocks.
            thread::sleep(Duration::from_millis(100));
        }

        // Cleanup runs on every path, including an early PA or write failure.
        let pa_off = self
            .pa
            .set_low()
            .map_err(|error| format!("ERR audio PA off: {error}\r\n"));
        if pa_off.is_ok() {
            self.pa_on = false;
        }
        let suspended = es8311_suspend(i2c);
        let disabled = self.stop();
        if let Some(error) = play_error {
            return Err(error);
        }
        pa_off?;
        suspended?;
        disabled?;
        if offset != pcm.len() {
            return Err(format!(
                "ERR audio playback underrun: wrote={offset} wanted={}\r\n",
                pcm.len()
            ));
        }
        Ok(())
    }

    pub fn tone(&mut self, i2c: &mut I2cDriver<'_>, ms: u32) -> Result<String, String> {
        if !tone_bounds(ms) {
            return Err(format!(
                "ERR audio tone ms must be {MIN_TONE_MS}..={MAX_TONE_MS}\r\n"
            ));
        }
        let pcm = tone_pcm(ms);
        let frames = pcm.len() / FRAME_BYTES;
        self.play_pcm(i2c, &pcm)?;
        Ok(format!(
            "OK audio tone hz=440 ms={ms} volume_db={} frames={frames}\r\n",
            self.volume_db
        ))
    }

    /// Play the test tone and capture at the same time, so the microphone path
    /// can be measured against a known acoustic source instead of room noise.
    pub fn loop_tone(&mut self, i2c: &mut I2cDriver<'_>, ms: u32) -> Result<String, String> {
        if !tone_bounds(ms) {
            return Err(format!(
                "ERR audio loop ms must be {MIN_TONE_MS}..={MAX_TONE_MS}\r\n"
            ));
        }
        let pcm = tone_pcm(ms);
        let mut captured = vec![0_u8; pcm.len()];

        self.start()?;
        thread::sleep(Duration::from_millis(30));
        if let Err(error) = es8311_start(i2c, self.volume_db) {
            let _ = self.stop();
            return Err(error);
        }
        thread::sleep(Duration::from_millis(30));

        let mut loop_error = self
            .pa
            .set_high()
            .map_err(|error| format!("ERR audio PA on: {error}\r\n"))
            .err();
        if loop_error.is_none() {
            self.pa_on = true;
            thread::sleep(Duration::from_millis(5));
            match es7210_start(i2c, self.es7210_clock_reg, self.gain_db) {
                Err(error) => loop_error = Some(error),
                Ok(()) => {
                    thread::sleep(Duration::from_millis(50));
                    loop_error = self.duplex(&pcm, &mut captured);
                }
            }
        }

        let pa_off = self
            .pa
            .set_low()
            .map_err(|error| format!("ERR audio PA off: {error}\r\n"));
        if pa_off.is_ok() {
            self.pa_on = false;
        }
        let stopped = es7210_stop(i2c);
        let suspended = es8311_suspend(i2c);
        let disabled = self.stop();
        if let Some(error) = loop_error {
            return Err(error);
        }
        pa_off?;
        stopped?;
        suspended?;
        disabled?;

        let measured = stats(&captured, CHANNELS as u16);
        let amplitudes = tone_amplitudes(&captured, CHANNELS as u16, TONE_HZ, SAMPLE_RATE);
        self.last = Some(measured);
        Ok(format!(
            "OK audio loop hz={} ms={ms} {} tone_l={} tone_r={}\r\n",
            TONE_HZ as u32,
            measured.measurement(),
            amplitudes[0],
            amplitudes[1]
        ))
    }

    /// Write and read one DMA buffer at a time so playback and capture share the
    /// running clock instead of racing for it.
    fn duplex(&mut self, pcm: &[u8], captured: &mut [u8]) -> Option<String> {
        let chunk = DUPLEX_FRAMES * FRAME_BYTES;
        let silence = vec![0_u8; chunk];
        let mut scratch = vec![0_u8; chunk];
        // Silence is played while the ADC inputs re-bias, so the captured tone
        // is not mixed with the start-up transient.
        let settle = SETTLE_FRAMES * FRAME_BYTES;
        let total = settle + pcm.len();
        let mut position = 0_usize;
        while position < total {
            let end = if position < settle {
                settle.min(position + chunk)
            } else {
                total.min(position + chunk)
            };
            let length = end - position;
            let (source, destination): (&[u8], &mut [u8]) = if position < settle {
                (&silence[..length], &mut scratch[..length])
            } else {
                (
                    &pcm[position - settle..end - settle],
                    &mut captured[position - settle..end - settle],
                )
            };
            if let Err(error) = self.i2s.write(source, TickType::new_millis(200).ticks()) {
                return Some(format!("ERR audio loop I2S write: {error}\r\n"));
            }
            if let Err(error) = self
                .i2s
                .read(destination, TickType::new_millis(200).ticks())
            {
                return Some(format!("ERR audio loop I2S read: {error}\r\n"));
            }
            position = end;
        }
        None
    }

    /// Dump the first and last captured frames verbatim: aggregate levels cannot
    /// show a DC offset, a clipped rail or one slot repeated into both channels.
    pub fn samples(&mut self, i2c: &mut I2cDriver<'_>) -> Result<String, String> {
        const FRAMES: usize = 240;
        const SHOWN: usize = 4;
        let data = self.capture(i2c, FRAMES)?;
        let mut reply = String::from("OK audio hex");
        for (name, range) in [("head", 0..SHOWN), ("tail", FRAMES - SHOWN..FRAMES)] {
            reply.push_str(&format!(" {name}="));
            for (position, index) in range.enumerate() {
                let frame = &data[index * FRAME_BYTES..(index + 1) * FRAME_BYTES];
                let left = i16::from_le_bytes([frame[0], frame[1]]);
                let right = i16::from_le_bytes([frame[2], frame[3]]);
                if position > 0 {
                    reply.push(' ');
                }
                reply.push_str(&format!("{left},{right}"));
            }
        }
        reply.push_str("\r\n");
        Ok(reply)
    }

    pub fn play(&mut self, i2c: &mut I2cDriver<'_>, relative: &str) -> Result<String, String> {
        let path = resolve(relative)?;
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("ERR audio open {}: {error}\r\n", path.display()))?;
        if metadata.len() > 5 * 1024 * 1024 {
            return Err("ERR audio WAV exceeds 5 MiB\r\n".into());
        }
        let bytes = fs::read(&path)
            .map_err(|error| format!("ERR audio read {}: {error}\r\n", path.display()))?;
        let wav = parse_wav(&bytes)?;
        let pcm = if wav.channels == 1 {
            let mut stereo = Vec::with_capacity(wav.data.len() * 2);
            for sample in wav.data.chunks_exact(2) {
                stereo.extend_from_slice(sample);
                stereo.extend_from_slice(sample);
            }
            stereo
        } else {
            wav.data.to_vec()
        };
        let frames = pcm.len() / FRAME_BYTES;
        self.play_pcm(i2c, &pcm)?;
        Ok(format!(
            "OK audio played path={} rate={} bits={} channels={} frames={frames}\r\n",
            path.display(),
            wav.sample_rate,
            wav.bits,
            wav.channels
        ))
    }

    pub fn shutdown(&mut self, i2c: &mut I2cDriver<'_>) -> Result<String, String> {
        let pa = self
            .pa
            .set_low()
            .map_err(|error| format!("ERR audio PA off: {error}\r\n"));
        if pa.is_ok() {
            self.pa_on = false;
        }
        let stopped = es7210_stop(i2c);
        // Enabling both directions is required for full-duplex: the RX channel
        // owns the shared clock, so TX alone never drains its DMA buffers.
        let disabled = self.stop();
        self.monitoring = false;
        let suspended = es8311_suspend(i2c);
        let result = pa.and(suspended).and(stopped).and(disabled);
        Ok(format!(
            "OK audio off pa=off es8311=suspended es7210=stopped result={}\r\n",
            if result.is_ok() { "ok" } else { "partial" }
        ))
    }
}
