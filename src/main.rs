mod actions;
mod audio;
mod bridge;
mod buttons;
mod clock;
mod config;
mod console;
mod diag;
mod display;
mod font;
mod gallery;
mod http;
mod image;
mod net;
mod opendisplay;
mod panel;
mod pcm;
mod power;
mod sleep;
mod storage;
mod vad;
mod web;

use std::{
    fs,
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use console::{Command, LineBuffer};
use esp_idf_svc::{
    hal::{
        delay::TickType,
        gpio::{Input, PinDriver, Pull},
        i2c::{I2cConfig, I2cDriver},
        peripherals::Peripherals,
        units::KiloHertz,
        usb_serial::{UsbSerialConfig, UsbSerialDriver},
    },
    sys::{self, EspError, TickType_t},
};

// 7-bit address of the AXP2101 PMIC (confirmed by chip-ID register 0x03).
const PMIC_ADDR: u8 = 0x34;
const AXP2101_CHIP_ID: u8 = 0x4A;
const AXP2101_LDO_ONOFF_CTRL0: u8 = 0x90;
const AXP2101_LDO_VOL2_CTRL: u8 = 0x94;
const AXP2101_LDO_VOL3_CTRL: u8 = 0x95;
const AXP2101_ALDO3_ENABLE: u8 = 1 << 2;
const AXP2101_ALDO4_ENABLE: u8 = 1 << 3;
const AXP2101_ALDO_3300MV_CODE: u8 = ((3300_u16 - 500) / 100) as u8;
const PCF85063_ADDR: u8 = 0x51;
const SHTC3_ADDR: u8 = 0x70;

const I2C_TIMEOUT_MS: u32 = 200;

fn i2c_timeout() -> TickType_t {
    TickType::new_millis(u64::from(I2C_TIMEOUT_MS)).ticks()
}

/// Start address + repeated start + read.
fn read_regs(i2c: &mut I2cDriver<'_>, addr: u8, reg: u8, out: &mut [u8]) -> Result<(), EspError> {
    i2c.write_read(addr, &[reg], out, i2c_timeout())
}

fn read_reg(i2c: &mut I2cDriver<'_>, addr: u8, reg: u8) -> Result<u8, EspError> {
    let mut byte = [0_u8; 1];
    read_regs(i2c, addr, reg, &mut byte)?;
    Ok(byte[0])
}

fn write_reg(i2c: &mut I2cDriver<'_>, addr: u8, reg: u8, value: u8) -> Result<(), EspError> {
    i2c.write(addr, &[reg, value], i2c_timeout())
}

/// The schematic routes EPD_VCC through AXP2101 ALDO4. It is set to 3.3 V and
/// enabled with read-back verification, because the panel is useless without it.
/// Touch no other output or charger setting here.
fn configure_display_rail(i2c: &mut I2cDriver<'_>) -> Result<(), String> {
    let read = |i2c: &mut I2cDriver<'_>, reg: u8| {
        read_reg(i2c, PMIC_ADDR, reg).map_err(|e| format!("ERR PMIC reg {reg:02x}: {e}\r\n"))
    };
    let chip_id = read(i2c, 0x03)?;
    if chip_id != AXP2101_CHIP_ID {
        return Err(format!(
            "ERR PMIC at 0x{PMIC_ADDR:02x} is not AXP2101: chip_id=0x{chip_id:02x}; required rails not configured\r\n"
        ));
    }

    let voltage = read(i2c, AXP2101_LDO_VOL3_CTRL)?;
    let wanted_voltage = (voltage & 0xE0) | AXP2101_ALDO_3300MV_CODE;
    if wanted_voltage != voltage {
        write_reg(i2c, PMIC_ADDR, AXP2101_LDO_VOL3_CTRL, wanted_voltage)
            .map_err(|e| format!("ERR PMIC ALDO4/EPD_VCC voltage write: {e}\r\n"))?;
    }
    let enable = read(i2c, AXP2101_LDO_ONOFF_CTRL0)?;
    if enable & AXP2101_ALDO4_ENABLE == 0 {
        write_reg(
            i2c,
            PMIC_ADDR,
            AXP2101_LDO_ONOFF_CTRL0,
            enable | AXP2101_ALDO4_ENABLE,
        )
        .map_err(|e| format!("ERR PMIC ALDO4/EPD_VCC enable write: {e}\r\n"))?;
    }

    let voltage = read(i2c, AXP2101_LDO_VOL3_CTRL)?;
    let enable = read(i2c, AXP2101_LDO_ONOFF_CTRL0)?;
    if voltage & 0x1F != AXP2101_ALDO_3300MV_CODE || enable & AXP2101_ALDO4_ENABLE == 0 {
        return Err(format!(
            "ERR PMIC ALDO4/EPD_VCC readback mismatch: vol=0x{voltage:02x} onoff=0x{enable:02x}\r\n"
        ));
    }
    Ok(())
}

/// Audio_VCC sits on AXP2101 ALDO3. The vendor firmware only writes its voltage
/// and relies on the power-on default that leaves the rail enabled, so this path
/// is deliberately read-first: it reports what the PMIC already does, and writes
/// only when the rail is off or not 3.3 V. It runs as part of audio bring-up, not
/// during the display boot path, so a console-only session never touches it.
fn ensure_audio_rail(i2c: &mut I2cDriver<'_>) -> Result<String, String> {
    let read = |i2c: &mut I2cDriver<'_>, reg: u8| {
        read_reg(i2c, PMIC_ADDR, reg).map_err(|e| format!("ERR PMIC reg {reg:02x}: {e}\r\n"))
    };
    let voltage = read(i2c, AXP2101_LDO_VOL2_CTRL)?;
    let enable = read(i2c, AXP2101_LDO_ONOFF_CTRL0)?;
    let millivolts = 500 + u16::from(voltage & 0x1F) * 100;
    let enabled = enable & AXP2101_ALDO3_ENABLE != 0;
    let mut writes = 0_u32;
    if voltage & 0x1F != AXP2101_ALDO_3300MV_CODE {
        let wanted = (voltage & 0xE0) | AXP2101_ALDO_3300MV_CODE;
        write_reg(i2c, PMIC_ADDR, AXP2101_LDO_VOL2_CTRL, wanted)
            .map_err(|e| format!("ERR PMIC ALDO3/Audio_VCC voltage write: {e}\r\n"))?;
        writes += 1;
    }
    if !enabled {
        write_reg(
            i2c,
            PMIC_ADDR,
            AXP2101_LDO_ONOFF_CTRL0,
            enable | AXP2101_ALDO3_ENABLE,
        )
        .map_err(|e| format!("ERR PMIC ALDO3/Audio_VCC enable write: {e}\r\n"))?;
        writes += 1;
    }
    let checked_voltage = read(i2c, AXP2101_LDO_VOL2_CTRL)?;
    let checked_enable = read(i2c, AXP2101_LDO_ONOFF_CTRL0)?;
    if checked_voltage & 0x1F != AXP2101_ALDO_3300MV_CODE
        || checked_enable & AXP2101_ALDO3_ENABLE == 0
    {
        return Err(format!(
            "ERR PMIC ALDO3/Audio_VCC readback mismatch: vol=0x{checked_voltage:02x} onoff=0x{checked_enable:02x}\r\n"
        ));
    }
    Ok(format!(
        "OK audio_rail=ALDO3/Audio_VCC found_mv={millivolts} found_en={} writes={writes} now_mv=3300 now_en=1\r\n",
        u8::from(enabled)
    ))
}

fn hundredths(value: i32) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let magnitude = value.unsigned_abs();
    format!("{sign}{}.{:02}", magnitude / 100, magnitude % 100)
}

fn sensor(i2c: &mut I2cDriver<'_>) -> Result<String, String> {
    let (id_value, temperature, humidity) = shtc3_read(i2c)?;
    Ok(format!(
        "OK sensor=shtc3 id=0x{id_value:04x} temp_c={} rh_pct={}.{:02}\r\n",
        hundredths(temperature),
        humidity / 100,
        humidity % 100
    ))
}

/// One SHTC3 measurement: id, temperature in 0.01 °C, humidity in 0.01 %.
fn shtc3_read(i2c: &mut I2cDriver<'_>) -> Result<(u16, i32, u32), String> {
    // The part must be woken before any transaction after a previous sleep.
    i2c.write(SHTC3_ADDR, &[0x35, 0x17], i2c_timeout())
        .map_err(|e| {
            format!("ERR sensor absent or wakeup failed at 0x{SHTC3_ADDR:02x}: {e}\r\n")
        })?;
    thread::sleep(Duration::from_millis(1));

    // 0xEFC8 is a 16-bit command, not a one-byte register address.
    let mut id = [0_u8; 3];
    i2c.write_read(SHTC3_ADDR, &[0xEF, 0xC8], &mut id, i2c_timeout())
        .map_err(|e| format!("ERR sensor ID read: {e}\r\n"))?;
    let id_value = u16::from_be_bytes([id[0], id[1]]);
    if diag::crc8(&id[..2]) != id[2] {
        return Err(format!("ERR sensor ID CRC mismatch: raw={id:02x?}\r\n"));
    }
    if !diag::shtc3_id_valid(id_value) {
        return Err(format!(
            "ERR sensor is not an SHTC3: id=0x{id_value:04x}\r\n"
        ));
    }

    // Clock stretching disabled, temperature first, normal mode.
    i2c.write(SHTC3_ADDR, &[0x78, 0x66], i2c_timeout())
        .map_err(|e| format!("ERR sensor measure: {e}\r\n"))?;
    thread::sleep(Duration::from_millis(20));

    let mut data = [0_u8; 6];
    let result = i2c.read(SHTC3_ADDR, &mut data, i2c_timeout());
    // Return the part to sleep even when the read failed.
    let _ = i2c.write(SHTC3_ADDR, &[0xB0, 0x98], i2c_timeout());
    result.map_err(|e| format!("ERR sensor read: {e}\r\n"))?;

    if diag::crc8(&data[0..2]) != data[2] || diag::crc8(&data[3..5]) != data[5] {
        return Err(format!("ERR sensor data CRC mismatch: raw={data:02x?}\r\n"));
    }
    let temperature = diag::shtc3_temperature(u16::from_be_bytes([data[0], data[1]]));
    let humidity = diag::shtc3_humidity(u16::from_be_bytes([data[3], data[4]]));
    Ok((id_value, temperature, humidity))
}

/// The battery, from five read-only AXP2101 registers.
fn battery(i2c: &mut I2cDriver<'_>) -> Result<power::Battery, String> {
    let mut read = |reg: u8| {
        read_reg(i2c, PMIC_ADDR, reg).map_err(|e| format!("ERR PMIC reg {reg:02x}: {e}\r\n"))
    };
    Ok(power::Battery::from_registers(
        read(0x00)?,
        read(0x01)?,
        read(0x34)?,
        read(0x35)?,
        read(0xA4)?,
    ))
}

/// The chip's own temperature sensor, installed once; `None` if the driver
/// refused, which only costs the MSD's chip-temperature byte.
struct ChipTemperature(Option<sys::temperature_sensor_handle_t>);

impl ChipTemperature {
    fn new() -> Self {
        let config = sys::temperature_sensor_config_t {
            range_min: -10,
            range_max: 80,
            clk_src:
                sys::soc_periph_temperature_sensor_clk_src_t_TEMPERATURE_SENSOR_CLK_SRC_DEFAULT,
            ..Default::default()
        };
        let mut handle: sys::temperature_sensor_handle_t = core::ptr::null_mut();
        // SAFETY: `config` and `handle` are valid for the call; the handle is
        // owned by this struct for the rest of the program.
        let ok = unsafe {
            sys::temperature_sensor_install(&config, &mut handle) == sys::ESP_OK
                && sys::temperature_sensor_enable(handle) == sys::ESP_OK
        };
        Self(ok.then_some(handle))
    }

    fn celsius(&self) -> Option<f32> {
        let handle = self.0?;
        let mut value = 0.0_f32;
        // SAFETY: the handle was installed and enabled in `new`.
        let ok = unsafe { sys::temperature_sensor_get_celsius(handle, &mut value) == sys::ESP_OK };
        ok.then_some(value)
    }
}

/// What the frame last measured: shown in the portal, published in the MSD.
#[derive(Default)]
struct Sensors {
    battery: Option<power::Battery>,
    /// Temperature and humidity, both in hundredths.
    ambient: Option<(i32, u32)>,
    chip_c: Option<f32>,
    /// When these were read; `None` until the first reading.
    at: Option<Instant>,
}

/// How often the readings are refreshed. The SHTC3 takes ~20 ms and the PMIC a
/// few register reads, so this is about freshness, not cost.
const SENSOR_PERIOD: Duration = Duration::from_secs(30);

impl Sensors {
    fn due(&self) -> bool {
        self.at.is_none_or(|at| at.elapsed() >= SENSOR_PERIOD)
    }

    fn read(&mut self, i2c: &mut I2cDriver<'_>, chip: &ChipTemperature) {
        self.battery = battery(i2c).ok();
        self.ambient = shtc3_read(i2c).ok().map(|(_, temp, rh)| (temp, rh));
        self.chip_c = chip.celsius();
        self.at = Some(Instant::now());
        opendisplay::publish(
            self.battery
                .filter(|battery| battery.present)
                .map(|battery| battery.mv),
            self.chip_c
                .map(|celsius| celsius.round().clamp(-40.0, 87.0) as i8),
            self.ambient
                .map(|(temp, rh)| opendisplay_proto::msd::Ambient {
                    temp_deci: (temp / 10).clamp(-400, 1250) as i16,
                    rh_deci: (rh / 10).min(1000) as u16,
                }),
        );
    }

    /// The portal's `/sensors` document.
    fn json(&self) -> String {
        let battery = self.battery.map_or_else(
            || "null".to_owned(),
            |battery| {
                format!(
                    "{{\"present\":{},\"usb\":{},\"charging\":{},\"mv\":{},\"percent\":{},\"low\":{}}}",
                    battery.present,
                    battery.usb,
                    battery.charging,
                    battery.mv,
                    battery.percent,
                    battery.low()
                )
            },
        );
        let ambient = self.ambient.map_or_else(
            || "null".to_owned(),
            |(temp, rh)| {
                format!(
                    "{{\"temp_c\":{},\"rh_pct\":{}.{:02}}}",
                    hundredths(temp),
                    rh / 100,
                    rh % 100
                )
            },
        );
        let chip = self
            .chip_c
            .map_or_else(|| "null".to_owned(), |celsius| format!("{celsius:.1}"));
        let age = self.at.map_or_else(
            || "null".to_owned(),
            |at| at.elapsed().as_secs().to_string(),
        );
        format!("{{\"battery\":{battery},\"ambient\":{ambient},\"chip_c\":{chip},\"age_s\":{age}}}")
    }
}

/// Raw PCF85063 registers 0x00..=0x0A; the first byte is control_1.
fn rtc_regs(i2c: &mut I2cDriver<'_>) -> Result<[u8; 11], String> {
    i2c.write(PCF85063_ADDR, &[], i2c_timeout())
        .map_err(|e| format!("ERR RTC absent or bus error at 0x{PCF85063_ADDR:02x}: {e}\r\n"))?;
    let mut regs = [0_u8; 11];
    read_regs(i2c, PCF85063_ADDR, 0x00, &mut regs).map_err(|e| format!("ERR RTC read: {e}\r\n"))?;
    Ok(regs)
}

/// A parseable time means the oscillator runs and the registers hold a valid
/// date, so only an invalid or unreadable RTC justifies waking the radio at boot.
fn rtc_needs_sync(i2c: &mut I2cDriver<'_>) -> bool {
    match rtc_regs(i2c) {
        Ok(regs) => {
            let time_regs: [u8; 7] = regs[4..11].try_into().expect("slice is 7 bytes");
            diag::pcf85063_time(regs[0], &time_regs).is_none()
        }
        Err(_) => true,
    }
}

fn rtc(i2c: &mut I2cDriver<'_>) -> Result<String, String> {
    let regs = rtc_regs(i2c)?;

    let control_1 = regs[0];
    let seconds = regs[4];
    let osc_stopped = seconds & 0x80 != 0;
    let time_regs: [u8; 7] = regs[4..11].try_into().expect("slice is 7 bytes");
    match diag::pcf85063_time(control_1, &time_regs) {
        Some(clock) => Ok(format!(
            "OK rtc=pcf85063 {:04}-{:02}-{:02} {:02}:{:02}:{:02} weekday={} hour_mode={}\r\n",
            clock.year,
            clock.month,
            clock.day,
            clock.hour,
            clock.minute,
            clock.second,
            clock.weekday,
            if control_1 & 0x02 != 0 { "12" } else { "24" }
        )),
        None => Ok(format!(
            "OK rtc=pcf85063 invalid osc_stopped={osc_stopped} control_1=0x{control_1:02x} raw={:02x?}\r\n",
            &regs[4..11]
        )),
    }
}

