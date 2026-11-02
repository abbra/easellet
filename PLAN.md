# Rust firmware plan — Waveshare ESP32-S3-PhotoPainter (SKU 32408)

## Goal and interpretation

Build a small Rust application that can exercise every *hardware* capability through USB, without depending on the factory cloud/AI service. Minimize handwritten code and maintenance, rather than promise an entirely Rust binary or the smallest flash image. ESP-IDF, its radio libraries, and initially some codec support will remain C underneath safe Rust interfaces.

If “Rust” means no C dependencies whatsoever, reconsider the backend before implementation; this is a substantially different scope. Reproducing the factory Xiaozhi conversation and paid image-generation integrations is optional application work, not necessary to exercise the hardware.

No firmware was built or flashed during research. The working directory was initially empty, not a Git repository. No ESP USB device appeared in the initial `lsusb` inventory. Host has `cargo`, but `rustup` and `espflash` were not found on PATH.

## Findings

### Hardware and practical limits

- ESP32-S3-WROOM-1-N16R8: dual-core Xtensa LX7, Wi-Fi 2.4 GHz, BLE, 16 MiB flash, 8 MiB PSRAM. BLE, not Bluetooth Classic.
- 800×480 six-color Spectra 6 e-paper: black, white, yellow, red, blue, green. Full refresh is approximately 25 seconds; fast refresh is explicitly unsupported. Serialize updates; do not use this as a live dashboard.
- FAT32 microSD, dual microphones with ES7210 ADC, ES8311 DAC and external speaker connector.
- SHTC3 temperature/humidity sensor, power-management IC, buttons and indicators. PCF85063 RTC is documented on the current product and present on V2 schematic.
- USB-C supports firmware programming and logging; ROM download mode remains the recovery route. USB mass storage is not an advertised factory capability.
- Optional main battery and rechargeable-only RTC backup battery. Verify connector polarity, battery specifications, and speaker load before use.
- Published sleep-current figure is ≤1 mA; measure battery-side current, with USB disconnected. This is a reference, not a verified result or a guarantee of our firmware.

### Critical revision issue: do not copy power initialization blindly

Both V1 and V2 schematics are linked by Waveshare. V1 schematic names AXP2101; V2 names TG28 and PCF85063ATL. The inspected example variants still call `Custom_PmicPortInit(..., 0x34)` and use `XPowersAXP2101` with explicit charging and regulator writes.

**Resolved on hardware:** register 0x03 reads **0x4A**, the AXP2101 chip ID, and the vendor factory test uses XPowersLib's AXP2101 driver. The TG28 label in the V2 schematic text is wrong for the PMIC. AXP2101 register semantics apply; the old TG28 datasheet must not be used for decoding. A response at 0x34 alone would still not have proven identity, which is why the chip-ID register was read.

First identify physical PCB revision and reconcile the PMIC chip ID/register map with the corresponding datasheet. Use revision-specific configuration. No charger, regulator, shutdown, or RTC-backup charging writes until this is resolved. Battery voltage/current/temperature settings are safety-sensitive.

### Provisional wiring from vendor code

Source root: `research/vendor/01_Example/xiaozhi-esp32/`. Treat these as reference wiring, not verification of the user's physical board.

| Function | GPIO / interface | Source |
|---|---|---|
| E-paper MOSI / clock | 11 / 10 | `components/user_app_bsp/user_app.cpp`, `port_bsp/display_bsp.cpp` |
| E-paper DC / CS / reset / busy | 8 / 9 / 12 / 13 | same; constructor argument order confirmed |
| Shared I2C SDA / SCL | 47 / 48 | `user_app.cpp`, board `config.h` |
| SDMMC CLK / CMD / D0 / D1 / D2 / D3 | 39 / 41 / 40 / 1 / 2 / 38 | `port_bsp/sdcard_bsp.h`; 4-bit default |
| Audio MCLK / BCLK / WS | 14 / 15 / 16 | `main/boards/waveshare-s3-PhotoPainter/config.h` |
| Audio input / output | 18 / 17 | same, from MCU perspective |
| Speaker amplifier enable | 7 | same |
| BOOT / KEY / PWR | 0 / 4 / 5 | same, `port_bsp/button_bsp.c` |
| Red / green indicators | 45 / 42, active low | `port_bsp/led_bsp.h` |
| PMIC interrupt | 21 | schematics; verify before enabling |
| Native USB | reserve S3 GPIO19/20 | confirm routing against schematic |

Reserve GPIO26–32 and GPIO33–37 for flash/Octal PSRAM; do not repurpose them. Confirm RTC interrupt wiring, all rail dependencies, and any revision-specific changes from schematics before finalizing a board definition.

Display reference uses SPI mode 0 at 40 MHz, reset 50/20/50 ms, busy low while busy, and a 192,000-byte packed buffer (two pixels/byte). Palette codes are 0 black, 1 white, 2 yellow, 3 red, 5 blue, 6 green: do not assume contiguous codes. Start with a conservative validated SPI frequency and preserve documented init/power sequencing. Add bounded busy waits instead of copying the vendor's infinite loop.

