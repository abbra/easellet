//! ED2208 / 7.3-inch Spectra 6 panel driver, transcribed from the vendor
//! factory-test demo (`bsp_fac.h`). Init sequence and wire protocol are the
//! vendor's; no register values were invented here.

use std::{thread, time::Duration};

use esp_idf_svc::{
    hal::{
        gpio::{PinDriver, Pull},
        spi::{SpiDeviceDriver, SpiDriver, config::Config as SpiConfig},
        units::MegaHertz,
    },
    sys::{self, EspError},
};

use crate::panel;

const BUSY_TIMEOUT: Duration = Duration::from_secs(90);

const INIT_SEQUENCE: &[(u8, &[u8])] = &[
    (0xAA, &[0x49, 0x55, 0x20, 0x08, 0x09, 0x18]),
    (0x01, &[0x3F]),
    (0x00, &[0x5F, 0x69]),
    (0x03, &[0x00, 0x54, 0x00, 0x44]),
    (0x05, &[0x40, 0x1F, 0x1F, 0x2C]),
    (0x06, &[0x6F, 0x1F, 0x17, 0x49]),
    (0x08, &[0x6F, 0x1F, 0x1F, 0x22]),
    (0x30, &[0x03]),
    (0x50, &[0x3F]),
    (0x60, &[0x02, 0x00]),
    (0x61, &[0x03, 0x20, 0x01, 0xE0]),
    (0x84, &[0x01]),
    (0xE3, &[0x2F]),
];

pub struct Display<'d> {
    spi: SpiDeviceDriver<'d, SpiDriver<'d>>,
    dc: PinDriver<'d, esp_idf_svc::hal::gpio::Output>,
    rst: PinDriver<'d, esp_idf_svc::hal::gpio::Output>,
    busy: PinDriver<'d, esp_idf_svc::hal::gpio::Input>,
    awake: bool,
}

pub fn alloc_buffer() -> Result<&'static mut [u8], String> {
    let len = panel::buffer_len();
    // SAFETY: heap_caps_malloc returns either null or a valid allocation of
    // `len` bytes; `from_raw_parts_mut` is only called on the non-null branch.
    // The allocation lives for the rest of the program and is never freed.
    let pointer = unsafe { sys::heap_caps_malloc(len, sys::MALLOC_CAP_SPIRAM) } as *mut u8;
    if pointer.is_null() {
        return Err("ERR display buffer allocation failed in PSRAM\r\n".to_owned());
    }
    // SAFETY: non-null, correctly aligned for u8, and `len` bytes long.
    Ok(unsafe { core::slice::from_raw_parts_mut(pointer, len) })
}

impl<'d> Display<'d> {
    pub fn new(
        spi: SpiDeviceDriver<'d, SpiDriver<'d>>,
        dc: PinDriver<'d, esp_idf_svc::hal::gpio::Output>,
        rst: PinDriver<'d, esp_idf_svc::hal::gpio::Output>,
        busy: PinDriver<'d, esp_idf_svc::hal::gpio::Input>,
    ) -> Self {
        Self {
            spi,
            dc,
            rst,
            busy,
            awake: false,
        }
    }

    pub fn awake(&self) -> bool {
        self.awake
    }

    /// Whether the panel can take another command: its BUSY line reads high
    /// between refreshes, which is what `wait_busy` polls for.
    pub fn ready(&self) -> bool {
        self.busy.is_high()
    }

    fn command(&mut self, command: u8) -> Result<(), String> {
        self.dc.set_low().map_err(stringify)?;
        self.spi
            .write(&[command])
            .map_err(|e| format!("ERR display command {command:02x}: {e}\r\n"))
    }