fn pmu(i2c: &mut I2cDriver<'_>) -> Result<String, String> {
    i2c.write(PMIC_ADDR, &[], i2c_timeout())
        .map_err(|e| format!("ERR PMIC absent or bus error at 0x{PMIC_ADDR:02x}: {e}\r\n"))?;
    let read = |i2c: &mut I2cDriver<'_>, reg: u8| {
        read_reg(i2c, PMIC_ADDR, reg).map_err(|e| format!("ERR PMIC reg {reg:02x}: {e}\r\n"))
    };
    let chip_id = read(i2c, 0x03)?;
    if chip_id != AXP2101_CHIP_ID {
        return Err(format!(
            "ERR PMIC at 0x{PMIC_ADDR:02x} is not AXP2101: chip_id=0x{chip_id:02x}, no registers decoded\r\n"
        ));
    }
    let status1 = read(i2c, 0x00)?;
    let status2 = read(i2c, 0x01)?;
    let control = read(i2c, 0x18)?;
    let current = read(i2c, 0x62)?;
    let voltage = read(i2c, 0x64)?;
    let percentage = read(i2c, 0xA4)?;
    // Power key: REG22[1] lets a long PWRON hold power off, REG22[0] makes it a
    // restart instead; REG27 holds the press thresholds. VBAT is REG34[5:0]:REG35
    // in millivolts, its ADC channel enabled at reset (REG30[0]). All reads.
    let pwroff_enable = read(i2c, 0x22)?;
    let key_levels = read(i2c, 0x27)?;
    let vbat_mv = (u16::from(read(i2c, 0x34)? & 0x3F) << 8) | u16::from(read(i2c, 0x35)?);
    let off_hold_s = [4, 6, 8, 10][usize::from((key_levels >> 2) & 0b11)];
    let long_press_ms = [1_000, 1_500, 2_000, 2_500][usize::from((key_levels >> 4) & 0b11)];
    let ldo_onoff = read(i2c, AXP2101_LDO_ONOFF_CTRL0)?;
    let aldo4_voltage = read(i2c, AXP2101_LDO_VOL3_CTRL)?;
    let aldo3_voltage = read(i2c, AXP2101_LDO_VOL2_CTRL)?;
    let aldo4_mv = 500 + u16::from(aldo4_voltage & 0x1F) * 100;
    let aldo3_mv = 500 + u16::from(aldo3_voltage & 0x1F) * 100;

    let current_ma = diag::axp2101_charge_current(current)
        .map_or_else(|| "unset".to_owned(), |ma| ma.to_string());
    let voltage_mv = diag::axp2101_charge_voltage(voltage)
        .map_or_else(|| "unset".to_owned(), |mv| mv.to_string());
    Ok(format!(
        "OK pmic=axp2101 chip_id=0x{chip_id:02x} vbus_good={} battery_present={} batfet={} direction={} charge_state={} gauge={} cell_charge={} backup_charge={} watchdog={} ichg_ma={current_ma} vreg_mv={voltage_mv} soc_pct={percentage} vbat_mv={vbat_mv} aldo4_en={} aldo4_mv={aldo4_mv} aldo3_en={} aldo3_mv={aldo3_mv}\r\n  pwrkey_off={} pwrkey_off_action={} pwrkey_off_hold_s={off_hold_s} pwrkey_long_ms={long_press_ms} reg22=0x{pwroff_enable:02x} reg27=0x{key_levels:02x}\r\n",
        (status1 >> 5) & 1,
        (status1 >> 3) & 1,
        (status1 >> 4) & 1,
        diag::axp2101_direction(status2),
        diag::axp2101_charge_state(status2),
        (control >> 3) & 1,
        (control >> 1) & 1,
        (control >> 2) & 1,
        control & 1,
        (ldo_onoff >> 3) & 1,
        (ldo_onoff >> 2) & 1,
        (pwroff_enable >> 1) & 1,
        if pwroff_enable & 1 == 1 {
            "restart"
        } else {
            "power-off"
        },
    ))
}

fn reply(result: Result<(), String>, ok: &str) -> String {
    match result {
        Ok(()) => format!("OK {ok}\r\n"),
        Err(error) => error,
    }
}

/// USB serial write granularity: one CDC buffer.
///
/// A single write larger than the configured 1024-byte TX buffer returns zero bytes
/// rather than a short count, and the whole reply disappears — `help` grew past that
/// boundary and stopped answering at all. Handing the driver a slice that always fits
/// turns that into a drain loop instead of a silent loss.
const SEND_CHUNK: usize = 256;

fn send(usb: &mut UsbSerialDriver<'_>, message: &str) -> Result<(), EspError> {
    // Bound each response; an unplugged host must not stall firmware forever.
    for chunk in message.as_bytes().chunks(SEND_CHUNK) {
        let mut remaining = chunk;
        while !remaining.is_empty() {
            let written = usb.write(remaining, TickType::new_millis(100).ticks())?;
            if written == 0 || written > remaining.len() {
                return Ok(());
            }
            remaining = &remaining[written..];
        }
    }
    Ok(())
}

type DisplayPair = (display::Display<'static>, &'static mut [u8]);

/// What the paint worker should put on the panel.
enum PaintJob {
    /// A turn: the question and, when there is one, the answer.
    Turn {
        heard: String,
        answer: Option<String>,
    },
    /// The frame already holds the picture; refresh it as it stands. Used by the
    /// slideshow, which decodes on the main loop because the card has one owner.
    Frame,
}

struct PaintDone {
    display: display::Display<'static>,
    frame: &'static mut [u8],
    result: Result<String, String>,
}

enum PainterState {
    Ready(DisplayPair),
    Painting(mpsc::Receiver<PaintDone>),
}

/// Owns the panel and decides who refreshes it.
///
/// A Spectra 6 refresh takes about 31 s. Doing that here on the main loop would
/// close the microphone for the whole time, so an idle turn hands the panel to a
/// short-lived worker thread and keeps listening. The paint itself waits for the
/// conversation to go quiet, otherwise a rapid series of questions keeps the
/// panel a minute behind and repaints answers nobody will read. Console turns
/// stay synchronous: the person at the terminal is already waiting for the report.
struct Painter {
    inner: Result<PainterState, String>,
    pending: Option<PaintJob>,
    /// Told how the running refresh ended, for a requester on another thread
    /// (OpenDisplay reports it to its client as 0x73 or 0x74).
    watcher: Option<mpsc::Sender<Result<String, String>>>,
}

impl Painter {
    fn new(state: Result<DisplayPair, String>) -> Self {
        Self {
            inner: state.map(PainterState::Ready),
            pending: None,
            watcher: None,
        }
    }

    /// Pass a finished paint's result to whoever asked to hear it.
    fn report(&mut self, result: &Result<String, String>) {
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.send(result.clone());
        }
    }

    /// Reclaim a finished refresh and, once the conversation is quiet, start the
    /// newest deferred paint.
    fn poll(&mut self, quiet: bool) {
        let state = std::mem::replace(&mut self.inner, Err("ERR display busy\r\n".to_owned()));
        self.inner = match state {
            Ok(PainterState::Painting(rx)) => match rx.try_recv() {
                Ok(done) => {
                    self.report(&done.result);
                    match done.result {
                        Ok(_) => Ok(PainterState::Ready((done.display, done.frame))),
                        Err(error) => Err(error),
                    }
                }
                Err(mpsc::TryRecvError::Empty) => Ok(PainterState::Painting(rx)),
                Err(mpsc::TryRecvError::Disconnected) => {
                    let error = "ERR display paint thread stopped\r\n".to_owned();
                    self.report(&Err(error.clone()));
                    Err(error)
                }
            },
            other => other,
        };
        if quiet
            && matches!(self.inner, Ok(PainterState::Ready(_)))
            && let Some(job) = self.pending.take()
            && let Ok(pair) = self.take_ready()
        {
            let _ = self.spawn(pair, job);
        }
    }

    /// Wait for any refresh in progress and take the panel for a synchronous use.
    fn take_ready(&mut self) -> Result<DisplayPair, String> {
        loop {
            let state = std::mem::replace(&mut self.inner, Err("ERR display busy\r\n".to_owned()));
            match state {
                Ok(PainterState::Ready(pair)) => return Ok(pair),
                Ok(PainterState::Painting(rx)) => {
                    // A console command has arrived and is about to draw over
                    // whatever text was queued, so the queued job is dropped.
                    self.pending = None;
                    match rx.recv() {
                        Ok(done) => {
                            self.report(&done.result);
                            self.inner = match done.result {
                                Ok(_) => Ok(PainterState::Ready((done.display, done.frame))),
                                Err(error) => Err(error),
                            };
                        }
                        Err(_) => {
                            let error = "ERR display paint thread stopped\r\n".to_owned();
                            self.report(&Err(error.clone()));
                            self.inner = Err(error);
                        }
                    }
                }
                Err(error) => {
                    self.inner = Err(error.clone());
                    return Err(error);
                }
            }
        }
    }

    fn with_ready<T>(
        &mut self,
        action: impl FnOnce(&mut display::Display<'static>, &mut [u8]) -> T,
    ) -> Result<T, String> {
        self.pending = None;
        let (mut display, frame) = self.take_ready()?;
        let result = action(&mut display, &mut frame[..]);
        self.inner = Ok(PainterState::Ready((display, frame)));
        Ok(result)
    }

    /// Paint and wait, for a turn whose report goes to the console.
    fn paint_now(&mut self, heard: &str, answer: Option<&str>) -> Result<String, String> {
        self.pending = None;
        let (mut display, frame) = self.take_ready()?;
        let result = paint_turn(&mut display, &mut frame[..], heard, answer);
        self.inner = Ok(PainterState::Ready((display, frame)));
        result
    }

    /// Queue the newest answer for painting once the conversation pauses.
    ///
    /// A new turn replaces the queued text, so three quick questions produce one
    /// refresh of the last answer rather than three refreshes, two of which would
    /// be obsolete before the panel finished flashing.
    fn paint_later(&mut self, heard: &str, answer: Option<&str>) -> Result<(), String> {
        self.poll(false);
        match &self.inner {
            Ok(_) => {
                self.pending = Some(PaintJob::Turn {
                    heard: heard.to_owned(),
                    answer: answer.map(str::to_owned),
                });
                Ok(())
            }
            Err(error) => Err(error.clone()),
        }
    }

    /// True when the panel is idle and can be borrowed without waiting.
    fn is_ready(&self) -> bool {
        matches!(self.inner, Ok(PainterState::Ready(_)))
    }

    /// Refresh what the frame already holds, without blocking the loop.
    ///
    /// The caller drew the picture with [`Painter::with_ready`] first, so the
    /// card was read by its single owner; this only does the slow part, the
    /// ~31 s panel refresh, off the loop so the microphone stays open.
    fn refresh_later(&mut self) -> Result<(), String> {
        self.pending = None;
        if !self.is_ready() {
            return Err("ERR display busy\r\n".to_owned());
        }
        let pair = self.take_ready()?;
        self.spawn(pair, PaintJob::Frame)
    }

    /// [`Painter::refresh_later`], with the outcome sent to `watcher` when the
    /// refresh ends.
    fn refresh_later_then(
        &mut self,
        watcher: mpsc::Sender<Result<String, String>>,
    ) -> Result<(), String> {
        self.refresh_later()?;
        self.watcher = Some(watcher);
        Ok(())
    }

    fn spawn(&mut self, pair: DisplayPair, job: PaintJob) -> Result<(), String> {
        let (display, frame) = pair;
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("epd-paint".to_owned())
            .stack_size(16 * 1024)
            .spawn(move || {
                let mut display = display;
                let frame = frame;
                let result = match &job {
                    PaintJob::Turn { heard, answer } => {
                        paint_turn(&mut display, &mut frame[..], heard, answer.as_deref())
                    }
                    PaintJob::Frame => {
                        let started = Instant::now();
                        display
                            .show(frame)
                            .map(|()| format!("paint_ms={}\r\n", started.elapsed().as_millis()))
                    }
                };
                let _ = tx.send(PaintDone {
                    display,
                    frame,
                    result,
                });
            });
        match spawned {
            Ok(_) => {
                self.inner = Ok(PainterState::Painting(rx));
                Ok(())
            }
            Err(error) => {
                let message = format!("ERR display paint thread: {error}\r\n");
                self.inner = Err(message.clone());
                Err(message)
            }
        }
    }
}

/// Apply the saved `ai.idle` and `ai.threshold`. A wall-mounted frame has no
/// terminal at reset, so the mode is persisted and applied at boot. Key is
/// silent because it needs nothing opened; voice says so, since it powers the
/// microphone and the ALDO3 rail; a failure is always spoken rather than
/// silently ignored. The gate comes first, so the first monitor start already
/// uses the level this room was tuned for. Returns the line to print, if any.
fn restore_idle(
    idle: &mut Idle,
    settings: &mut config::Settings,
    audio_state: &mut Result<audio::Audio, String>,
    i2c: &mut I2cDriver<'_>,
) -> Option<String> {
    if let Some(threshold) = settings.idle_threshold() {
        idle.detector.set_threshold(threshold);
    }
    let mode = settings.ai_idle?;
    let reply = set_idle_mode(idle, mode, audio_state, i2c);
    // If voice could not open the microphone the mode reverted to off; keep
    // the settings object honest so a later `config save` cannot write a mode
    // the hardware refused.
    settings.ai_idle = Some(idle.mode);
    let voice_opened = mode == console::IdleMode::Voice && idle.mode == console::IdleMode::Voice;
    (voice_opened || (mode != console::IdleMode::Off && !reply.starts_with("OK"))).then_some(reply)
}

/// A timer or BOOT wake: start the slideshow from `gallery.dir`, aimed at the
/// picture after the one shown before the sleep, and show it.
fn continue_slideshow(
    gallery: &mut gallery::Gallery,
    painter: &mut Painter,
    storage_state: &Result<storage::Storage, String>,
    settings: &config::Settings,
) -> String {
    let started = gallery_start(gallery, storage_state, settings, None, None);
    if !gallery.is_running() {
        return started;
    }
    if let Some(last) = sleep::last_shown()
        && let Ok(storage) = storage_state
        && let Ok(images) = storage.images(Some(gallery.dir()))
    {
        let focus = gallery::after(&images, &last).map(str::to_owned);
        gallery.replace(images, focus.as_deref());
    }
    next_picture(gallery, painter, storage_state, settings)
}

/// What a timed wake got from the bridge.
enum Pulled {
    /// A new picture, refreshing now.
    Shown(String),
    /// The panel already shows the bridge's picture: no refresh at all.
    Unchanged(String),
    /// Nothing from the bridge (204, no link, an error): use the card.
    Fallback(String),
}

struct PullResult {
    outcome: Pulled,
    /// The radio, kept so the sleep path turns it off.
    net: Option<net::Net<'static>>,
}

/// A timed wake with `bridge.url`: join, ask `GET /frames/<id>/next` with the
/// ETag of what the panel shows, and paint a new picture if there is one. The
/// link costs a few seconds of radio per wake, which is why this is opt-in.
fn pull_from_bridge(
    modem: &mut Option<esp_idf_svc::hal::modem::Modem<'static>>,
    sys_loop: &esp_idf_svc::eventloop::EspSystemEventLoop,
    i2c: &mut I2cDriver<'_>,
    settings: &config::Settings,
    painter: &mut Painter,
) -> PullResult {
    let (net, joined) = join_at_boot(modem, sys_loop, i2c, settings);
    let fallback = |note: String| Pulled::Fallback(format!("{}\r\n", note.trim_end()));
    let Some((_, mac)) = net.as_ref().and_then(net::Net::address) else {
        return PullResult {
            outcome: fallback(format!("bridge pull: no link ({joined})")),
            net,
        };
    };
    let base = settings
        .pull_url
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/');
    let url = format!("{base}/frames/{}/next", opendisplay::frame_id(mac));
    let authorization = settings
        .bridge_token()
        .map(|token| format!("Bearer {token}"));
    let if_none_match = sleep::bridge_etag().map(|etag| format!("\"{etag:016x}\""));
    let mut headers = Vec::new();
    if let Some(value) = &authorization {
        headers.push(("Authorization", value.as_str()));
    }
    if let Some(value) = &if_none_match {
        headers.push(("If-None-Match", value.as_str()));
    }
    let outcome = match http::get(&url, &headers, panel::buffer_len() + 1) {
        Ok(reply) if reply.status == 304 => {
            Pulled::Unchanged("OK bridge pull: unchanged, no refresh\r\n".to_owned())
        }
        Ok(reply) if reply.status == 200 && reply.body.len() == panel::buffer_len() => {
            let copied = painter.with_ready(|_, frame| frame.copy_from_slice(&reply.body));
            match copied.and_then(|()| painter.refresh_later()) {
                Ok(()) => {
                    let etag = reply
                        .etag
                        .as_deref()
                        .map(|etag| etag.trim_matches('"'))
                        .and_then(|hex| u64::from_str_radix(hex, 16).ok());
                    sleep::set_bridge_etag(etag);
                    Pulled::Shown(format!(
                        "OK bridge pull: new picture etag={etag:016x?} paint=started\r\n"
                    ))
                }
                Err(error) => fallback(format!("bridge pull: {error}")),
            }
        }
        Ok(reply) if reply.status == 204 => {
            fallback("bridge pull: nothing queued for this frame".to_owned())
        }
        Ok(reply) => fallback(format!(
            "bridge pull: HTTP {} {}",
            reply.status,
            reply.text()
        )),
        Err(error) => fallback(format!("bridge pull: {error}")),
    };
    PullResult { outcome, net }
}