Factory basic/LAN modes require pre-dithered 24-bit 800×480 or 480×800 BMP. The underlying example code has broader decode functions, but documentation only promises baseline JPEG in the cloud mode. Start with host-converted packed images; image decoding on-device is optional.

## Backend decision

**Recommend Rust `std` on ESP-IDF**, target `xtensa-esp32s3-espidf`:

- `esp-idf-hal`: GPIO/SPI/I2C/I2S safe wrappers and embedded-hal traits.
- `esp-idf-svc`: Wi-Fi, networking, HTTP, NVS and service wrappers where available.
- `esp-idf-sys`: isolated raw bindings for remaining IDF APIs (SDMMC/FAT mount, sleep, BLE and codec integration as needed).
- Existing embedded-hal sensor/RTC crates where their exact device support is verified. A small datasheet-based driver can be less work than adapting an incompatible crate.
- Espressif `esp_codec_dev` for ES8311/ES7210, behind a narrow C shim if required. Reuse codec setup, not the factory AI application, audio processing stack, or its task graph.
- Check existing e-paper crates for this exact E6 panel; do not assume a 7.3-inch seven-color driver is compatible. Otherwise port only panel initialization, transfer, refresh and sleep into a small Rust driver.

Alternative: `esp-hal` / `no_std`, with Embassy and the compatible radio stack. ESP32-S3 is supported and this is attractive for Rust-native control, but achieving filesystem, radio, codecs, and power-management coverage is likely more handwritten integration. Choose it only if avoiding ESP-IDF is more important than minimal code. Neither route should be described as removing proprietary radio dependencies without checking them.

### Version gate

The inspected vendor `xiaozhi-esp32/sdkconfig` records IDF 5.5.2; another example directory is named IDF6.1. Neither proves compatibility with current released Rust wrappers. ESP-Rust documentation explicitly warns wrappers may lag IDF.

Generate from the official ESP-IDF template, build its hello-world using a mutually supported released HAL/SVC/SYS/toolchain/IDF set, then pin it. Prefer the template's supported release baseline over forcing vendor IDF6.1. Record versions and Cargo.lock; no floating `master` dependencies. Enable 16 MiB flash and validated Octal 8 MiB PSRAM configuration. Ensure `sdkconfig.defaults` is actually consumed by the root crate; metadata/environment precedence and workspace paths matter.

## Minimal architecture

Start with **one firmware crate** with modules `board`, `display`, `sensors`, `storage`, `audio`, `power`, `console`. Extract a platform-independent driver crate only if host testing or reuse benefits justify it.

- One owner for each peripheral; one shared I2C bus abstraction, no independently initialized codec/sensor buses on the same pins.
- Small command enum and one dispatcher; synchronous APIs first. A worker/queue for long refresh/audio operations only when needed, not an async framework everywhere.
- Central board/revision definition, typed errors, resource cleanup and narrow documented unsafe/FFI boundaries.
- Bounded USB command input and binary image transfer; return queued/busy/completed accurately. Logs must not corrupt binary framing.
- A 192 kB packed framebuffer in validated PSRAM; use IDF-compatible DMA buffers or chunked staging as required. Avoid several full RGB and rotated copies. Initial dithering/scaling occurs on the host.
- No LVGL, web frontend, cloud account, wake-word engine, OTA framework or generic plugin architecture for first bring-up.

USB serial console is the principal feature-exercise surface: `info`, `buttons`, `led`, `sensor`, `rtc`, `sd`, `display`, `audio`, `wifi`, `ble`, `power`, `sleep`. Start with native USB Serial/JTAG and verify the Rust/IDF console path; custom TinyUSB CDC is only a fallback or later USB-class feature. Development firmware should not auto-sleep and disappear from USB.

## Delivery stages and acceptance gates

1. **Inventory and recoverability**
   - Connect device; record USB IDs, ROM chip/flash information, PCB revision and installed peripherals. Do not install toolchains or write flash without agreeing the next implementation step.
   - Inspect secure-boot/encryption status with read-only tooling; never burn eFuses. Back up readable full flash (16 MiB if confirmed), hash it and record tool version. Back up SD separately and obtain the correct factory merged binary.
   - Treat backups as sensitive: Wi-Fi/API credentials may be present. Encryption can limit backup usability; prove restoration procedure is appropriate before replacement.
   - Recovery: vendor FAQ says long-press PWR ~5s, hold BOOT and click PWR to enter download mode. Confirm procedure on this revision. No full erase by default.
   - Gate: recovery path, board identity and restore artifacts documented.

2. **Small Rust boot image**
   - Install isolated Xtensa toolchain via the documented ESP Rust setup/espup, linker/build prerequisites, flash/monitor tools. Check distro permissions rather than running serial tools permanently as root.
   - Build hello-world with selected pins, flash/PSRAM config and an explicit partition table. Rust ELF is not a merged factory binary: flash using the build's actual bootloader/partition/app offsets. 0x00 applies to the vendor merged image, not a raw Rust app.
   - Gate: repeated cold boots, USB command echo, reset reason, verified PSRAM size and memory test, both user buttons and indicator tests. PMIC configuration disabled.