    fn data(&mut self, data: &[u8]) -> Result<(), String> {
        self.dc.set_high().map_err(stringify)?;
        for chunk in data.chunks(2048) {
            self.spi
                .write(chunk)
                .map_err(|e| format!("ERR display data: {e}\r\n"))?;
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), String> {
        self.rst.set_high().map_err(stringify)?;
        thread::sleep(Duration::from_millis(50));
        self.rst.set_low().map_err(stringify)?;
        thread::sleep(Duration::from_millis(20));
        self.rst.set_high().map_err(stringify)?;
        thread::sleep(Duration::from_millis(50));
        Ok(())
    }

    fn wait_busy(&mut self) -> Result<(), String> {
        let start = std::time::Instant::now();
        while !self.busy.is_high() {
            if start.elapsed() > BUSY_TIMEOUT {
                return Err("ERR display busy timeout\r\n".to_owned());
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    pub fn init(&mut self) -> Result<(), String> {
        self.reset()?;
        self.wait_busy()?;
        thread::sleep(Duration::from_millis(50));
        for (command, data) in INIT_SEQUENCE {
            self.command(*command)?;
            self.data(data)?;
        }
        self.command(0x04)?;
        self.wait_busy()?;
        self.awake = true;
        Ok(())
    }

    pub fn show(&mut self, buffer: &[u8]) -> Result<(), String> {
        if buffer.len() != panel::buffer_len() {
            return Err("ERR display buffer size mismatch\r\n".to_owned());
        }
        if !self.awake {
            self.init()?;
        }
        // Match the vendor's EPD_TurnOnDisplay exactly: every refresh powers the
        // panel on, then waits for BUSY before starting the update. Without this
        // 0x04, a panel that is already awake can accept 0x12 without refreshing.
        self.command(0x10)?;
        self.data(buffer)?;
        self.command(0x04)?;
        self.wait_busy()?;
        self.command(0x06)?;
        self.data(&[0x6F, 0x1F, 0x17, 0x49])?;
        self.command(0x12)?;
        self.data(&[0x00])?;
        self.wait_busy()?;
        self.command(0x02)?;
        self.data(&[0x00])?;
        self.wait_busy()?;
        Ok(())
    }

    pub fn sleep(&mut self) -> Result<(), String> {
        if !self.awake {
            return Ok(());
        }
        self.command(0x07)?;
        self.data(&[0xA5])?;
        self.awake = false;
        Ok(())
    }
}

fn stringify(error: EspError) -> String {
    format!("ERR display GPIO: {error}\r\n")
}

/// Build the SPI device from the board's panel pins (GPIO10 SCK, GPIO11 MOSI,
/// GPIO9 CS, GPIO8 DC, GPIO12 RST, GPIO13 BUSY).
pub fn build<'d>(
    spi: esp_idf_svc::hal::spi::SPI2<'d>,
    sclk: esp_idf_svc::hal::gpio::Gpio10<'d>,
    mosi: esp_idf_svc::hal::gpio::Gpio11<'d>,
    cs: esp_idf_svc::hal::gpio::Gpio9<'d>,
    dc: esp_idf_svc::hal::gpio::Gpio8<'d>,
    rst: esp_idf_svc::hal::gpio::Gpio12<'d>,
    busy: esp_idf_svc::hal::gpio::Gpio13<'d>,
) -> Result<Display<'d>, String> {
    use esp_idf_svc::hal::gpio::AnyIOPin;
    let driver = SpiDriver::new(spi, sclk, mosi, None::<AnyIOPin>, &Default::default())
        .map_err(|e| format!("ERR display SPI bus: {e}\r\n"))?;
    let config = SpiConfig::new().baudrate(MegaHertz(10).into());
    let device = SpiDeviceDriver::new(driver, Some(cs), &config)
        .map_err(|e| format!("ERR display SPI device: {e}\r\n"))?;
    let dc = PinDriver::output(dc).map_err(stringify)?;
    let rst = PinDriver::output(rst).map_err(stringify)?;
    let busy = PinDriver::input(busy, Pull::Up).map_err(stringify)?;
    Ok(Display::new(device, dc, rst, busy))
}