/// `wifi.auto=1` at boot: join with the card's credentials and stay online, so
/// later stages (portal, OpenDisplay, the assistant) find a link instead of
/// having to bring the radio up themselves. The only other thing it does is heal
/// a dead clock: if the RTC has no valid time, one SNTP sync runs while the link
/// is up. Returns the radio, if it was created, and a line for `info`.
fn join_at_boot(
    modem: &mut Option<esp_idf_svc::hal::modem::Modem<'static>>,
    sys_loop: &esp_idf_svc::eventloop::EspSystemEventLoop,
    i2c: &mut I2cDriver<'_>,
    settings: &config::Settings,
) -> (Option<net::Net<'static>>, String) {
    let mut net_state = None;
    let wifi_auto;
    let needs_time = rtc_needs_sync(i2c);
    match settings
        .credentials()
        .map(|(ssid, password)| (ssid.to_owned(), password.to_owned()))
    {
        None => wifi_auto = "wifi_auto=skipped (no credentials)".to_owned(),
        Some((ssid, password)) => match modem.take() {
            None => wifi_auto = "wifi_auto=skipped (radio unavailable)".to_owned(),
            Some(radio) => match net::Net::new(radio, sys_loop.clone()) {
                Err(error) => wifi_auto = format!("wifi_auto={}", error.trim()),
                Ok(mut net) => {
                    wifi_auto = match net.join(&ssid, &password) {
                        // A failed join has nothing to serve, so do not leave
                        // the radio burning current on an absent access point.
                        Err(error) => {
                            let _ = net.off();
                            format!("wifi_auto={}", error.trim())
                        }
                        Ok(_) => {
                            let time = if needs_time {
                                wifi_time(&mut net, i2c, None)
                                    .map(|reply| format!("time={}", reply.trim()))
                                    .unwrap_or_else(|error| format!("time={}", error.trim()))
                            } else {
                                "time=rtc_valid".to_owned()
                            };
                            format!("wifi_auto=connected {time}")
                        }
                    };
                    // Kept either way: the driver is initialized, so a later
                    // `wifi` command reuses it instead of failing.
                    net_state = Some(net);
                }
            },
        },
    }
    (net_state, wifi_auto)
}