3. **Shared I2C and read-only diagnostics**
   - Explicit known-device probes with timeouts. SHTC3 read/CRC/sleep; RTC read and validity flags if present; validated PMIC identity and read-only status.
   - Gate: plausible stable sensor readings, clear absence/error reporting, no initialization writes to an unrecognized PMIC. Add RTC set/time/alarm only after backup charging/IRQ routing is understood.

4. **Display and SD**
   - Native Rust panel driver; six-color bars, text via embedded-graphics if useful, orientations, refresh and panel sleep. Allow for full refresh time; rate-limit requests.
   - IDF FAT/SDMMC mount without automatic formatting. Read-only listing first; writes only to a test directory with user permission.
   - Host image converter to exact palette and nibble packing; display from SD or checksummed USB upload. Validate sizes, palette values, paths and partial transfers.
   - Gate: six correct colors, 800×480 geometry/orientation, reliable repeated refresh, missing-card handling and controlled busy-timeout errors.

5. **Audio**
   - Configure verified codec clocks/routes, I2S format, amplifier enable and safe initial gain/volume. Capture both microphone channels, expose level/RMS and write WAV to SD; play a quiet test tone/WAV through a suitable external speaker.
   - Gate: channel isolation, intelligible recording/playback, no underruns at chosen sample rate; codecs/amplifier shut down cleanly. Echo cancellation and wake words are separate optional capabilities.

6. **Radio and persistence**
   - Wi-Fi scan/connect, DHCP, SNTP and HTTPS request; optionally bounded image upload/download. No default open writable AP or embedded credentials.
   - BLE scan plus small GATT diagnostic service; evaluate NimBLE/raw-IDF integration against the pinned release. Test coexistence with Wi-Fi and audio.
   - Gate: independent hardware coverage, bounded memory, reconnect behavior and credentials not leaked in logs. Add NVS settings only as necessary.

7. **Power management and autonomous demo**
   - Implement revision-specific PMIC telemetry and validated rail transitions; preserve safe charger defaults until battery parameters are known. No generic arbitrary register-write command.
   - Quiesce SD, panel, audio and radio before sleep. Test KEY/BOOT wake and timer wake; RTC alarm wake only if the actual interrupt route supports it. Distinguish ESP deep sleep, PMIC power-off and RTC-backed timed power-on.
   - Gate: repeated wake cycles, persisted minimal state, external battery-side current measurements. Final demo: wake → sample → draw/load image → refresh → sleep, with USB manual-test mode always available.

8. **Optional applications**
   - Device-side JPEG/dithering, slideshow, weather, LAN upload UI, cloud AI, OTA, additional USB classes. Each is opt-in; cloud features should use user-selected endpoints and never copied vendor keys.

## Testing and definition of done

Every confirmed fitted peripheral has a USB command, documented expected output and a reproducible hardware acceptance test. Missing optional battery/speaker/SD is reported, not treated as a firmware crash. Pure logic gets host tests (packing, palette, rotation, CRC, command parsing and malformed uploads); mock buses test driver transactions; hardware tests cover timing and rails. Run fmt/clippy against supported targets and CI build from pinned dependencies. Verify third-party licenses before copying driver code.

Track failures as recoverable `Result`s with context, reserve panics for invariants, and impose bounds on busy waits, I2C retries, USB inputs and network downloads. Do not claim “full platform coverage” until audio, BLE, RTC/revision-specific features and battery-power behavior have actually been tested.

## Evidence and local research

- Product: https://www.waveshare.com/esp32-s3-photopainter.htm?sku=32408
- Overview: https://docs.waveshare.com/ESP32-S3-PhotoPainter (`research/overview.html`)
- Resources, use, flashing, FAQ and ESP-IDF pages: `research/*.txt`; URLs preserved in each capture.
- Both schematic PDFs/text and the TG28 datasheet (NOT applicable to the actual PMIC): `research/schematic-v{1,2}.*`, `research/TG28.*`.
- AXP2101 register definitions and vendor init: `research/vendor/.../components/pmicpower/`.
- Vendor examples: https://github.com/waveshareteam/ESP32-S3-PhotoPainter at commit `f7e92b16a8faf9975ce8469ad7d66f8ec5420b44`, local `research/vendor/` shallow clone. This is reference material, not project firmware.
- Rust ESP-IDF wrappers: https://github.com/esp-rs/esp-idf (`research/esp-idf-hal-current.md`, `research/esp-idf-svc-current.md`, `research/esp-idf-build-options-current.md`).
- Template: https://github.com/esp-rs/esp-idf-template (`research/esp-idf-template.md`).
- Bare-metal alternative: https://github.com/esp-rs/esp-hal (`research/esp-hal.md`).
- Setup reference: https://docs.espressif.com/projects/rust/book/

Next action: this roadmap is largely delivered — stages 1-6 (inventory, display, SD,
I²C/RTC/PMIC diagnostics, audio, Wi-Fi and persisted settings) are implemented and
verified on hardware. The live task queue is in `TODO.md`; see it for the next
stage and for the loose ends this plan does not track.