fn run() -> Result<(), EspError> {
    // First, before anything resets the sleep controller's latched cause.
    let wake = sleep::wake();
    let p = Peripherals::take()?;
    let mut usb = UsbSerialDriver::new(
        p.usb_serial,
        p.pins.gpio19,
        p.pins.gpio20,
        &UsbSerialConfig::new()
            .tx_buffer_size(1024)
            .rx_buffer_size(256),
    )?;
    // V2 schematic: KEY=4 and BOOT=0 read low when pressed. PWR goes to the
    // PMIC's PWRON and, through a transistor, to GPIO5, which reads high when
    // pressed (the vendor's `button_bsp.c` agrees); only GPIO5 is read here, the
    // PMIC is not touched.
    let key = PinDriver::input(p.pins.gpio4, Pull::Up)?;
    let boot = PinDriver::input(p.pins.gpio0, Pull::Up)?;
    let pwr = PinDriver::input(p.pins.gpio5, Pull::Up)?;
    // Both lights are active low. Red (GPIO45) is the vendor's "operable" light;
    // GPIO45 is the VDD_SPI strap, but the eFuses force VDD_SPI to 3.3 V, so the
    // strap is ignored and driving it is safe. Green (GPIO42) blinks while the
    // panel refreshes and otherwise follows `led on|off`.
    let mut green = PinDriver::output(p.pins.gpio42)?;
    green.set_high()?;
    let mut red = PinDriver::output(p.pins.gpio45)?;
    red.set_high()?;
    // Shared V2 bus: PMIC, SHTC3 and audio codec all sit on GPIO47/48.
    let mut i2c = I2cDriver::new(
        p.i2c0,
        p.pins.gpio47,
        p.pins.gpio48,
        &I2cConfig::new().baudrate(KiloHertz(100).into()),
    )?;
    let required_rails = configure_display_rail(&mut i2c);
    if let Err(error) = &required_rails {
        send(&mut usb, error)?;
    }
    // ALDO3 is audio-only, so it is brought up with the codecs rather than in
    // the display boot path above. The report is kept for `info` and
    // `audio status` so every rail write is visible instead of implicit.
    let audio_rail = ensure_audio_rail(&mut i2c);
    if let Err(error) = &audio_rail {
        send(&mut usb, error)?;
    }
    let audio_rail = audio_rail.unwrap_or_else(|error| error);
    // Panel pins from the schematic and vendor factory test. Build lazily at
    // command time so a missing display cannot stop the USB console.
    let mut painter = Painter::new(
        display::build(
            p.spi2,
            p.pins.gpio10,
            p.pins.gpio11,
            p.pins.gpio9,
            p.pins.gpio8,
            p.pins.gpio12,
            p.pins.gpio13,
        )
        .and_then(|display| display::alloc_buffer().map(|frame| (display, frame))),
    );
    // Vendor V2 slot: CLK 39, CMD 41, D0..D3 40,1,2,38. Probing a missing
    // card only reports an error; the console and display stay available.
    let storage_state = storage::Storage::mount(
        p.sdmmc1,
        p.pins.gpio41,
        p.pins.gpio39,
        p.pins.gpio40,
        p.pins.gpio1,
        p.pins.gpio2,
        p.pins.gpio38,
    );
    // Settings live on the card because this firmware has no key/value store:
    // NVS is never initialised and the radio is built without flash storage, so
    // RAM-only settings die at every reset - and the S3 USB-Serial/JTAG resets
    // the chip whenever a host opens or closes the port. A missing or unparsable
    // file is reported, never fatal, and the defaults stay in force.
    let settings_path = Path::new("/sdcard").join(config::FILE);
    let mut settings = config::Settings::default();
    let mut settings_state = "absent";
    let mut settings_secret = false;
    let settings_note;
    match fs::read_to_string(&settings_path) {
        Ok(text) => match config::Settings::parse(&text) {
            Ok(parsed) => {
                settings = parsed;
                settings_state = "loaded";
                // Report whether the passphrase is sitting on this card in the
                // clear, without ever printing it.
                settings_secret = text.lines().any(|line| {
                    line.trim()
                        .to_ascii_lowercase()
                        .starts_with("wifi.password")
                });
                settings_note = format!(
                    "OK config loaded path={} bytes={} password_on_disk={}\r\n",
                    settings_path.display(),
                    text.len(),
                    if settings_secret { "yes" } else { "no" }
                );
            }
            Err(error) => {
                settings_state = "invalid";
                settings_note = error;
            }
        },
        Err(error) => {
            settings_note = format!("config absent at {}: {error}\r\n", settings_path.display());
        }
    }
    if settings_state == "invalid" {
        send(&mut usb, &settings_note)?;
    }
    // Audio is initialized once but the ADC, DAC, clock outputs and PA stay
    // off until an audio command. A missing codec cannot stop the console.
    let mut audio_state = audio::Audio::new(
        &mut i2c,
        p.i2s0,
        p.pins.gpio14,
        p.pins.gpio15,
        p.pins.gpio16,
        p.pins.gpio17,
        p.pins.gpio18,
        p.pins.gpio7,
    );

    // Settings-file levels are only remembered here: `start()` applies them, so
    // a corrupt file can never write to an idle codec at boot.
    let audio_defaults = match &mut audio_state {
        Ok(audio) => audio
            .set_defaults(settings.volume_tenths, settings.gain_tenths)
            .map_or_else(|error| error, |()| "applied".to_owned()),
        Err(error) => error.clone(),
    };

    // The radio is created on the first `wifi` command: an unused network stack
    // costs nothing, and credentials live only as long as this object.
    let sys_loop = esp_idf_svc::eventloop::EspSystemEventLoop::take()?;
    let mut modem = Some(p.modem);
    let mut net_state: Option<net::Net<'static>> = None;

    // `wifi.auto=1` joins at boot and stays online, so later stages (image
    // sync, remote control) find a link instead of having to bring the radio up
    // themselves. The only thing it also does is heal a dead clock: if the RTC
    // has no valid time, one SNTP sync runs while the link is already up. The
    // radio then stays connected, at the cost of its idle current.
    // A timer or BOOT wake only shows the next picture and sleeps again, so it
    // leaves the radio off; KEY during that wake brings it up after all.
    let mut wifi_auto = "wifi_auto=off".to_owned();
    if settings.wifi_auto && !wake.shows_picture() {
        let (joined, report) = join_at_boot(&mut modem, &sys_loop, &mut i2c, &settings);
        net_state = joined;
        wifi_auto = report;
    }

    let mut idle = Idle::new();
    // A wall-mounted frame has no terminal at reset, so `ai.idle` is persisted:
    // boot applies the saved mode. Key is silent because it needs nothing
    // opened; voice says so, since it powers the microphone and the ALDO3 rail;
    // a failure is always spoken rather than silently ignored.
    // The saved gate is applied before the mode, so the first monitor start
    // already uses the level this room was tuned for.
    // A picture wake keeps the microphone closed: it sleeps again in seconds.
    if !wake.shows_picture()
        && let Some(reply) = restore_idle(&mut idle, &mut settings, &mut audio_state, &mut i2c)
    {
        send(&mut usb, &reply)?;
    }
    // The slideshow is off unless the file asks for it: a card full of pictures
    // must not start flashing the panel by itself the first time this build
    // boots on somebody else's frame. A picture wake is the slideshow itself,
    // continuing after the picture it showed before sleeping.
    let mut gallery = gallery::Gallery::stopped();
    // Battery, ambient and chip temperature, read now so a picture wake can
    // decline to paint on an empty cell, and refreshed every SENSOR_PERIOD.
    let chip = ChipTemperature::new();
    let mut sensors = Sensors::default();
    sensors.read(&mut i2c, &chip);
    opendisplay::set_sleep_secs(settings.sleep_secs());
    let battery_low = sensors.battery.is_some_and(|battery| battery.low());
    if wake.shows_picture() && !battery_low {
        // A timer wake with `bridge.url` asks the bridge first; BOOT always means
        // "the card's next picture".
        let pulled = if wake == power::Wake::Timer && settings.pull_url.is_some() {
            let pulled = pull_from_bridge(&mut modem, &sys_loop, &mut i2c, &settings, &mut painter);
            net_state = pulled.net;
            Some(pulled.outcome)
        } else {
            None
        };
        match pulled {
            Some(Pulled::Shown(note) | Pulled::Unchanged(note)) => send(&mut usb, &note)?,
            Some(Pulled::Fallback(note)) => {
                send(&mut usb, &note)?;
                let reply =
                    continue_slideshow(&mut gallery, &mut painter, &storage_state, &settings);
                send(&mut usb, &reply)?;
            }
            None => {
                let reply =
                    continue_slideshow(&mut gallery, &mut painter, &storage_state, &settings);
                send(&mut usb, &reply)?;
            }
        }
    } else if wake.shows_picture() {
        // An empty cell: no refresh. The loop below sleeps on buttons only.
    } else if settings.gallery_auto() {
        let reply = gallery_start(&mut gallery, &storage_state, &settings, None, None);
        if !reply.starts_with("OK") {
            send(&mut usb, &reply)?;
        }
    }
    // The portal is a listener on the same radio as the assistant. `web stop`
    // turns it off for this session; `web start` brings it back. It is not a
    // settings key because a frame that is on the network should be reachable
    // without editing a file - what it exposes is decided by `web.password`.
    let mut web_state: Option<(web::Server, mpsc::Receiver<web::Job>)> = None;
    // OpenDisplay (TCP 2446) comes up and goes down with the web portal's link.
    let mut opendisplay_state: Option<(opendisplay::Handle, mpsc::Receiver<opendisplay::Job>)> =
        None;
    let mut web_enabled = true;
    // One attempt per network event: a listener that cannot bind is reported
    // once, not once per 20 ms loop turn.
    let mut web_attempted = false;
    let mut line = LineBuffer::default();
    let mut input = [0_u8; 64];
    let mut controls = Controls::new(Instant::now());
    // A timer or BOOT wake shows one picture and sleeps; anything a person does
    // during it (KEY, the console) turns it into an ordinary awake boot.
    let mut picture_wake = wake.shows_picture();
    // Last time somebody or something used the frame; sleep waits for quiet.
    let mut last_activity = Instant::now();
    // Set anywhere in the loop, carried out at the top of the next turn, where
    // everything the shutdown needs is in scope and nothing is half done.
    let mut sleep_request: Option<u32> = None;
    // A press that cancelled a picture wake's sleep, handled as a person below.
    let mut wake_up = false;
    // The low-battery sleep is requested once per boot; a button wake on a still
    // empty cell gets a few seconds of use (the console) before the next one.
    let mut low_handled = false;
    // Everything above has run: the frame is operable.
    red.set_low()?;
    loop {
        if let Some(timer) = sleep_request.take() {
            // The refresh in progress finishes first: cutting the panel's power
            // mid-waveform leaves a half-drawn picture for the whole sleep. A
            // queued paint (an answer, the low-battery notice) is started now.
            painter.poll(true);
            let deadline = Instant::now() + Duration::from_secs(60);
            while !painter.is_ready() && Instant::now() < deadline {
                painter.poll(true);
                std::thread::sleep(Duration::from_millis(100));
            }
            // KEY or PWR down during a picture wake is a person who wants the
            // frame: the wake lasts only seconds, too short for a click to
            // finish, so the press itself cancels the sleep and wakes it fully.
            if picture_wake && (key.is_low() || pwr.is_high()) {
                wake_up = true;
                last_activity = Instant::now();
                continue;
            }
            // A button still in its wake state would wake the chip at once (the
            // vendor waits for BOOT the same way).
            let deadline = Instant::now() + Duration::from_secs(10);
            let held = || key.is_low() || boot.is_low() || pwr.is_high();
            while held() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if held() {
                send(
                    &mut usb,
                    "ERR sleep: a button is still held; staying awake\r\n",
                )?;
                last_activity = Instant::now();
                continue;
            }
            send(
                &mut usb,
                &format!(
                    "OK sleep timer_s={timer} wake=boot,key,pwr{}\r\n",
                    if timer > 0 { ",timer" } else { "" }
                ),
            )?;
            // Servers first, so the mDNS goodbye goes out while the radio is up.
            web_state = None;
            opendisplay_state = None;
            std::thread::sleep(Duration::from_millis(300));
            if let Some(net) = net_state.as_mut() {
                let _ = net.off();
            }
            let _ = set_idle_mode(
                &mut idle,
                console::IdleMode::Off,
                &mut audio_state,
                &mut i2c,
            );
            if let Ok(audio) = &mut audio_state {
                let _ = audio.shutdown(&mut i2c);
            }
            let _ = painter.with_ready(|display, _| display.sleep());
            green.set_high()?;
            red.set_high()?;
            match sleep::enter(timer) {
                Ok(never) => match never {},
                Err(error) => {
                    red.set_low()?;
                    send(&mut usb, &format!("ERR sleep: {error}\r\n"))?;
                    last_activity = Instant::now();
                }
            }
        }
        // Bind the listener the first time there is a link. The radio decides:
        // an address is only meaningful when it is connected, so the attempt is
        // made from inside the loop rather than at boot.
        if web_enabled && !web_attempted && net_state.is_some() {
            web_attempted = true;
            let online = match net_state.as_mut() {
                Some(net) => ensure_online(net, &settings).is_ok(),
                None => false,
            };
            if online {
                match start_web(&mut web_state, &settings) {
                    Ok(reply) => send(&mut usb, &reply)?,
                    Err(error) => send(&mut usb, &error)?,
                }
                if opendisplay_state.is_none() {
                    // mDNS discovery needs the radio awake to hear multicast;
                    // it stays awake until the radio is turned off.
                    if let Some(Err(error)) =
                        net_state.as_mut().map(|net| net.set_power_save(false))
                    {
                        send(&mut usb, &error)?;
                    }
                    let identity = net_state.as_ref().and_then(|net| net.address());
                    match start_opendisplay(&mut opendisplay_state, identity) {
                        Ok(reply) => send(&mut usb, &reply)?,
                        Err(error) => send(&mut usb, &error)?,
                    }
                }
            }
        }
        let count = usb.read(&mut input, TickType::new_millis(20).ticks())?;
        // Guard the HAL's signed-to-unsigned read result before slicing.
        if count > input.len() {
            return Err(EspError::from_infallible::<{ sys::ESP_FAIL }>());
        }
        let mut person = count > 0 || std::mem::take(&mut wake_up);
        if person {
            last_activity = Instant::now();
        }
        for &byte in &input[..count] {
            let Some(command) = line.push(byte) else {
                continue;
            };
            // A command may wait for KEY itself (`ai listen`); that press is
            // the command's, and its release must not start a second turn.
            controls.key.disarm();
            match command {
                Command::Help => send(
                    &mut usb,
                    "OK help info buttons led on|off sensor rtc pmu sd\r\nsleep [0|60..86400] (deep sleep now; BOOT/KEY/PWR wake it, and the timer if non-zero; default power.sleep_secs)\r\nsd ls [<dir>]\r\nconfig|config save (settings on SD)\r\ngallery|status|start [<dir> [120..3600s]]|stop|next (slideshow over .bmp/.pf6 in one directory; `config save` persists gallery.dir/secs/auto)\r\nweb|status|start|stop|password <8..64 chars>|password off (page on port 80: upload, gallery as rendered with removal, settings; only PFTEST may be written or emptied; the password locks the whole page and is saved at once)\r\ndisplay bars|clear black|white|yellow|red|blue|green|sleep|status\r\ndisplay bmp <file.bmp>|raw <file.pf6>\r\naudio status|regs|level <ms>|rec <1..10s> (double-beep cue)|tone [100..2000ms]|loop [100..2000ms]|hex|volume <db>|gain <db>|play <file.wav>|off\r\nwifi scan|status|off|auto on|off|join [<ssid> [password]]|time [ntp server]\r\nai|status|selftest|stt [ms] (double-beep cue, records, posts to /stt)|listen [ms] (waits for KEY, then as `ai stt`, prints and draws the transcript)|talk [ms] (KEY, then stt, llm, tts, speech, panel)|ask <text> (posts to /llm then /tts, plays the answer)|url <http://host:port>|key <token> (`-` clears; `config save` persists)\r\nai idle off|key|voice (who may start a turn: nobody, KEY, or loudness on the microphones; `config save` persists)\r\nai threshold <20..4000> (voice gate in sample units; read `last_rms` in `ai status` first)\r\nI2C reads, ALDO4 display rail, ALDO3 audio rail only when audio is used. SD read-only except PFTEST. Radio only for wifi commands. `wifi join` with no arguments reuses PFTEST/CONFIG.TXT; that file is loaded at boot and holds the passphrase once `config save` runs; `wifi.auto=1` joins at boot and stays online.\r\n",
                )?,
                Command::Info => {
                    // SAFETY: These IDF heap queries accept capability masks, require
                    // no pointers, and are safe after IDF has initialized the heap.
                    let (psram, free) = unsafe {
                        (
                            sys::heap_caps_get_total_size(sys::MALLOC_CAP_SPIRAM),
                            sys::heap_caps_get_free_size(sys::MALLOC_CAP_SPIRAM),
                        )
                    };
                    // SAFETY: as above; a null task handle means the calling task,
                    // and IDF reports the high-water mark in bytes.
                    let (internal_free, internal_min, main_stack_min) = unsafe {
                        (
                            sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL),
                            sys::heap_caps_get_minimum_free_size(sys::MALLOC_CAP_INTERNAL),
                            sys::uxTaskGetStackHighWaterMark(core::ptr::null_mut()),
                        )
                    };
                    send(
                        &mut usb,
                        &format!(
                            "OK photoframe {} Rev2.0 target=ESP32-S3 psram_total={} psram_free={}\r\n  internal_free={internal_free} internal_min_free={internal_min} main_stack_min_free={main_stack_min}\r\n  wake={} wakes={} sleep_secs={} awake_secs={} idle_s={} picture_wake={picture_wake}\r\nPMIC ALDO4 EPD rail configured to 3.3 V; other rails untouched. PSRAM startup test configured.\r\n{audio_rail}{settings_note}  source={settings_state} path={} password_on_disk={} {wifi_auto} audio_defaults={audio_defaults}\r\n",
                            env!("CARGO_PKG_VERSION"),
                            psram,
                            free,
                            wake.name(),
                            sleep::wakes(),
                            settings.sleep_secs(),
                            settings.awake_secs().as_secs(),
                            last_activity.elapsed().as_secs(),
                            settings_path.display(),
                            if settings_secret { "yes" } else { "no" }
                        ),
                    )?;
                }
                Command::Buttons => send(
                    &mut usb,
                    &format!(
                        "OK key_pressed={} boot_pressed={} pwr_pressed={}\r\n",
                        key.is_low(),
                        boot.is_low(),
                        pwr.is_high()
                    ),
                )?,
                Command::LedOn => {
                    controls.green = true;
                    green.set_low()?;
                    send(&mut usb, "OK green on\r\n")?;
                }
                Command::LedOff => {
                    controls.green = false;
                    green.set_high()?;
                    send(&mut usb, "OK green off\r\n")?;
                }
                Command::Sensor => {
                    let reply = sensor(&mut i2c).unwrap_or_else(|error| error);
                    send(&mut usb, &reply)?;
                }
                Command::Rtc => {
                    let reply = rtc(&mut i2c).unwrap_or_else(|error| error);
                    send(&mut usb, &reply)?;
                }
                Command::Pmu => {
                    let reply = pmu(&mut i2c).unwrap_or_else(|error| error);
                    send(&mut usb, &reply)?;
                }
                Command::Sd => {
                    let reply = match &storage_state {
                        Ok(storage) => storage.report().unwrap_or_else(|error| error),
                        Err(error) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::SdList(path) => {
                    // An empty path is a bare `sd ls`: the card root, which is
                    // where the slideshow directories are listed from.
                    let relative = (!path.is_empty()).then_some(path.as_str());
                    let reply = match &storage_state {
                        Ok(storage) => storage.list(relative).unwrap_or_else(|error| error),
                        Err(error) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::Sleep(secs) => {
                    let timer = secs.unwrap_or_else(|| settings.sleep_secs());
                    if timer == 0
                        || (config::MIN_SLEEP_SECS..=config::MAX_SLEEP_SECS).contains(&timer)
                    {
                        sleep_request = Some(timer);
                    } else {
                        send(
                            &mut usb,
                            &format!(
                                "ERR sleep takes 0 or {}..={} seconds\r\n",
                                config::MIN_SLEEP_SECS,
                                config::MAX_SLEEP_SECS
                            ),
                        )?;
                    }
                }
                Command::GalleryStatus => send(&mut usb, &gallery.status())?,
                Command::GalleryStop => {
                    gallery.stop();
                    send(&mut usb, "OK gallery=off\r\n")?;
                }
                Command::GalleryStart { dir, seconds } => {
                    let reply = gallery_start(
                        &mut gallery,
                        &storage_state,
                        &settings,
                        dir.as_deref(),
                        seconds,
                    );
                    send(&mut usb, &reply)?;
                }
                Command::GalleryNext => {
                    let reply = match gallery.take_next(Instant::now()) {
                        None => "ERR gallery=off; start it with `gallery start`\r\n".to_owned(),
                        Some(path) => {
                            match painter.with_ready(|display, frame| {
                                match image::load_any(&path, frame) {
                                    Ok(()) => {
                                        gallery.record_advance();
                                        reply(
                                            display.show(frame),
                                            &format!("gallery next path={path}"),
                                        )
                                    }
                                    // Name the file: a silent `next` that fails
                                    // must say which entry on the card it choked
                                    // on, exactly as the loop's own failures do.
                                    Err(error) => {
                                        format!("ERR gallery {path}: {}", error.trim_end())
                                    }
                                }
                            }) {
                                Ok(text) => text,
                                Err(error) => error,
                            }
                        }
                    };
                    send(&mut usb, &reply)?;
                }
                Command::WebStatus => {
                    let reply = match &web_state {
                        Some((server, _)) => format!(
                            "OK web listening port={} locked={} auth={} route=/\r\n",
                            server.port(),
                            u8::from(server.locked()),
                            if server.locked() { "basic" } else { "none" }
                        ),
                        None => format!(
                            "ERR web=off enabled={} (start it with `web start`)\r\n",
                            u8::from(web_enabled)
                        ),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::WebStart => {
                    web_enabled = true;
                    web_attempted = false;
                    send(&mut usb, "OK web will start on the next loop\r\n")?;
                }
                Command::WebStop => {
                    web_enabled = false;
                    web_attempted = true;
                    let reply = match web_state.take() {
                        Some((server, _)) => {
                            drop(server);
                            "OK web=off\r\n"
                        }
                        None => "OK web=off (it was not listening)\r\n",
                    };
                    send(&mut usb, reply)?;
                }
                Command::WebPassword(value) => {
                    let reply = match settings.set_web_password(value.as_deref().unwrap_or("")) {
                        Err(error) => error,
                        Ok(message) => match save_settings(&settings) {
                            Err(error) => error,
                            // Drop the listener rather than leaving the old lock
                            // in force: the loop rebinds with the new password at
                            // once, so there is no window where the console and
                            // the page disagree.
                            Ok(_) => {
                                if web_state.take().is_some() {
                                    web_attempted = false;
                                }
                                let line = message.trim_end();
                                format!("{line}; saved on the card\r\n")
                            }
                        },
                    };
                    send(&mut usb, &reply)?;
                }
                Command::DisplayBars => {
                    let reply = match painter.with_ready(|display, frame| {
                        panel::bars(frame);
                        reply(
                            display.show(frame),
                            "display bars left-to-right=black,yellow,red,blue,green,white",
                        )
                    }) {
                        Ok(text) => text,
                        Err(error) => error,
                    };
                    send(&mut usb, &reply)?;
                }
                Command::DisplayBmp(path) => {
                    let reply = match &storage_state {
                        Err(error) => error.clone(),
                        Ok(_) => painter
                            .with_ready(|display, frame| {
                                match image::resolve(&path)
                                    .and_then(|path| image::load_bmp(&path, frame))
                                {
                                    Ok(()) => reply(
                                        display.show(frame),
                                        &format!("display bmp path={path}"),
                                    ),
                                    Err(error) => error,
                                }
                            })
                            .unwrap_or_else(|error| error),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::DisplayRaw(path) => {
                    let reply = match &storage_state {
                        Err(error) => error.clone(),
                        Ok(_) => painter
                            .with_ready(|display, frame| {
                                match image::resolve(&path)
                                    .and_then(|path| image::load_raw(&path, frame))
                                {
                                    Ok(()) => reply(
                                        display.show(frame),
                                        &format!("display raw path={path}"),
                                    ),
                                    Err(error) => error,
                                }
                            })
                            .unwrap_or_else(|error| error),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::DisplayClear(color) => {
                    let reply = painter
                        .with_ready(|display, frame| {
                            panel::fill(frame, color);
                            reply(display.show(frame), &format!("display clear color={color}"))
                        })
                        .unwrap_or_else(|error| error);
                    send(&mut usb, &reply)?;
                }
                Command::DisplaySleep => {
                    let reply = painter
                        .with_ready(|display, _| reply(display.sleep(), "display asleep"))
                        .unwrap_or_else(|error| error);
                    send(&mut usb, &reply)?;
                }
                Command::DisplayStatus => {
                    let reply = painter
                        .with_ready(|display, _| {
                            format!(
                                "OK display_awake={} busy={}\r\n",
                                display.awake(),
                                display.busy()
                            )
                        })
                        .unwrap_or_else(|error| error);
                    send(&mut usb, &reply)?;
                }
                Command::AudioStatus => {
                    let reply = match &audio_state {
                        Ok(audio) => format!("{}{audio_rail}", audio.status()),
                        Err(error) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AudioRegs => {
                    let reply = match &mut audio_state {
                        Ok(audio) => audio.regs(&mut i2c).unwrap_or_else(|error| error),
                        Err(error) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AudioVolume(tenths) => {
                    let reply = match &mut audio_state {
                        Ok(audio) => audio
                            .set_volume(&mut i2c, tenths)
                            .unwrap_or_else(|error| error),
                        Err(error) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AudioGain(tenths) => {
                    let reply = match &mut audio_state {
                        Ok(audio) => audio
                            .set_gain(&mut i2c, tenths)
                            .unwrap_or_else(|error| error),
                        Err(error) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AudioLevel(ms) => {
                    let reply = match &mut audio_state {
                        Ok(audio) => audio.level(&mut i2c, ms).unwrap_or_else(|error| error),
                        Err(error) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AudioRecord(seconds) => {
                    let reply = match (&mut audio_state, &storage_state) {
                        (Ok(audio), Ok(_)) => audio
                            .record(&mut i2c, seconds)
                            .unwrap_or_else(|error| error),
                        (Err(error), _) => error.clone(),
                        (_, Err(error)) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AudioTone(ms) => {
                    let reply = match &mut audio_state {
                        Ok(audio) => audio.tone(&mut i2c, ms).unwrap_or_else(|error| error),
                        Err(error) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AudioLoop(ms) => {
                    let reply = match &mut audio_state {
                        Ok(audio) => audio.loop_tone(&mut i2c, ms).unwrap_or_else(|error| error),
                        Err(error) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AudioHex => {
                    let reply = match &mut audio_state {
                        Ok(audio) => audio.samples(&mut i2c).unwrap_or_else(|error| error),
                        Err(error) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AudioPlay(path) => {
                    let reply = match (&mut audio_state, &storage_state) {
                        (Ok(audio), Ok(_)) => {
                            audio.play(&mut i2c, &path).unwrap_or_else(|error| error)
                        }
                        (Err(error), _) => error.clone(),
                        (_, Err(error)) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AudioOff => {
                    let reply = match &mut audio_state {
                        Ok(audio) => audio.shutdown(&mut i2c).unwrap_or_else(|error| error),
                        Err(error) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AiStatus => {
                    // The watcher's counters come first: a frame count of zero
                    // while `monitor=on` is the one reading that separates a quiet
                    // room from a microphone that is not delivering audio, and
                    // `last_rms` next to `trigger` is what makes the threshold a
                    // measurement rather than a guess.
                    let monitoring = match &audio_state {
                        Ok(audio) => {
                            if audio.monitoring() {
                                "on"
                            } else {
                                "off"
                            }
                        }
                        Err(_) => "n/a",
                    };
                    let watcher = format!(
                        "OK ai.idle={} monitor={} frames={} last_rms={} threshold={} trigger={} floor={} state={}\r\n",
                        idle.mode.name(),
                        monitoring,
                        idle.frames,
                        idle.last_frame_rms.round(),
                        idle.detector.threshold().round(),
                        idle.detector.trigger().round(),
                        idle.detector.floor().round(),
                        idle.detector.state_name(),
                    );
                    send(&mut usb, &watcher)?;
                    // Everything needed to tell "unconfigured" from "misconfigured"
                    // without ever echoing the token itself.
                    let reply = match settings.bridge_url() {
                        Some(url) => match bridge::Base::parse(url) {
                            Ok(base) => format!(
                                "OK ai bridge={} tls={} token={} lang={} voice={} model={} idle={} rate={}\r\n  \
                                 endpoints stt={} llm={} tts={}\r\n",
                                base.summary(),
                                u8::from(base.tls),
                                match settings.bridge_token() {
                                    Some(token) => format!("<set:{} chars>", token.len()),
                                    None => "<none>".to_owned(),
                                },
                                settings.lang(),
                                settings.voice(),
                                settings.model(),
                                idle.mode.name(),
                                bridge::ASSISTANT_RATE,
                                base.endpoint(bridge::STT_PATH),
                                base.endpoint(bridge::LLM_PATH),
                                base.endpoint(bridge::TTS_PATH),
                            ),
                            Err(error) => error,
                        },
                        None => format!(
                            "OK ai bridge=<none>; add ai.url=http://<host>:<port> to {}\r\n",
                            config::FILE
                        ),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AiSelftest => {
                    // Builds all three requests from the settings and decodes
                    // synthetic replies, so a bad URL, voice or event type shows up
                    // here instead of as a silent failure at the bridge.
                    let reply = match assistant_selftest(&settings) {
                        Ok(reply) => reply,
                        Err(error) => error,
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AiUrl(value) => {
                    // Validated by the settings type, so a URL the console accepts is
                    // one the next boot will load. Live only until `config save`.
                    let reply = match settings.set_ai_url(&value) {
                        Ok(reply) => reply,
                        Err(error) => error,
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AiKey(value) => {
                    // The value is never echoed: the reply carries its length only. The
                    // token is on the card after `config save`, like the passphrase.
                    let reply = match settings.set_ai_key(&value) {
                        Ok(reply) => reply,
                        Err(error) => error,
                    };
                    send(&mut usb, &reply)?;
                }
                Command::AiIdle(mode) => {
                    let reply = set_idle_mode(&mut idle, mode, &mut audio_state, &mut i2c);
                    // Live, like `ai url` and `ai key`: `config save` writes it. If
                    // voice could not open the microphone, the effective mode is
                    // off, and that is what the file should hold.
                    settings.ai_idle = Some(idle.mode);
                    send(&mut usb, &reply)?;
                }
                Command::AiThreshold(value) => {
                    let reply = match threshold_reply(&mut idle, &mut settings, value) {
                        Ok(reply) => reply,
                        Err(error) => error,
                    };
                    send(&mut usb, &reply)?;
                }
                Command::Config => {
                    let mut reply = settings.summary();
                    reply.push_str(&format!(
                        "  source={settings_state} path={} password_on_disk={} audio_defaults={audio_defaults}\r\n",
                        settings_path.display(),
                        if settings_secret { "yes" } else { "no" }
                    ));
                    send(&mut usb, &reply)?;
                }
                Command::ConfigSave => {
                    // Persist the levels actually in force, not just what the
                    // file held: `audio volume`/`audio gain` change live state.
                    if let Ok(audio) = &audio_state {
                        settings.volume_tenths = Some(tenths(audio.volume_db()));
                        settings.gain_tenths = Some(tenths(audio.gain_db()));
                    }
                    let reply = match &storage_state {
                        Ok(_) => match save_settings(&settings) {
                            Ok(reply) => {
                                // The file now holds whatever is in memory.
                                settings_secret = settings.password.is_some();
                                reply
                            }
                            Err(error) => error,
                        },
                        // No card means the mount error already explains it.
                        Err(error) => error.clone(),
                    };
                    send(&mut usb, &reply)?;
                }
                Command::WifiAuto(on) => {
                    // The flag is consumed before the command loop, so it cannot
                    // affect the session it is set in - only the next boot.
                    settings.wifi_auto = on;
                    send(
                        &mut usb,
                        &format!("OK wifi auto={} applies=next boot\r\n", u8::from(on)),
                    )?;
                }
                Command::WifiScan
                | Command::WifiJoin { .. }
                | Command::WifiStatus
                | Command::WifiTime(_)
                | Command::WifiOff
                | Command::AiStt(_)
                | Command::AiListen(_)
                | Command::AiTalk(_)
                | Command::AiAsk(_) => {
                    // `wifi off` takes the listener with it: a page that answers
                    // while the radio is down would be lying about the frame.
                    let stopping_radio = matches!(command, Command::WifiOff);
                    let reply = if net_state.is_none() {
                        match modem.take() {
                            Some(modem) => match net::Net::new(modem, sys_loop.clone()) {
                                Ok(net) => {
                                    net_state = Some(net);
                                    String::new()
                                }
                                Err(error) => error,
                            },
                            None => "ERR wifi is not available\r\n".to_owned(),
                        }
                    } else {
                        String::new()
                    };
                    let reply = match (net_state.as_mut(), reply.is_empty()) {
                        (Some(net), true) => match command {
                            Command::WifiScan => net.scan(),
                            Command::WifiJoin { ssid, password } => {
                                // `None` selects the credentials read from the card.
                                let resolved = match (ssid, password) {
                                    (Some(ssid), Some(password)) => Some((ssid, password)),
                                    _ => settings.credentials().map(|(ssid, password)| {
                                        (ssid.to_owned(), password.to_owned())
                                    }),
                                };
                                match resolved {
                                    Some((ssid, password)) => {
                                        match net.join(&ssid, &password) {
                                            Ok(reply) => {
                                                // Remember what worked so that
                                                // `config save` can persist it.
                                                settings.ssid = Some(ssid);
                                                settings.password = Some(password);
                                                // A fresh link is the moment to try
                                                // the listener again.
                                                web_attempted = false;
                                                Ok(reply)
                                            }
                                            Err(error) => Err(error),
                                        }
                                    }
                                    None => Err(
                                        "ERR no saved credentials; use wifi join <ssid> <password> then config save\r\n"
                                            .to_owned(),
                                    ),
                                }
                            }
                            Command::WifiStatus => net.status(),
                            Command::WifiTime(server) => ensure_online(net, &settings)
                                .and_then(|()| wifi_time(net, &mut i2c, server.as_deref())),
                            Command::WifiOff => net.off(),
                            Command::AiStt(ms) => ensure_online(net, &settings).and_then(|()| {
                                match &mut audio_state {
                                    Ok(audio) => {
                                        assistant_turn(audio, &mut i2c, &settings, ms, "console")
                                            .map(|turn| turn.report)
                                    }
                                    Err(error) => Err(error.clone()),
                                }
                            }),
                            // The button comes first and the radio after it: a join
                            // that has to run takes seconds, and making somebody hold
                            // the button through it would feel like a dead frame.
                            Command::AiListen(ms) => wait_for_key(&key, KEY_WAIT)
                                .and_then(|waited| {
                                    ensure_online(net, &settings)?;
                                    let turn = match &mut audio_state {
                                        Ok(audio) => {
                                            assistant_turn(audio, &mut i2c, &settings, ms, "key")?
                                        }
                                        Err(error) => return Err(error.clone()),
                                    };
                                    let painted =
                                        painter.paint_now(&turn.transcript, None)?;
                                    Ok(format!(
                                        "{}  waited_ms={waited} {painted}",
                                        turn.report.trim_end()
                                    ))
                                }),
                            Command::AiTalk(ms) => wait_for_key(&key, KEY_WAIT)
                                .and_then(|waited| {
                                    ensure_online(net, &settings)?;
                                    let turn = match &mut audio_state {
                                        Ok(audio) => {
                                            assistant_turn(audio, &mut i2c, &settings, ms, "key")?
                                        }
                                        Err(error) => return Err(error.clone()),
                                    };
                                    let answer = match &mut audio_state {
                                        Ok(audio) => assistant_answer(
                                            audio,
                                            &mut i2c,
                                            &settings,
                                            &turn.transcript,
                                        )?,
                                        Err(error) => return Err(error.clone()),
                                    };
                                    let painted = if let Some(action) = answer.action {
                                        run_action(
                                            action,
                                            &mut gallery,
                                            &mut painter,
                                            &storage_state,
                                            &settings,
                                        )
                                    } else if answer.keep_panel {
                                        "paint=plugin".to_owned()
                                    } else {
                                        painter.paint_now(&turn.transcript, Some(&answer.reply))?
                                    };
                                    Ok(format!(
                                        "{}  waited_ms={waited} {}\r\n  {}",
                                        turn.report.trim_end(),
                                        painted.trim_end(),
                                        answer.report.trim_end()
                                    ))
                                }),
                            Command::AiAsk(text) => ensure_online(net, &settings).and_then(|()| {
                                match &mut audio_state {
                                    Ok(audio) => assistant_answer(audio, &mut i2c, &settings, &text)
                                        .map(|answer| answer.report),
                                    Err(error) => Err(error.clone()),
                                }
                            }),
                            _ => unreachable!("guarded by the match arm above"),
                        }
                        .unwrap_or_else(|error| error),
                        _ => reply,
                    };
                    send(&mut usb, &reply)?;
                    if stopping_radio && let Some((server, _)) = web_state.take() {
                        drop(server);
                        send(&mut usb, "OK web stopped with the radio\r\n")?;
                    }
                    if stopping_radio && opendisplay_state.take().is_some() {
                        send(&mut usb, "OK opendisplay stopped with the radio\r\n")?;
                    }
                }
                Command::Unknown => send(&mut usb, "ERR invalid command; use help\r\n")?,
            }
        }

        // The idle watcher runs here, between console reads: the loop already
        // wakes every 20 ms, so a microphone checked from here needs no thread
        // and no second owner for the codec, the panel or the radio.
        // Requests from the portal are answered here, not on the web thread: the
        // card, the panel and the settings each have exactly one owner, and this
        // loop is it. The HTTP thread is blocked on the reply, so a slow handler
        // is a slow page rather than a race.
        let mut rebind_web = false;
        if let Some((_, jobs)) = &web_state {
            while let Ok(job) = jobs.try_recv() {
                last_activity = Instant::now();
                handle_web_job(
                    job,
                    &sensors,
                    &storage_state,
                    &mut settings,
                    &mut painter,
                    &mut gallery,
                    &mut idle,
                    &mut audio_state,
                    &mut i2c,
                    &mut rebind_web,
                );
            }
        }
        // OpenDisplay requests, answered here for the same reason.
        if let Some((_, jobs)) = &opendisplay_state {
            while let Ok(job) = jobs.try_recv() {
                last_activity = Instant::now();
                if let Some(secs) = handle_opendisplay_job(job, &mut painter, &settings) {
                    sleep_request = Some(secs);
                }
            }
        }
        // A password saved from the page must take hold without a console
        // session; dropping the listener here makes the loop bind again with it.
        if rebind_web && web_state.take().is_some() {
            web_attempted = false;
        }
        let now = Instant::now();
        let key_event = controls.key.update(key.is_low(), now);
        let boot_event = controls.boot.update(boot.is_low(), now);
        let pwr_event = controls.pwr.update(pwr.is_high(), now);
        match boot_event {
            Some(buttons::Event::Click) => {
                let reply = next_picture(&mut gallery, &mut painter, &storage_state, &settings);
                send(&mut usb, &reply)?;
            }
            // The vendor's BOOT long press: low power now.
            Some(buttons::Event::Hold) => sleep_request = Some(settings.sleep_secs()),
            None => {}
        }
        match pwr_event {
            Some(buttons::Event::Click) => sleep_request = Some(settings.sleep_secs()),
            // Holding on reaches the PMIC's OFFLEVEL (about 6 s) and it cuts the
            // power itself. Every card write runs to completion on this loop
            // before the next tick, so there is nothing to flush here.
            Some(buttons::Event::Hold) => {
                send(
                    &mut usb,
                    "OK pwr hold: keep holding and the PMIC powers off\r\n",
                )?;
            }
            None => {}
        }
        if key_event.is_some() || boot_event.is_some() || pwr_event.is_some() {
            last_activity = now;
        }
        // KEY or the console during a picture wake: a person wants the frame, so
        // finish the boot that the picture wake skipped. The press only wakes;
        // it does not also start a voice turn.
        let mut key_click = key_event == Some(buttons::Event::Click);
        person |= key_event.is_some();
        if picture_wake && person {
            picture_wake = false;
            key_click = false;
            if settings.wifi_auto && net_state.is_none() {
                let (joined, report) = join_at_boot(&mut modem, &sys_loop, &mut i2c, &settings);
                net_state = joined;
                wifi_auto = report;
            }
            if let Some(reply) = restore_idle(&mut idle, &mut settings, &mut audio_state, &mut i2c)
            {
                send(&mut usb, &reply)?;
            }
            send(
                &mut usb,
                &format!("OK awake after a {} wake {wifi_auto}\r\n", wake.name()),
            )?;
        } else if key_event == Some(buttons::Event::Hold) {
            send(&mut usb, "OK key hold: the mode menu arrives with P7.3\r\n")?;
        }
        let painting = !painter.is_ready();
        let lit = if painting {
            // The vendor's ACT pattern: 100 ms on, 100 ms off.
            (controls.started.elapsed().as_millis() / 100).is_multiple_of(2)
        } else {
            controls.green
        };
        if lit {
            green.set_low()?;
        } else {
            green.set_high()?;
        }
        let event = idle_poll(&mut idle, key_click, &mut audio_state, &mut i2c);
        painter.poll(idle.quiet());
        // Sleep: a picture wake once its picture is on the panel (or failed to
        // get there); an awake frame after `power.awake_secs` of nothing, never
        // under a refresh, a voice turn being spoken or an OpenDisplay client.
        let busy = !painter.is_ready()
            || !idle.quiet()
            || opendisplay_state
                .as_ref()
                .is_some_and(|(server, _)| server.connected());
        if busy {
            last_activity = Instant::now();
        }
        if sensors.due() {
            sensors.read(&mut i2c, &chip);
            opendisplay::set_sleep_secs(settings.sleep_secs());
        }
        // An empty cell on battery: say so once on the panel, then sleep until
        // a button (no timer, so the slideshow does not drain the rest).
        let battery = sensors.battery;
        if battery.is_some_and(|battery| battery.low()) && sleep_request.is_none() && !low_handled {
            low_handled = true;
            if !sleep::low_notice_shown() {
                let _ = painter.paint_later(
                    "Батарея разряжена",
                    Some("Подключите USB-кабель. Рамка проснётся от любой кнопки."),
                );
                sleep::set_low_notice_shown(true);
            }
            send(
                &mut usb,
                &format!(
                    "OK battery low mv={} percent={}: sleeping until a button\r\n",
                    battery.map_or(0, |battery| battery.mv),
                    battery.map_or(0, |battery| battery.percent)
                ),
            )?;
            sleep_request = Some(0);
        } else if battery.is_some_and(|battery| !battery.low()) {
            sleep::set_low_notice_shown(false);
        }
        if sleep_request.is_none() {
            if picture_wake {
                if painter.is_ready() {
                    sleep_request = Some(match sleep::timer_secs() {
                        0 => settings.sleep_secs(),
                        secs => secs,
                    });
                }
            } else if power::sleep_due(
                settings.sleep_secs(),
                busy,
                last_activity.elapsed(),
                settings.awake_secs(),
            ) {
                sleep_request = Some(settings.sleep_secs());
            }
        }
        // Slideshow: advance only while the room is quiet and the panel is free.
        // A refresh here is what makes the frame deaf for half a minute, so it
        // waits for the same condition a queued answer does. The picture is
        // decoded inside this loop, which keeps the card single-owner, and only
        // the refresh - the slow part - goes to the paint thread.
        if gallery.wants_advance(now, idle.quiet() && painter.is_ready())
            && let Some(path) = gallery.take_next(now)
        {
            let loaded = painter.with_ready(|_, frame| image::load_any(&path, frame));
            let outcome = match loaded {
                Ok(Ok(())) => painter.refresh_later(),
                Ok(Err(error)) => Err(format!("ERR gallery {path}: {}", error.trim())),
                Err(error) => Err(error),
            };
            match outcome {
                // The refresh owns the panel from here; count it and let the
                // next deadline stand as it was set by `take_next`.
                Ok(()) => {
                    gallery.record_advance();
                    sleep::remember_shown(&path);
                }
                Err(error) => {
                    gallery.postpone(now);
                    send(&mut usb, &format!("{}\r\n", error.trim_end()))?;
                }
            }
        }
        if !matches!(event, IdleEvent::None) {
            let started = Instant::now();
            let online = match net_state.as_mut() {
                Some(net) => ensure_online(net, &settings),
                None => Err(
                    "ERR ai.idle: radio is off; type `wifi join` or set wifi.auto=1\r\n".to_owned(),
                ),
            };
            let trigger = match &event {
                IdleEvent::Key => "key",
                IdleEvent::Voice(_) => "voice",
                _ => "failed",
            };
            let reply = online.and_then(|()| {
                let turn = match event {
                    IdleEvent::Key => match &mut audio_state {
                        Ok(audio) => {
                            assistant_turn(audio, &mut i2c, &settings, IDLE_UTTERANCE_MS, "key")
                        }
                        Err(error) => Err(error.clone()),
                    },
                    IdleEvent::Voice(utterance) => {
                        transcribe_utterance(&settings, utterance, started, "voice")
                    }
                    IdleEvent::Failed(error) => return Err(error),
                    IdleEvent::None => unreachable!("filtered above"),
                }?;
                let answer = match &mut audio_state {
                    Ok(audio) => assistant_answer(audio, &mut i2c, &settings, &turn.transcript)?,
                    Err(error) => return Err(error.clone()),
                };
                let painted = if let Some(action) = answer.action {
                    run_action(
                        action,
                        &mut gallery,
                        &mut painter,
                        &storage_state,
                        &settings,
                    )
                } else if answer.keep_panel {
                    "paint=plugin".to_owned()
                } else {
                    match painter.paint_later(&turn.transcript, Some(&answer.reply)) {
                        Ok(()) => "paint=queued".to_owned(),
                        Err(error) => return Err(error),
                    }
                };
                Ok(format!(
                    "{}  trigger={trigger} {}\r\n  {}",
                    turn.report.trim_end(),
                    painted.trim_end(),
                    answer.report.trim_end()
                ))
            });
            send(&mut usb, &reply.unwrap_or_else(|error| error))?;
            // A turn takes tens of seconds; the quiet before sleep starts after it.
            last_activity = Instant::now();
        }
        // Yield even if a host continuously supplies data.
        thread::sleep(Duration::from_millis(10));
    }
}

/// Write the PCF85063 time. This is the firmware's only RTC write: it clears the
/// oscillator-stop flag and writes seconds through years, then reads the result
/// back. No alarm, timer, clock-output or backup-charging register is touched.
/// How long a watcher-started turn listens.
///
/// Longer than the console default: nobody is watching a terminal when the
/// frame starts by itself, so the speaker gets the time a question actually
/// takes rather than the time a typed command implies.
const IDLE_UTTERANCE_MS: u32 = 4_000;

/// Wait this long after the last voice before refreshing the panel. A Spectra 6
/// refresh costs ~31 s, so starting one between two quick questions guarantees
/// that the panel is always showing an answer older than the one being spoken.
const PAINT_QUIET: Duration = Duration::from_secs(15);

/// What made the idle watcher act.
#[derive(Debug)]
enum IdleEvent {
    /// Nothing happened this tick.
    None,
    /// KEY was pressed; the audio has not been captured yet, and the cue beep is
    /// the right feedback because the press *is* the request.
    Key,
    /// The detector assembled an utterance from the always-listening stream.
    Voice(audio::Utterance),
    /// The watcher turned itself off and would like to say why.
    Failed(String),
}

/// The three buttons and the green light's resting state, owned by the loop.
struct Controls {
    key: buttons::Button,
    boot: buttons::Button,
    pwr: buttons::Button,
    /// What `led on|off` asked for; a refresh blinks over it and then restores it.
    green: bool,
    /// Phase reference for the refresh blink.
    started: Instant,
}

impl Controls {
    fn new(now: Instant) -> Self {
        Self {
            key: buttons::Button::new(now),
            boot: buttons::Button::new(now),
            pwr: buttons::Button::new(now),
            green: false,
            started: now,
        }
    }
}

/// BOOT click: show the next picture, as the vendor's Basic mode does. A stopped
/// slideshow is started first, from `gallery.dir`, so the press always has a
/// next picture to go to; the refresh runs on the paint worker like any other.
fn next_picture(
    gallery: &mut gallery::Gallery,
    painter: &mut Painter,
    storage_state: &Result<storage::Storage, String>,
    settings: &config::Settings,
) -> String {
    if !painter.is_ready() {
        return "ERR boot: the panel is still refreshing\r\n".to_owned();
    }
    if !gallery.is_running() {
        let started = gallery_start(gallery, storage_state, settings, None, None);
        if !gallery.is_running() {
            return started;
        }
    }
    let Some(path) = gallery.take_next(Instant::now()) else {
        return "ERR boot: no picture to show\r\n".to_owned();
    };
    let loaded = painter.with_ready(|_, frame| image::load_any(&path, frame));
    let outcome = match loaded {
        Ok(Ok(())) => painter.refresh_later(),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(error),
    };
    match outcome {
        Ok(()) => {
            gallery.record_advance();
            sleep::remember_shown(&path);
            format!("OK boot next path={path} paint=started\r\n")
        }
        Err(error) => format!("ERR boot {path}: {}\r\n", error.trim()),
    }
}

/// The idle watcher: what may start a turn with nobody typing.
struct Idle {
    mode: console::IdleMode,
    detector: vad::Detector,
    /// Mono samples from a read that ended in the middle of a frame.
    carry: Vec<i16>,
    /// One frame handed to the detector, reused so a tick allocates nothing.
    frame: Vec<i16>,
    /// Earliest time the next trigger may fire; one press is one turn.
    ready_at: Option<Instant>,
    /// Last time the detector saw voice or a turn ended. A panel refresh waits
    /// until this is old enough that no follow-up question is in flight.
    last_voice: Option<Instant>,
    /// Frames the monitor has handed to the detector since the last reset.
    /// Zero while the microphone is open is the difference between "the room is
    /// quiet" and "no audio is arriving at all", which no other counter shows.
    frames: u32,
    /// Level of the most recent frame, in 16-bit sample units.
    last_frame_rms: f32,
}

impl Idle {
    fn new() -> Self {
        Self {
            mode: console::IdleMode::Off,
            detector: vad::Detector::new(pcm::SAMPLE_RATE, vad::DEFAULT_THRESHOLD),
            carry: Vec::new(),
            frame: Vec::new(),
            ready_at: None,
            last_voice: None,
            frames: 0,
            last_frame_rms: 0.0,
        }
    }

    /// True when no voice turn is being spoken and the last one is old enough.
    fn quiet(&self) -> bool {
        if self.mode != console::IdleMode::Voice {
            return true;
        }
        self.detector.state_name() == "idle"
            && self.last_voice.is_none_or(|at| at.elapsed() >= PAINT_QUIET)
    }
}

/// Apply `ai idle <mode>`, opening or closing the microphone to match.
fn set_idle_mode(
    idle: &mut Idle,
    mode: console::IdleMode,
    audio_state: &mut Result<audio::Audio, String>,
    i2c: &mut I2cDriver<'_>,
) -> String {
    if idle.mode == console::IdleMode::Voice
        && mode != console::IdleMode::Voice
        && let Ok(audio) = audio_state
    {
        let _ = audio.monitor_stop(i2c);
    }
    idle.detector.reset();
    idle.carry.clear();
    // A leftover utterance or a stale deadline cannot fire after the change.
    idle.ready_at = None;
    idle.last_voice = None;
    idle.frames = 0;
    idle.last_frame_rms = 0.0;
    idle.mode = mode;
    match mode {
        console::IdleMode::Off => "OK ai.idle=off\r\n".to_owned(),
        console::IdleMode::Key => {
            "OK ai.idle=key: any KEY press starts a turn (no console needed)\r\n".to_owned()
        }
        console::IdleMode::Voice => match audio_state {
            Ok(audio) => match audio.monitor_start(i2c) {
                Ok(()) => format!(
                    "OK ai.idle=voice threshold={} trigger={} floor={} state={} monitor_setup_ms={} note=microphone and ALDO3 stay powered\r\n",
                    idle.detector.threshold().round(),
                    idle.detector.trigger().round(),
                    idle.detector.floor().round(),
                    idle.detector.state_name(),
                    audio.monitor_setup_ms()
                ),
                Err(error) => {
                    idle.mode = console::IdleMode::Off;
                    error
                }
            },
            Err(error) => {
                idle.mode = console::IdleMode::Off;
                error.clone()
            }
        },
    }
}

/// Smallest gate the console accepts, in 16-bit sample units. Below this the
/// room itself is the signal.
const MIN_IDLE_THRESHOLD: u32 = 20;

/// Largest gate: past this a person standing at the frame is inaudible.
const MAX_IDLE_THRESHOLD: u32 = 4_000;

/// Apply `ai threshold <n>`: move the voice watcher's gate and remember it.
///
/// The value is live immediately — the point of the command is to say a sentence,
/// read `last_rms` from `ai status`, and set the gate from that number instead of
/// from a guess. `config save` writes it, like the other `ai.*` settings.
fn threshold_reply(
    idle: &mut Idle,
    settings: &mut config::Settings,
    value: u32,
) -> Result<String, String> {
    if !(MIN_IDLE_THRESHOLD..=MAX_IDLE_THRESHOLD).contains(&value) {
        return Err(format!(
            "ERR ai threshold must be {MIN_IDLE_THRESHOLD}..={MAX_IDLE_THRESHOLD}\r\n"
        ));
    }
    // The bounds above are inside `u16`, so this conversion cannot fail.
    let threshold = f32::from(u16::try_from(value).unwrap_or(u16::MAX));
    idle.detector.set_threshold(threshold);
    settings.ai_threshold = u16::try_from(value).ok();
    Ok(format!(
        "OK ai.threshold={value} trigger={} floor={} live; `config save` persists\r\n",
        idle.detector.trigger().round(),
        idle.detector.floor().round()
    ))
}

/// One watcher tick, called between console reads.
///
/// Deliberately not a thread: the codec, the panel and the radio each have one
/// owner, and the console loop already wakes every 20 ms, which is exactly the
/// cadence a monitor needs.
fn idle_poll(
    idle: &mut Idle,
    key_click: bool,
    audio_state: &mut Result<audio::Audio, String>,
    i2c: &mut I2cDriver<'_>,
) -> IdleEvent {
    match idle.mode {
        console::IdleMode::Off => IdleEvent::None,
        // A click, not a press: holding KEY is the mode menu (P7.3), so a turn
        // starts when the button comes up.
        console::IdleMode::Key => {
            if !key_click {
                return IdleEvent::None;
            }
            // A person pressing a button twice in a second wants one turn, and
            // the turn itself takes tens of seconds anyway.
            if idle.ready_at.is_some_and(|at| Instant::now() < at) {
                return IdleEvent::None;
            }
            idle.ready_at = Some(Instant::now() + Duration::from_secs(2));
            IdleEvent::Key
        }
        console::IdleMode::Voice => voice_poll(idle, audio_state, i2c),
    }
}

fn voice_poll(
    idle: &mut Idle,
    audio_state: &mut Result<audio::Audio, String>,
    i2c: &mut I2cDriver<'_>,
) -> IdleEvent {
    let audio = match audio_state {
        Ok(audio) => audio,
        Err(error) => {
            idle.mode = console::IdleMode::Off;
            return IdleEvent::Failed(error.clone());
        }
    };
    if !audio.monitoring() {
        // A turn closed the microphone before its own playback, so reopening it
        // here — after the answer, not during it — is what keeps the reply out of
        // the next utterance.
        return match audio.monitor_start(i2c) {
            Ok(()) => {
                idle.detector.reset();
                idle.carry.clear();
                // The answer has just finished; give the next question its full
                // quiet window rather than counting time spent speaking.
                idle.last_voice = Some(Instant::now());
                IdleEvent::None
            }
            Err(error) => {
                idle.mode = console::IdleMode::Off;
                IdleEvent::Failed(error)
            }
        };
    }
    let data = match audio.monitor_read() {
        Ok(data) => data,
        Err(error) => {
            idle.mode = console::IdleMode::Off;
            return IdleEvent::Failed(error);
        }
    };
    if data.is_empty() {
        return IdleEvent::None;
    }
    let Ok(mono) = pcm::to_mono(&data, pcm::CHANNELS) else {
        // A partial frame means the stream is misaligned; dropping it is cheaper
        // than carrying half a sample into the next tick.
        idle.carry.clear();
        return IdleEvent::None;
    };
    idle.carry.extend_from_slice(&mono);

    let frame_len = vad::frame_len(pcm::SAMPLE_RATE);
    let mut offset = 0_usize;
    let mut captured = None;
    while idle.carry.len() - offset >= frame_len {
        idle.frame.clear();
        idle.frame
            .extend_from_slice(&idle.carry[offset..offset + frame_len]);
        offset += frame_len;
        idle.frames = idle.frames.saturating_add(1);
        idle.last_frame_rms = vad::rms(&idle.frame);
        match idle.detector.push(&idle.frame) {
            vad::Event::Started => idle.last_voice = Some(Instant::now()),
            vad::Event::Captured(samples) => {
                idle.last_voice = Some(Instant::now());
                captured = Some(samples);
                break;
            }
            vad::Event::Idle => {}
        }
    }
    if offset > 0 {
        idle.carry.drain(..offset);
    }
    match captured {
        Some(samples) => {
            // Close the microphone now: the answer is about to be spoken, and a
            // microphone that keeps listening would hear it.
            let _ = audio.monitor_stop(i2c);
            IdleEvent::Voice(audio.utterance_from_mono(&samples))
        }
        None => IdleEvent::None,
    }
}

fn rtc_set(i2c: &mut I2cDriver<'_>, stamp: &clock::DateTime) -> Result<(), String> {
    let time = stamp
        .pcf85063_regs()
        .ok_or_else(|| format!("ERR rtc cannot store {} (year range)\r\n", stamp.iso()))?;
    // Control_1 = 0x00: 24-hour mode, oscillator running, all interrupts masked.
    write_reg(i2c, PCF85063_ADDR, 0x00, 0x00)
        .map_err(|error| format!("ERR rtc control write: {error}\r\n"))?;
    let mut write = [0_u8; 8];
    write[0] = 0x04;
    write[1..].copy_from_slice(&time);
    i2c.write(PCF85063_ADDR, &write, i2c_timeout())
        .map_err(|error| format!("ERR rtc time write: {error}\r\n"))?;

    let mut read_back = [0_u8; 7];
    read_regs(i2c, PCF85063_ADDR, 0x04, &mut read_back)
        .map_err(|error| format!("ERR rtc read back: {error}\r\n"))?;
    if read_back != time {
        return Err(format!(
            "ERR rtc read back mismatch wrote={time:02x?} read={read_back:02x?}\r\n"
        ));
    }
    Ok(())
}

/// Synchronise from SNTP and store the result in the RTC.
/// Decibels to the tenths the settings file and console both use.
fn tenths(db: f32) -> i16 {
    (db * 10.0).round() as i16
}

/// Write the settings file. The passphrase goes in too: the card is inside the
/// device, so reading it already requires physical access to the hardware.
fn save_settings(settings: &config::Settings) -> Result<String, String> {
    let directory = Path::new("/sdcard").join(config::DIR);
    fs::create_dir_all(&directory)
        .map_err(|error| format!("ERR config create {}: {error}\r\n", directory.display()))?;
    let path = Path::new("/sdcard").join(config::FILE);
    let text = settings.render();
    fs::write(&path, text.as_bytes())
        .map_err(|error| format!("ERR config write {}: {error}\r\n", path.display()))?;
    Ok(format!(
        "OK config saved path={} bytes={} password={}\r\n",
        path.display(),
        text.len(),
        if settings.password.is_some() {
            "<set>"
        } else {
            "<none>"
        }
    ))
}

/// Offline pre-flight for the voice assistant: all three requests are built from
/// the real settings and a synthetic reply on each endpoint is decoded. No socket
/// opens, so this costs nothing and is safe without a link.
fn assistant_selftest(settings: &config::Settings) -> Result<String, String> {
    let url = settings.bridge_url().ok_or_else(|| {
        format!(
            "ERR ai.selftest: no ai.url in {}; add ai.url=http://<host>:<port>\r\n",
            config::FILE
        )
    })?;
    let base = bridge::Base::parse(url)?;
    let hints = bridge::Hints {
        lang: settings.lang(),
        voice: settings.voice(),
        model: settings.model(),
        token: settings.bridge_token().unwrap_or(""),
        trigger: "",
    };

    // One second of silence, encoded exactly the way a real utterance is: the
    // compression and the request are the parts that can quietly go wrong.
    let silence = vec![0_u8; bridge::ASSISTANT_RATE as usize * 2];
    let utterance = pcm::ulaw_encode_all(&silence)?;
    let stt = bridge::stt_request(&base, &hints, &utterance);
    let llm = bridge::llm_request(&base, &hints, "Скажи одно слово.");
    let tts = bridge::tts_request(&base, &hints, "Проверка связи.");

    // Round-trip the capture path the way the bridge would: µ-law back to samples.
    let recovered = pcm::ulaw_decode_all(&stt.body);
    if recovered != silence {
        return Err("ERR ai.selftest: ulaw round trip changed the signal\r\n".to_owned());
    }

    // And the playback path: a µ-law reply decodes to a playable mono stream.
    let speech = bridge::speech_response("audio/basic", &utterance)?;
    let stereo = speech.to_channels(2);
    if speech.frames() != silence.len() / 2 || stereo.len() != speech.pcm.len() * 2 {
        return Err("ERR ai.selftest: speech decode produced the wrong length\r\n".to_owned());
    }

    // A transcript and a reply have to survive the same validation the real
    // responses will meet, including the character budget.
    let transcript = bridge::transcript_response("Проверка связи.".as_bytes())?;
    let reply = bridge::reply_response("Ответ. ".repeat(400).as_bytes())?;

    Ok(format!(
        "OK ai.selftest bridge={} tls={} token={} lang={} voice={} model={}\r\n  \
         capture_1s_bytes={} ulaw={} stt_body={} llm_body={} tts_body={}\r\n  \
         decode frames={} stereo_bytes={} transcript={}chars reply={}chars\r\n  \
         urls stt={} llm={} tts={}\r\n",
        base.summary(),
        u8::from(base.tls),
        match settings.bridge_token() {
            Some(token) => format!("<set:{} chars>", token.len()),
            None => "<none>".to_owned(),
        },
        settings.lang(),
        settings.voice(),
        settings.model(),
        silence.len(),
        utterance.len(),
        stt.body.len(),
        llm.body.len(),
        tts.body.len(),
        speech.frames(),
        stereo.len(),
        transcript.chars().count(),
        reply.chars().count(),
        stt.url,
        llm.url,
        tts.url,
    ))
}

/// One spoken turn, end to end: capture at the codec rate, convert to what the
/// bridge expects, post it, print what came back.
///
/// The three conversions are separate on purpose. The bridge speaks 16 kHz µ-law
/// mono because that is what speech recognisers and the telephone network both
/// assume; the codec runs at 24 kHz stereo because that is the configuration its
/// registers were verified in. `to_mono` then `resample` then `ulaw_encode_all`
/// keeps each step testable on the host, and the console reports the byte counts so
/// a silent failure at any step is visible without a logic analyser.
/// One transcription: the words, and the report describing how they were obtained.
struct Turn {
    transcript: String,
    report: String,
}

/// One answer: the reply text, and the report describing the round trip.
struct Answer {
    reply: String,
    report: String,
    /// The bridge handed the question to a plugin whose picture is on its way:
    /// speak the reply, leave the panel to the plugin.
    keep_panel: bool,
    /// A command for the frame (`X-Action`), carried out after speaking.
    action: Option<actions::Action>,
}

/// Capture, encode, upload and transcribe, returning the words with their timings.
fn assistant_turn(
    audio: &mut audio::Audio,
    i2c: &mut I2cDriver<'_>,
    settings: &config::Settings,
    ms: u32,
    trigger: &str,
) -> Result<Turn, String> {
    let started = Instant::now();
    let utterance = audio.utterance(i2c, ms)?;
    transcribe_utterance(settings, utterance, started, trigger)
}

/// The uplink half of a turn: audio in, transcript out.
///
/// Split out of [`assistant_turn`] because the idle watcher has to capture
/// before it knows a turn is possible — the audio arrives from the detector, not
/// from a console command — and only this half may be duplicated.
fn transcribe_utterance(
    settings: &config::Settings,
    utterance: audio::Utterance,
    started: Instant,
    trigger: &str,
) -> Result<Turn, String> {
    let url = settings.bridge_url().ok_or_else(|| {
        format!(
            "ERR ai.stt: no ai.url in {}; add ai.url=http://<host>:<port>\r\n",
            config::FILE
        )
    })?;
    let base = bridge::Base::parse(url)?;
    let hints = bridge::Hints {
        lang: settings.lang(),
        voice: settings.voice(),
        model: settings.model(),
        token: settings.bridge_token().unwrap_or(""),
        trigger,
    };

    // Each stage is timed separately: the whole conversion was 511 ms of device time for
    // 2 ms of arithmetic on a host, and a single total cannot say which stage that is.
    let converting = Instant::now();
    let mono = pcm::to_mono(&utterance.pcm, pcm::CHANNELS)?;
    let mono_ms = converting.elapsed().as_millis();
    let resampling = Instant::now();
    let converted = pcm::resample(&mono, pcm::SAMPLE_RATE, bridge::ASSISTANT_RATE)?;
    let resample_ms = resampling.elapsed().as_millis();
    let packing = Instant::now();
    let packed = pcm::to_frames(&converted, 1);
    let frames_ms = packing.elapsed().as_millis();
    let encoding = Instant::now();
    let ulaw = pcm::ulaw_encode_all(&packed)?;
    let ulaw_ms = encoding.elapsed().as_millis();
    let conv_ms = converting.elapsed().as_millis();
    let request = bridge::stt_request(&base, &hints, &ulaw);
    let headers = request_headers(&request);

    let sent = Instant::now();
    let reply = http::post(
        &request.url,
        &headers,
        &request.body,
        bridge::MAX_TRANSCRIPT_BYTES,
    )?;
    let roundtrip_ms = sent.elapsed().as_millis();
    if !reply.ok() {
        // The bridge explains a 401 or a failed recognition in words, so pass them
        // through bounded rather than reporting only the status code.
        let text: String = reply.text().chars().take(200).collect();
        return Err(format!(
            "ERR ai.stt status={} from {}: {}\r\n",
            reply.status, request.url, text
        ));
    }
    if reply.truncated {
        // Half a sentence is not a transcript, so this is an error rather than a
        // shortened answer; the bridge should never produce one.
        return Err(format!(
            "ERR ai.stt reply exceeds {} bytes\r\n",
            bridge::MAX_TRANSCRIPT_BYTES
        ));
    }
    let transcript = bridge::transcript_response(&reply.body)?;

    Ok(Turn {
        report: format!(
            "OK ai.stt status={} url={} seconds={:.2} pcm_bytes={} ulaw_bytes={} headers={}\r\n  \
         cue_ms={} setup_ms={} read_ms={} capture_ms={} conv_ms={conv_ms} roundtrip_ms={roundtrip_ms} norm_db={} {} trace_ms={} content_type={}\r\n  \
         mono_ms={mono_ms} resample_ms={resample_ms} frames_ms={frames_ms} ulaw_ms={ulaw_ms} resampled={}\r\n  \
         transcript=\"{}\"\r\n",
            reply.status,
            request.url,
            utterance.pcm.len() as f32 / (pcm::SAMPLE_RATE as f32 * pcm::CHANNELS as f32 * 2.0),
            utterance.pcm.len(),
            ulaw.len(),
            headers.len(),
            utterance.cue_ms,
            utterance.setup_ms,
            utterance.read_ms,
            utterance.capture_ms,
            utterance.norm_db,
            utterance.measured.measurement(),
            started.elapsed().as_millis(),
            reply.content_type,
            converted.len(),
            transcript,
        ),
        transcript,
    })
}

/// Text in, speech out: the downlink half of a turn, with no microphone in the
/// path so the reply can be exercised by typing.
///
/// Three stages, each with its own duration: the model answers text, the
/// synthesiser answers 16 kHz µ-law mono, and the codec wants 24 kHz stereo. A
/// silent assistant is then traceable to one stage instead of "no sound".
/// Ask the bridge, speak the answer, and return both texts.
fn assistant_answer(
    audio: &mut audio::Audio,
    i2c: &mut I2cDriver<'_>,
    settings: &config::Settings,
    prompt: &str,
) -> Result<Answer, String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("ERR ai.ask: nothing to ask\r\n".into());
    }
    let url = settings.bridge_url().ok_or_else(|| {
        format!(
            "ERR ai.ask: no ai.url in {}; add ai.url=http://<host>:<port>\r\n",
            config::FILE
        )
    })?;
    let base = bridge::Base::parse(url)?;
    let hints = bridge::Hints {
        lang: settings.lang(),
        voice: settings.voice(),
        model: settings.model(),
        token: settings.bridge_token().unwrap_or(""),
        trigger: "",
    };

    let started = Instant::now();
    let llm = bridge::llm_request(&base, &hints, prompt);
    let headers = request_headers(&llm);
    let sent = Instant::now();
    let answer = http::post(&llm.url, &headers, &llm.body, bridge::MAX_ANSWER_BYTES)?;
    let llm_ms = sent.elapsed().as_millis();
    if !answer.ok() {
        let text: String = answer.text().chars().take(200).collect();
        return Err(format!(
            "ERR ai.ask status={} from {}: {}\r\n",
            answer.status, llm.url, text
        ));
    }
    if answer.truncated {
        return Err(format!(
            "ERR ai.ask reply exceeds {} bytes\r\n",
            bridge::MAX_ANSWER_BYTES
        ));
    }
    let reply = bridge::reply_response(&answer.body)?;
    // «В комнате {temp}.»: only the frame knows its readings.
    let reply = if actions::wants_readings(&reply) {
        actions::fill(&reply, &room_readings(i2c))
    } else {
        reply
    };
    let action = answer.action.as_deref().and_then(actions::parse);

    let tts = bridge::tts_request(&base, &hints, &reply);
    let headers = request_headers(&tts);
    let sent = Instant::now();
    let spoken = http::post(&tts.url, &headers, &tts.body, bridge::MAX_SPEECH_BYTES)?;
    let tts_ms = sent.elapsed().as_millis();
    if !spoken.ok() {
        let text: String = spoken.text().chars().take(200).collect();
        return Err(format!(
            "ERR ai.ask status={} from {}: {}\r\n",
            spoken.status, tts.url, text
        ));
    }
    // The bridge answers µ-law at 16 kHz mono by default and WAV when the host
    // prefers it, so the rate comes from the reply rather than from a constant.
    let converting = Instant::now();
    // A long answer is cut at the transport limit rather than refused: the speaker
    // says the first twenty seconds and the report says it was cut.
    let speech = bridge::speech_response(&spoken.content_type, &spoken.body)?;
    let mono = pcm::to_mono(&speech.pcm, usize::from(speech.channels))?;
    let converted = pcm::resample(&mono, speech.sample_rate, pcm::SAMPLE_RATE)?;
    let stereo = pcm::to_frames(&converted, pcm::CHANNELS);
    let conv_ms = converting.elapsed().as_millis();
    let seconds = converted.len() as f32 / pcm::SAMPLE_RATE as f32;

    let played = Instant::now();
    audio.play_speech(i2c, &stereo)?;
    let play_ms = played.elapsed().as_millis();

    Ok(Answer {
        report: format!(
            "OK ai.ask url={} prompt=\"{}\" reply_chars={} speech_bytes={} truncated={} frames={} seconds={:.2} llm_ms={llm_ms} tts_ms={tts_ms} conv_ms={conv_ms} play_ms={play_ms} total_ms={} content_type={}\r\n  \
         reply=\"{}\"\r\n",
            llm.url,
            prompt,
            reply.chars().count(),
            spoken.body.len(),
            spoken.truncated,
            converted.len(),
            seconds,
            started.elapsed().as_millis(),
            spoken.content_type,
            reply,
        ),
        reply,
        keep_panel: answer.keep_panel,
        action,
    })
}

/// The readings a reply may ask for, read now.
fn room_readings(i2c: &mut I2cDriver<'_>) -> actions::Readings {
    let ambient = shtc3_read(i2c).ok();
    actions::Readings {
        temp_hundredths: ambient.map(|(_, temp, _)| temp),
        rh_hundredths: ambient.map(|(_, _, rh)| rh),
        battery_percent: battery(i2c)
            .ok()
            .filter(|battery| battery.present)
            .map(|battery| battery.percent),
    }
}

/// Carry out what the bridge asked for after an answer (`actions.rs`), with
/// the same means as the buttons and the console. A picture is drawn at once
/// when the panel is free, else as soon as it is (the gallery is aimed and due).
fn run_action(
    action: actions::Action,
    gallery: &mut gallery::Gallery,
    painter: &mut Painter,
    storage_state: &Result<storage::Storage, String>,
    settings: &config::Settings,
) -> String {
    use actions::Action;
    let start_if_stopped = |gallery: &mut gallery::Gallery| {
        if gallery.is_running() {
            return Ok(());
        }
        let started = gallery_start(gallery, storage_state, settings, None, None);
        if gallery.is_running() {
            Ok(())
        } else {
            Err(started)
        }
    };
    match action {
        Action::Slideshow(false) => {
            gallery.stop();
            "action=slideshow-off".to_owned()
        }
        Action::Slideshow(true) => {
            if gallery.is_running() {
                return "action=slideshow-on already".to_owned();
            }
            let started = gallery_start(gallery, storage_state, settings, None, None);
            format!("action=slideshow-on {}", started.trim())
        }
        Action::Interval(seconds) => {
            gallery.set_interval(Duration::from_secs(u64::from(seconds)), Instant::now());
            format!(
                "action=interval interval_s={} running={}",
                gallery.interval().as_secs(),
                gallery.is_running()
            )
        }
        Action::Next | Action::Previous | Action::Show(_) => {
            if let Err(error) = start_if_stopped(gallery) {
                return format!("action=picture {}", error.trim());
            }
            let aimed = match action {
                Action::Previous => gallery.aim(gallery.previous_position()),
                Action::Show(number) => gallery.aim(number as usize - 1),
                _ => {
                    gallery.hurry();
                    true
                }
            };
            if !aimed {
                return format!("action=picture ERR no such picture of {}", gallery.count());
            }
            if painter.is_ready() {
                let shown = next_picture(gallery, painter, storage_state, settings);
                format!("action=picture {}", shown.trim())
            } else {
                // Aimed and due: the slideshow draws it once the panel is free.
                "action=picture queued (panel busy)".to_owned()
            }
        }
    }
}

/// How long a console command will wait for somebody to press KEY.
const KEY_WAIT: Duration = Duration::from_secs(30);

/// Poll KEY until it is pressed, returning how long that took.
///
/// Polling rather than the GPIO interrupt: the console loop is already the only
/// place that touches this pin, and an interrupt would need to hand a token back to
/// a loop that is busy running a capture.
fn wait_for_key(key: &PinDriver<'_, Input>, limit: Duration) -> Result<u128, String> {
    let started = Instant::now();
    loop {
        if key.is_low() {
            return Ok(started.elapsed().as_millis());
        }
        if started.elapsed() >= limit {
            return Err(format!(
                "ERR ai.listen: no KEY press within {} s\r\n",
                limit.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Bind the portal and describe it once. The reply goes to the console, so the
/// one line says what is reachable and whether it is locked.
fn start_web(
    state: &mut Option<(web::Server, mpsc::Receiver<web::Job>)>,
    settings: &config::Settings,
) -> Result<String, String> {
    let (jobs, receiver) = mpsc::channel();
    let server = web::start(web::PORT, settings.web_password().map(str::to_owned), jobs)?;
    let reply = format!(
        "OK web listening port={} password={} route=/ (upload, gallery, settings)\r\n",
        server.port(),
        if server.locked() { "<set>" } else { "<none>" }
    );
    *state = Some((server, receiver));
    Ok(reply)
}

fn start_opendisplay(
    state: &mut Option<(opendisplay::Handle, mpsc::Receiver<opendisplay::Job>)>,
    identity: Option<(std::net::Ipv4Addr, [u8; 6])>,
) -> Result<String, String> {
    let (jobs, receiver) = mpsc::channel();
    let (handle, note) =
        opendisplay::start(jobs, identity).map_err(|error| format!("{error}\r\n"))?;
    *state = Some((handle, receiver));
    Ok(format!(
        "OK opendisplay listening port={} {note}\r\n",
        opendisplay::PORT
    ))
}

/// Serve one request from an OpenDisplay client.
///
/// A picture is shown only when the panel is free, exactly like a portal
/// upload, and nothing is saved: an OpenDisplay client keeps its own images.
/// Returns a sleep request (timed wake in seconds) when the client asked for one.
fn handle_opendisplay_job(
    job: opendisplay::Job,
    painter: &mut Painter,
    settings: &config::Settings,
) -> Option<u32> {
    match job {
        opendisplay::Job::LoadNames { reply } => {
            let _ = reply.send(opendisplay::load_names());
        }
        opendisplay::Job::SaveNames { names, reply } => {
            let _ = reply.send(opendisplay::save_names(names.as_deref()));
        }
        opendisplay::Job::PanelFree { reply } => {
            let _ = reply.send(painter.is_ready());
        }
        opendisplay::Job::Show { frame, done, reply } => {
            let started = painter.is_ready()
                && painter
                    .with_ready(|_, live| live.copy_from_slice(&frame))
                    .and_then(|()| painter.refresh_later_then(done))
                    .is_ok();
            let _ = reply.send(if started { Ok(frame) } else { Err(frame) });
        }
        // Jobs run one at a time on this loop, so no card write is in flight.
        opendisplay::Job::Reboot if settings.opendisplay_reboot() => {
            esp_idf_svc::hal::reset::restart();
        }
        opendisplay::Job::Reboot => {}
        // The client's own interval wins, held to the same minimum as the
        // settings so a client cannot turn the frame into a refresh loop.
        opendisplay::Job::DeepSleep(secs) => {
            return Some(secs.map_or_else(
                || settings.sleep_secs(),
                |secs| secs.max(config::MIN_SLEEP_SECS),
            ));
        }
    }
    None
}

/// Serve one request from the portal.
///
/// The main loop owns the card, the panel, the codec and the settings, so the
/// web thread only ever describes work; this is where it is done and answered.
#[allow(clippy::too_many_arguments)]
fn handle_web_job(
    job: web::Job,
    sensors: &Sensors,
    storage_state: &Result<storage::Storage, String>,
    settings: &mut config::Settings,
    painter: &mut Painter,
    gallery: &mut gallery::Gallery,
    idle: &mut Idle,
    audio_state: &mut Result<audio::Audio, String>,
    i2c: &mut I2cDriver<'_>,
    rebind_web: &mut bool,
) {
    match job {
        web::Job::Upload { frame, reply } => {
            let outcome = save_upload(&frame, settings).map(|relative| {
                // Show it at once when the panel is free. If it is already
                // refreshing, the file is on the card and the slideshow will
                // bring it up: no request may wait 31 s for a segment.
                let painted = if painter.is_ready() {
                    match painter.with_ready(|_, live| live.copy_from_slice(&frame)) {
                        Ok(()) => match painter.refresh_later() {
                            Ok(()) => "paint=started".to_owned(),
                            Err(error) => format!("paint={}", error.trim()),
                        },
                        Err(error) => format!("paint={}", error.trim()),
                    }
                } else {
                    "paint=busy".to_owned()
                };
                // A running slideshow started with an older list; focusing the
                // new picture makes the next advance the one just sent.
                if gallery.is_running()
                    && let Ok(storage) = storage_state
                    && let Ok(images) = storage.images(Some(gallery.dir()))
                {
                    gallery.replace(images, Some(&relative));
                }
                format!("OK saved={relative} bytes={} {painted}\n", frame.len())
            });
            let _ = reply.send(outcome);
        }
        web::Job::List { reply } => {
            let directory = gallery_directory(settings);
            let _ = reply.send(list_gallery(storage_state, &directory));
        }
        web::Job::Image { path, reply } => {
            // A fresh buffer per request: the reply is handed to the web thread
            // and must stay valid while the loop does something else.
            let mut frame = vec![0_u8; panel::buffer_len()];
            let outcome = image::load_any(&path, &mut frame)
                .map(|()| frame)
                .map_err(|error| format!("ERR {}: {}\n", path, error.trim_end()));
            let _ = reply.send(outcome);
        }
        web::Job::Show { path, reply } => {
            let _ = reply.send(show_now(painter, &path));
        }
        web::Job::Delete { path, reply } => {
            let _ = reply.send(delete_picture(&path, storage_state, gallery));
        }
        web::Job::Sensors { reply } => {
            let _ = reply.send(sensors.json());
        }
        web::Job::Settings { reply } => {
            let _ = reply.send(web_settings_json(settings));
        }
        web::Job::SaveSettings { fields, reply } => {
            let before = settings.clone();
            let outcome = apply_web_fields(&fields, settings).map(|summary| {
                let live = apply_live_settings(
                    &before,
                    settings,
                    idle,
                    audio_state,
                    i2c,
                    gallery,
                    storage_state,
                );
                // The running listener cannot change its own lock, so the loop
                // rebinds it once this request has been answered.
                if before.web_password != settings.web_password {
                    *rebind_web = true;
                }
                format!("{summary}{live}")
            });
            let _ = reply.send(outcome);
        }
    }
}

/// Directory the portal lists and writes to: the slideshow's own, so an upload
/// is immediately part of what the frame can show.
fn gallery_directory(settings: &config::Settings) -> String {
    settings
        .gallery_dir()
        .unwrap_or(DEFAULT_GALLERY_DIR)
        .to_owned()
}

/// Most uploads one directory can hold before a number has to be recycled. The
/// bound exists so `UPnnnn` stays inside an 8.3 name; a card that full needs a
/// tidy-up more than a longer name.
const MAX_UPLOADS: u32 = 9_999;

/// Write one raw frame into the slideshow directory under the first free name.
///
/// Only inside [`image::WRITABLE_DIR`]: `gallery.dir` may point the slideshow
/// at the vendor's pictures, but the portal does not write among them.
fn save_upload(frame: &[u8], settings: &config::Settings) -> Result<String, String> {
    let directory = gallery_directory(settings);
    if !image::is_writable_dir(&directory) {
        return Err(format!(
            "ERR gallery.dir={directory} is outside {}; uploads are only written there\n",
            image::WRITABLE_DIR
        ));
    }
    let path = Path::new("/sdcard").join(&directory);
    fs::create_dir_all(&path)
        .map_err(|error| format!("ERR create {}: {error}\n", path.display()))?;
    // One directory read, names only: probing UP0001, UP0002... with `exists()`
    // made each upload a lookup per earlier upload, each a scan of the directory.
    let names: Vec<String> = fs::read_dir(&path)
        .map_err(|error| format!("ERR list {}: {error}\n", path.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    let number = gallery::first_free_upload(names.iter().map(String::as_str), MAX_UPLOADS)
        .ok_or_else(|| format!("ERR {directory} already holds {MAX_UPLOADS} uploads\n"))?;
    let relative = format!("{directory}/UP{number:04}.PF6");
    let file = Path::new("/sdcard").join(&relative);
    fs::write(&file, frame).map_err(|error| format!("ERR write {}: {error}\n", file.display()))?;
    Ok(relative)
}

/// The slideshow directory as the portal's JSON, sizes included.
fn list_gallery(
    storage_state: &Result<storage::Storage, String>,
    directory: &str,
) -> Result<(String, Vec<web::Entry>), String> {
    let storage = storage_state
        .as_ref()
        .map_err(|error| error.trim_end().to_owned())?;
    // Created on first use: a fresh card has no upload directory, and an empty
    // gallery is a more useful answer than an error page. Never outside the
    // writable directory, though; there a missing directory is an error.
    if image::is_writable_dir(directory) {
        let path = Path::new("/sdcard").join(directory);
        fs::create_dir_all(&path)
            .map_err(|error| format!("ERR create {}: {error}\n", path.display()))?;
    }
    let entries = storage
        .pictures(Some(directory))?
        .into_iter()
        .map(|(name, bytes)| web::Entry { name, bytes })
        .collect();
    Ok((directory.to_owned(), entries))
}

/// Remove one picture from the writable directory.
///
/// The path arrives from a query string, so it is treated as untrusted input;
/// the policy itself lives in `image::removable`, which refuses anything that is
/// not a picture inside `PFTEST` (see its tests). This function only performs
/// the removal and keeps the slideshow's list honest afterwards.
fn delete_picture(
    path: &str,
    storage_state: &Result<storage::Storage, String>,
    gallery: &mut gallery::Gallery,
) -> Result<String, String> {
    let file = image::removable(path)?;
    fs::remove_file(&file)
        .map_err(|error| format!("ERR remove {}: {error}\r\n", file.display()))?;

    // The slideshow holds the list it started with, so a removed picture would
    // otherwise be offered once more and fail to decode.
    let mut remaining = None;
    if gallery.is_running()
        && let Ok(storage) = storage_state
        && let Ok(images) = storage.images(Some(gallery.dir()))
    {
        remaining = Some(images.len());
        gallery.replace(images, None);
    }
    Ok(match remaining {
        Some(remaining) => format!("OK deleted={path} images={remaining}\r\n"),
        None => format!("OK deleted={path}\r\n"),
    })
}

/// Decode an existing picture into the panel's buffer and start a refresh, the
/// same way an advance does. The refresh is asynchronous, so the browser is not
/// held for half a minute.
fn show_now(painter: &mut Painter, path: &str) -> Result<String, String> {
    if !painter.is_ready() {
        return Err("ERR display is painting; try again in a moment\n".to_owned());
    }
    match painter.with_ready(|_, frame| image::load_any(path, frame)) {
        Ok(Ok(())) => match painter.refresh_later() {
            Ok(()) => Ok(format!("OK show={path} paint=started (about 30 s)\n")),
            Err(error) => Err(format!("ERR {}\n", error.trim())),
        },
        Ok(Err(error)) => Err(format!("ERR {path}: {}\n", error.trim_end())),
        Err(error) => Err(format!("ERR {}\n", error.trim())),
    }
}

/// The settings document the form is filled from. Secrets become a boolean: an
/// empty field means "leave it", so the page never needs to see the value.
fn web_settings_json(settings: &config::Settings) -> String {
    let text = |value: Option<&str>| match value {
        Some(value) => format!("\"{}\"", web::json_escape(value)),
        None => "null".to_owned(),
    };
    let tenths = |value: Option<i16>| match value {
        Some(tenths) => format!("\"{:.1}\"", f32::from(tenths) / 10.0),
        None => "null".to_owned(),
    };
    let sections = [
        format!(
            "\"wifi\":{{\"ssid\":{},\"password_set\":{},\"auto\":{}}}",
            text(settings.ssid.as_deref()),
            settings.password.is_some(),
            settings.wifi_auto
        ),
        format!(
            "\"audio\":{{\"volume_db\":{},\"gain_db\":{}}}",
            tenths(settings.volume_tenths),
            tenths(settings.gain_tenths)
        ),
        format!(
            "\"ai\":{{\"url\":{},\"key_set\":{},\"lang\":{},\"idle\":{},\"threshold\":{}}}",
            text(settings.bridge_url()),
            settings.bridge_token().is_some(),
            text(Some(settings.lang())),
            text(Some(settings.ai_idle.unwrap_or_default().name())),
            settings
                .ai_threshold
                .map_or_else(|| "null".to_owned(), |value| value.to_string())
        ),
        format!(
            "\"gallery\":{{\"dir\":{},\"secs\":{},\"auto\":{}}}",
            text(Some(&gallery_directory(settings))),
            settings.gallery_secs.map_or_else(
                || gallery::DEFAULT_INTERVAL_SECS.to_string(),
                |secs| secs.to_string()
            ),
            settings.gallery_auto()
        ),
        format!(
            "\"web\":{{\"password_set\":{}}}",
            settings.web_password().is_some()
        ),
    ];
    format!("{{{}}}", sections.join(","))
}

/// Apply the settings form by editing the canonical text and re-parsing it.
///
/// Nothing is assigned field by field: every key keeps the parser's own bounds
/// and validation, which is the only way a value accepted over HTTP is certain
/// to load on the next boot. An empty field means "leave it alone", which is
/// what a password box the user did not fill in has to mean.
fn apply_web_fields(
    fields: &[(String, String)],
    settings: &mut config::Settings,
) -> Result<String, String> {
    let mut lines: Vec<String> = settings
        .render()
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(str::to_owned)
        .collect();
    let mut touched = Vec::new();
    for (name, value) in fields {
        if value.is_empty() {
            continue;
        }
        // `-` is the portal's "remove the lock", the same convention `ai.key`
        // uses. An empty field is reserved for "leave it alone".
        if name == "web.password" && value == "-" {
            settings.set_web_password("")?;
            touched.push("web.password(cleared)".to_owned());
            continue;
        }
        lines.retain(|line| !line.starts_with(&format!("{name}=")));
        lines.push(format!("{name}={value}"));
        touched.push(name.clone());
    }
    let parsed =
        config::Settings::parse(&lines.join("\n")).map_err(|error| error.trim_end().to_owned())?;
    *settings = parsed;
    save_settings(settings)?;
    Ok(format!(
        "OK settings saved on the card: {}\n",
        touched.join(" ")
    ))
}

/// Make the settings that have a live effect take it now, and say which ones
/// only apply later. Called after the file has been written, so the two can
/// never disagree about what was asked for.
fn apply_live_settings(
    before: &config::Settings,
    settings: &mut config::Settings,
    idle: &mut Idle,
    audio_state: &mut Result<audio::Audio, String>,
    i2c: &mut I2cDriver<'_>,
    gallery_state: &mut gallery::Gallery,
    storage_state: &Result<storage::Storage, String>,
) -> String {
    let mut notes = String::new();
    if before.volume_tenths != settings.volume_tenths
        && let Some(tenths) = settings.volume_tenths
    {
        let line = match audio_state {
            Ok(audio) => audio.set_volume(i2c, tenths),
            Err(error) => Err(error.clone()),
        };
        let line = line.unwrap_or_else(|error| error);
        notes.push_str(&format!("  volume: {}\n", line.trim()));
    }
    if before.gain_tenths != settings.gain_tenths
        && let Some(tenths) = settings.gain_tenths
    {
        let line = match audio_state {
            Ok(audio) => audio.set_gain(i2c, tenths),
            Err(error) => Err(error.clone()),
        };
        let line = line.unwrap_or_else(|error| error);
        notes.push_str(&format!("  gain: {}\n", line.trim()));
    }
    if before.ai_threshold != settings.ai_threshold {
        let threshold = settings.idle_threshold().unwrap_or(vad::DEFAULT_THRESHOLD);
        idle.detector.set_threshold(threshold);
        notes.push_str(&format!(
            "  threshold: live at trigger={} floor={}\n",
            idle.detector.trigger().round(),
            idle.detector.floor().round()
        ));
    }
    if before.ai_idle != settings.ai_idle {
        let mode = settings.ai_idle.unwrap_or_default();
        let line = set_idle_mode(idle, mode, audio_state, i2c);
        // The hardware may refuse voice; keep the file honest about it.
        settings.ai_idle = Some(idle.mode);
        notes.push_str(&format!("  idle: {}\n", line.trim()));
    }
    if gallery_state.is_running()
        && (before.gallery_dir != settings.gallery_dir
            || before.gallery_secs != settings.gallery_secs)
    {
        let line = gallery_start(gallery_state, storage_state, settings, None, None);
        notes.push_str(&format!("  slideshow: {}\n", line.trim()));
    }
    if before.web_password != settings.web_password {
        notes.push_str("  web.password: the listener is restarting with it\n");
    }
    if before.wifi_auto != settings.wifi_auto
        || before.ssid != settings.ssid
        || before.password != settings.password
    {
        notes.push_str("  wifi: applies on the next join or reboot\n");
    }
    notes
}

/// Where `gallery start` looks and where the portal writes when neither the
/// command nor the file names a directory. Inside `PFTEST`, the one place a host
/// is allowed to write, and its own directory so uploads stay separable from
/// `CONFIG.TXT`.
const DEFAULT_GALLERY_DIR: &str = "PFTEST/UPLOADS";

/// Start the slideshow, from the console or from `gallery.auto` at boot.
///
/// Both arguments fall back to the settings file, so a bare `gallery start`
/// resumes what the card remembers rather than resetting it. The reply is the
/// `gallery status` line, which is the only place the effective interval is
/// visible - a clamped request has to be readable as the number actually used.
fn gallery_start(
    gallery: &mut gallery::Gallery,
    storage_state: &Result<storage::Storage, String>,
    settings: &config::Settings,
    dir: Option<&str>,
    seconds: Option<u32>,
) -> String {
    let storage = match storage_state {
        Ok(storage) => storage,
        Err(error) => return error.clone(),
    };
    let directory = dir
        .map(str::to_owned)
        .or_else(|| settings.gallery_dir().map(str::to_owned))
        .unwrap_or_else(|| DEFAULT_GALLERY_DIR.to_owned());
    let images = match storage.images(Some(&directory)) {
        Ok(images) => images,
        Err(error) => return error,
    };
    if images.is_empty() {
        return format!(
            "ERR gallery: no .bmp or .pf6 in /sdcard/{directory}; copy images there first\r\n"
        );
    }
    let interval = seconds.map_or_else(
        || settings.gallery_interval(),
        gallery::Gallery::interval_from_secs,
    );
    gallery.start(directory, images, interval);
    let mut line = gallery.status().trim_end().to_owned();
    if let Some(requested) = seconds
        && gallery.interval().as_secs() != u64::from(requested)
    {
        line.push_str(&format!(
            "  requested_s={requested} clamped_to={}",
            gallery.interval().as_secs()
        ));
    }
    line.push_str("\r\n");
    line
}

const PAINT_MARGIN: usize = 48;
const PAINT_WIDTH: usize = panel::WIDTH - 2 * PAINT_MARGIN;
// Two short blocks rather than one equal split: a spoken question is a sentence,
// while an answer may run to MAX_REPLY_CHARS, and the panel has 12 lines of room
// below the two headers.
const PAINT_HEARD_LINES: usize = 3;
const PAINT_ANSWER_LINES: usize = 7;

/// Draw the heard question and, when there is one, the answer, and refresh the panel.
///
/// A full Spectra 6 refresh is slow - tens of seconds - so this is deliberately the
/// last thing a turn does: the panel is between the user and nothing, while the
/// console report and the spoken answer are not.
fn paint_turn(
    display: &mut display::Display<'_>,
    frame: &mut [u8],
    heard: &str,
    answer: Option<&str>,
) -> Result<String, String> {
    panel::fill(frame, panel::WHITE);
    let header = usize::from(font::LINE_HEIGHT);
    panel::draw_text(frame, PAINT_MARGIN, PAINT_MARGIN, panel::BLUE, "УСЛЫШАНО");
    let (heard_lines, mut top) = panel::draw_paragraph(
        frame,
        PAINT_MARGIN,
        PAINT_MARGIN + header,
        PAINT_WIDTH,
        panel::BLACK,
        PAINT_HEARD_LINES,
        heard,
    );
    let mut answer_lines = 0;
    if let Some(answer) = answer {
        top += header / 2;
        panel::draw_text(frame, PAINT_MARGIN, top, panel::RED, "ОТВЕТ");
        let (lines, bottom) = panel::draw_paragraph(
            frame,
            PAINT_MARGIN,
            top + header,
            PAINT_WIDTH,
            panel::BLACK,
            PAINT_ANSWER_LINES,
            answer,
        );
        answer_lines = lines;
        top = bottom;
    }
    let started = Instant::now();
    display.show(frame)?;
    let paint_ms = started.elapsed().as_millis();
    Ok(format!(
        "paint_ms={paint_ms} heard_lines={heard_lines} answer_lines={answer_lines} bottom_y={top}\r\n"
    ))
}

/// Borrow a request's headers as the name/value pairs the HTTP client takes.
fn request_headers(request: &bridge::Request) -> Vec<(&str, &str)> {
    request
        .headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

/// Bring the link up from stored credentials when it is down. Without stored
/// credentials this does nothing, so the caller's own error explains the state.
fn ensure_online(net: &mut net::Net<'_>, settings: &config::Settings) -> Result<(), String> {
    if net.is_connected()? {
        return Ok(());
    }
    match settings.credentials() {
        Some((ssid, password)) => net.join(ssid, password).map(|_| ()),
        None => Ok(()),
    }
}

fn wifi_time(
    net: &mut net::Net<'_>,
    i2c: &mut I2cDriver<'_>,
    server: Option<&str>,
) -> Result<String, String> {
    let epoch = net.sync_time(server)?;
    let stamp = clock::from_unix(epoch);
    rtc_set(i2c, &stamp)?;
    Ok(format!(
        "OK wifi time utc={} epoch={epoch} rtc=written\r\n",
        stamp.iso()
    ))
}

fn main() {
    sys::link_patches();
    if let Err(error) = run() {
        // No auto-reboot or power cycling on failure. ROM recovery remains available.
        eprintln!("diagnostic stopped: {error}");
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
}
