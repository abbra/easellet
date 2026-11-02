# Easellet

Easellet is a Rust/ESP-IDF firmware for the Waveshare ESP32-S3-PhotoPainter. It
provides hardware diagnostics, a card slideshow, a browser portal and a voice
assistant through a LAN bridge. `PLAN.md` gives the roadmap.
`research/device/INVENTORY.md` gives the hardware identity, the battery data and the
verified factory backup.

**The firmware supports GPIO, I²C diagnostics, display refresh and SD mount.** The
commands `help`, `info`, `buttons`, `led`, `sensor`, `rtc`, `pmu`, `sd` and
`display` are available. PSRAM reports 8 MiB. The
green LED and the KEY press respond. The SHTC3 temperature and humidity readings are
stable, and their CRCs are valid. The PCF85063 answers, but it reports a stopped
oscillator: the time is not set, and there is no backup cell. The PMIC is an
**AXP2101** (chip ID 0x4A), not the TG28 that the V2 schematic text names. Two
PMIC writes with a narrow scope run at boot: ALDO4 (`EPD_VCC`) and ALDO3
(`Audio_VCC`) are set to 3.3 V and enabled. Both writes read the registers back.
The firmware does not touch charging or the other rails. The ALDO3/Audio_VCC write
supports the audio stage. Review that write before the next flash. The vendor
16 GB-class FAT card mounts at `/sdcard`. The firmware lists its seven top-level
directories and does not write to or format the card.

The 7.3-inch ED2208 / Spectra 6 panel refreshes an 800x480 six-color image in about
30 s. The panel survives deep sleep and a new initialization. A bar refresh without the SD
card takes 30.9 s and 30.3 s in two measurements, and the console stays available
during the refresh. With the card inserted at 20 MHz, the vendor image
loads and refreshes in 33.9 s. The driver and the converter apply the vendor's full
180-degree rotation: both axes are flipped, x=0 is the viewer's left edge, and the
bar order is black, yellow, red, blue, green, white. The audio stage uses ES8311
playback and ES7210 capture at 24 kHz/16-bit stereo. The commands `audio level`,
`audio tone`, `audio rec` (which writes `/sdcard/EASELTEST/REC.WAV`) and `audio play
<file.wav>` are available. The SD test directory is writable. Playback and capture
share the one full-duplex I²S port, so the firmware enables both directions
together. ESP-IDF makes the later-initialized TX channel the clock consumer. A TX
channel alone never drains. Remaining work: radio, more PMIC configuration, RTC
time setting and system sleep.

`TODO.md` holds the live task queue for stages that are not built yet. `PLAN.md` is
the original roadmap and is historical.

## Build on this machine

```bash
source scripts/env.sh
bash scripts/check.sh
```

The tools live in `.tools/`. The setup does not change Fedora's Rust installation
or the shell profile. Espup also created its usual `~/.espup/esp-clang` symlink.
ESP-IDF and its tools are downloaded under `.embuild/`. `.venv-tools/` holds
esptool. Git excludes these directories and the sensitive flash backups.

Installed toolchain: espup 0.19.0, Xtensa Rust 1.97.0.0, ldproxy 0.3.4,
esptool 5.6.0. `esp` is a local toolchain alias. It is not an automatically pinned
release. Reinstall that exact version to reproduce this setup. ESP-IDF is pinned to
v5.5.5 (commit `b774170ff46c393eeb5e495ea37936038d3f4f4f`). `Cargo.lock` fixes the
dependency resolution, including esp-idf-svc 0.53.0, hal 0.47.0 and sys 0.38.1.
Template reference: esp-rs/esp-idf-template commit
`b814c2b968f84949707ed0a6ab72d262e3fbfbc4`.

The build writes these outputs to `target/xtensa-esp32s3-espidf/debug/`:

- `easellet`: an ELF file with debugging symbols.
- `easellet.bin`: the application image. It is not a full flash image.
- `bootloader.bin` and `partition-table.bin`: the boot layout that IDF generates
  for the sdkconfig table. These files are only a build-time size check. The frame
  uses `partitions.csv` (see “Flashing gate”), and `scripts/check.sh` measures the
  build against the slot in that file.

The check script runs these steps:

1. Formatting.
2. Native tests for the parser, decoder, framebuffer and image loader.
3. Clippy for the cross target.
4. The firmware build.
5. Image conversion.

`cargo test` alone does not run the parser tests. The firmware binary has its
embedded test harness disabled.

## USB diagnostic protocol (after approved flashing)

Open the USB serial port. Do not reset DTR or RTS. Send ASCII commands one at a
time. End each command with LF or CRLF. The firmware does not send a banner. Start
with `help`.

| Command | Behavior |
|---|---|
| `help` | Shows the supported commands. |
| `info` | Shows the firmware version, the configured board and the actual PSRAM heap totals. |
| `buttons` | Shows a snapshot of KEY (GPIO4) and BOOT (GPIO0), active low, and PWR (GPIO5), active high. |
| `led on` / `led off` | Controls the green indicator (GPIO42), active low. A refresh blinks it, then restores this setting. |
| `sensor` | Shows the SHTC3 ID and CRC, the temperature and the humidity from I²C 0x70. |
| `rtc` | Shows the PCF85063 time or the validity flags from I²C 0x51 (read-only). |
| `pmu` | Shows the AXP2101 chip ID, status, charge settings, battery percentage and voltage, ALDO3/ALDO4 and power-key settings. |
| `sd` | Shows the status of the mounted FAT card and a read-only root listing (`/sdcard`). |
| `sd ls [<dir>]` | Shows a bounded read-only listing of a directory below `/sdcard`, or of the root. |
| `display bars` | Draws six vertical color bars, left to right black, yellow, red, blue, green, white. |
| `display bmp <file>` | Loads an uncompressed 800x480 24-bit BMP from `/sdcard`, maps it to the six colors and refreshes the panel. |
| `display raw <file>` | Loads an exact 192000-byte host-converted `.el6` frame from `/sdcard` and refreshes the panel. |
| `display clear <color>` | Fills the panel with one of the six colors. |
| `display sleep` | Puts the panel into deep sleep. The next draw runs the initialization again. |
| `display status` | Shows the awake flag and the BUSY pin state. |
| `audio status` | Shows the codec pair, rate/format, PA state, live volume and mic gain, last capture stats and the audio rail report. |
| `audio regs` | Shows 16 ES8311 and 14 ES7210 registers on `/dev/ttyACM0`. |
| `audio tone [ms]` | Plays a 440 Hz stereo tone through the speaker for 100..2000 ms. |
| `audio loop [ms]` | Plays the tone and captures it at the same time. Reports capture statistics. |
| `audio level <ms>` | Captures audio and reports rms, peak and correlation for 100..5000 ms. |
| `audio rec <1..10>` | Gives a double beep cue, then records to `/sdcard/EASELTEST/REC.WAV`. |
| `audio hex` | Shows the first and last captured frames as hex pairs. |
| `audio play <file>` | Plays a 24 kHz/16-bit/stereo WAV from `/sdcard`. |
| `audio volume <db>` | Sets the ES8311 DAC volume from -24 to +12 dB and applies it immediately. |
| `audio gain <db>` | Sets the ES7210 microphone gain from 0 to 37.5 dB. Applies at the next capture. |
| `audio off` | Sets PA low, suspends ES8311 and stops ES7210 and I²S. |
| `wifi scan` | Shows nearby SSIDs with RSSI and auth mode. Starts the radio on demand. |
| `wifi status` | Shows link state, address, netmask, gateway, DNS and SNTP state. |
| `wifi join [<ssid> [password]]` | Joins an AP. With no arguments, uses the credentials from the card. |
| `wifi time [server]` | Runs an SNTP sync, then writes the RTC. Connects first when credentials are saved. |
| `wifi off` | Disconnects and powers down the radio. |
| `wifi auto on` / `wifi auto off` | Sets the boot-join flag. Takes effect at the next boot. `config save` makes it survive a power cycle. |
| `config` | Shows the effective settings, their source and the file path. |
| `config save` | Writes `/sdcard/EASELTEST/CONFIG.TXT`, including the passphrase. |
| `ai status` | Shows the resolved assistant endpoints, language, voice and model. The token is redacted. |
| `ai selftest` | Builds every assistant request from the settings and sends nothing. |
| `ai stt [ms]` | Waits for KEY, captures, converts to µ-law, posts to `/stt` and prints the transcript. |
| `ai ask <text>` | Posts to `/llm` and then `/tts`, and plays the answer. |
| `ai url <url>` / `ai key <token>` | Sets the bridge endpoint live. `-` clears it. `config save` persists it. |
| `ai idle off\|key\|voice` | Selects what can start a turn when nobody types. `config save` persists it. |
| `ai threshold <20..4000>` | Sets the loudness gate for the voice watcher, in 16-bit sample units. |
| `gallery status` | Shows the slideshow state: directory, image count, next picture, interval and counts. |
| `gallery start [<dir> [<secs>]]` | Starts or restarts the slideshow over the `.bmp`/`.el6` files in one directory. |
| `gallery next` | Draws the next picture now, without waiting for the interval. |
| `gallery stop` | Stops advancing. The panel keeps the picture that it shows. |

The I²C bus is GPIO47 (SDA) / GPIO48 (SCL) at 100 kHz. The `diag` decoders cover
CRC, identifiers, BCD time validity and AXP2101 charge encodings. `panel` packs
4 bpp six-color pixels for the 800x480 ED2208 panel.

The firmware rejects a line in full if the line is longer than 160 bytes or
contains control characters. Printable bytes pass through, so a Russian `ai ask`
prompt arrives as UTF-8. Image paths must be relative to `/sdcard`. The firmware
rejects absolute paths, backslashes and `.`/`..` components. BMP input must be
800x480, 24-bit and uncompressed, and the firmware reads it only. There is no
editing/echo, reboot, raw register access or shutdown command. USB transfers have
bounded waits. A HAL failure stops in a yielding loop; it does not cause an
automatic reboot or power cycle. Console output is disabled in IDF to avoid a
conflict with the HAL USB driver. As a result, early boot failures can be silent.

Octal PSRAM is configured at 40 MHz with IDF's startup memory test. The test fails
if PSRAM is missing. A successful build is not evidence that the memory test
passed. `info` reports allocator totals, not an exhaustive runtime memory-test
result.

The application initializes the SD card when the card is present. It never
initializes NVS, BLE or Wi-Fi on its own initiative. The radio starts for a `wifi`
command, or at boot only when the settings file contains `wifi.auto=1`. The audio
codecs stay idle until an `audio` command.

PMIC handling is split by purpose. The boot path touches only ALDO4 (`EPD_VCC`):
3.3 V, enabled, both registers read back, because the panel does not work without
it. ALDO3 (`Audio_VCC`) belongs to the audio path. The audio path **reads it
first**: it observes the voltage and the enable bit, writes only if the rail is off
or not at 3.3 V, then reads again. `info` and `audio status` print the result, so a
rail write is never implicit:

```
OK audio_rail=ALDO3/Audio_VCC found_mv=3300 found_en=1 writes=0 now_mv=3300 now_en=1
```

`writes=0` means that the PMIC already powered the codecs and this firmware
changed nothing. No path touches a charging parameter, another regulator, RTC
charging or a sleep setting. Panel commands write only panel RAM and the vendor
initialization sequence. The PMIC can continue its own autonomous behavior. This
is not a guarantee that the factory charging settings are safe or unchanged across
a power cycle.

The firmware normalizes a recording at capture (`pcm::normalize`). It scales the
loudest sample to -6 dBFS with at most +24 dB of gain, so distant speech is usable
and the ES8311 can stay at unity. Silence is left untouched. A loud file is never
attenuated. Playback defaults to unity, so no file can clip the DAC through a
hidden boost.

## Buttons and lights

The main loop reads the buttons every 20 ms (`src/buttons.rs`). A click counts on
release. A hold counts when it reaches 1.5 s.

| Button | Click | Hold |
| --- | --- | --- |
| BOOT | Shows the next picture. Starts the slideshow from `gallery.dir` if it is off. | Goes to deep sleep now. |
| KEY | Starts a voice turn when `ai.idle=key`. | Reserved for the mode menu (P7.3). |
| PWR | Goes to deep sleep now. | Keep holding: the PMIC powers off at about 6 s. |

The red light is on while the frame is operable. The green light blinks while the
panel refreshes.

## Deep sleep

The frame sleeps in the same way as the vendor firmware (`src/power.rs`,
`src/sleep.rs`):

- **What wakes it:** BOOT or KEY (GPIO0/GPIO4, low), PWR (GPIO5, high), and a timer
  when one is set.
- **Timer or BOOT wake:** the slideshow continues after the picture that was shown
  before the sleep. The frame remembers that picture in RTC memory. There is no
  Wi-Fi. The frame sleeps again when the refresh is done. This is the battery
  slideshow.
- **KEY, PWR or power-on:** an ordinary boot (Wi-Fi, portal, OpenDisplay, voice). A
  KEY press or console input during a timer/BOOT wake turns it into an ordinary
  boot.
- **When it sleeps:** on a PWR click, a BOOT hold, `sleep [secs]`, an OpenDisplay
  DeepSleep request, or after `power.awake_secs` with no activity if
  `power.sleep_secs` is not zero. It never sleeps during a refresh, while a voice
  answer is spoken, or while an OpenDisplay client is connected.
- **Before it sleeps** it finishes the refresh, waits for the buttons to be
  released, says goodbye over mDNS, stops the radio, the codecs and the panel, and
  turns both lights off.

On battery with an empty cell (5 % or less, or under 3.4 V) the frame paints
«Батарея разряжена» once and sleeps until a button is pressed. Timed wakes stop
until the battery is charged. The battery voltage, the SHTC3 temperature and
humidity, and the chip temperature also go out in the OpenDisplay MSD record and
the mDNS `msd` key.

With `bridge.url=http://<bridge>:<port>`, a timed wake joins Wi-Fi and asks the
bridge for this frame's picture (`GET /frames/<id>/next`, with the bridge token as
bearer and the ETag of what the panel shows). The frame paints a new picture. A 304
answer means that nothing changed, and the frame sleeps again after about 5 s
without a refresh. A 204 answer or any error falls back to the card's slideshow.
Without `bridge.url`, a timed wake stays radio-off. That is the lower-power default.

`power.sleep_secs=0` (the default) never sleeps by itself. A PWR click still
sleeps, and the frame then wakes only on a button. `info` shows
`wake=cold|timer|boot|key|pwr` and the wake count since power-on.

The USB console disappears while the chip sleeps. The port is created again on
every wake. `tools/udev/70-easellet-esp32s3.rules` gives the logged-in user
access each time:

```bash
sudo install -m 0644 tools/udev/70-easellet-esp32s3.rules /etc/udev/rules.d/
sudo udevadm control --reload
```

## Settings on the card

There is no key/value store. The firmware never initializes NVS. The radio runs with
flash storage disabled. The USB-Serial/JTAG peripheral resets the chip whenever a
host opens the port, so RAM settings cannot survive between sessions. The only
durable place is the card, at `/sdcard/EASELTEST/CONFIG.TXT`. This is an 8.3 name
because FATFS is built with `CONFIG_FATFS_LFN_NONE`.

```
# keys: wifi.auto (1 = join at boot and stay online), wifi.ssid,
#       wifi.password, audio.volume_db (-24.0..12.0),
#       audio.gain_db (0.0..37.5), ai.url, ai.key, ai.lang,
#       ai.voice, ai.model, ai.idle=off|key|voice, ai.threshold,
#       gallery.dir, gallery.secs (120..3600), gallery.auto (1 = start at boot),
#       opendisplay.reboot (0 = ignore OpenDisplay clients' restart requests),
#       opendisplay.mdns (0 = do not advertise the frame; the bridge does),
#       power.sleep_secs (0 = never sleep by itself, or 60..86400),
#       power.awake_secs (30..3600, default 120)
wifi.auto=1
wifi.ssid=example
wifi.password=...
audio.volume_db=-12.0
audio.gain_db=30.0
ai.idle=key
```

Parsing is bounded (1 KiB, 32 lines) and strict. The parser rejects unknown keys
and malformed lines with a line number, so it reports a typo instead of ignoring
it. An invalid file is never fatal and never writes anything. The built-in defaults
stay in force, and `info` prints the parse error.

The passphrase is stored in the clear on purpose. Reading the card requires
physical access to the device. At that point the network is reachable anyway, so
obfuscation would buy nothing and would cost a retyping after every boot. `config`
prints `password=<set>`, never the value.

`wifi.auto=1` joins at boot and **stays** connected, so later features find a link
that is already up. The cost is the radio's idle current. If the RTC holds no valid
time, one SNTP sync runs while the link is up and the firmware writes the clock.
Otherwise the firmware does not attempt a sync. A failed join powers the radio back
down, since there is nothing to serve, and boot continues normally.

`config save` writes the file from live state, so `audio volume -6` followed by
`config save` survives a reboot. The values that `audio status` reports are the
values that are persisted. Nothing is written at boot, and the firmware creates only
this one file under `EASELTEST`.

## OpenDisplay and the bridge

The frame is an OpenDisplay device. It listens on TCP port 2446 and advertises
`_opendisplay._tcp` over mDNS. A client reads the panel configuration, sends an
image and receives the refresh result. The firmware also answers `ReadMsd`,
`Reboot` and `DeepSleep`.

A second mode is available. The bridge (`bridge/`) can be the OpenDisplay device.
Home Assistant and other clients then connect to the bridge, and the bridge sends
the image to one selected frame. Use the **OpenDisplay server** section on the
Frames tab:

1. Select a frame.
2. Set `Serve the selected frame through the bridge`.
3. Set the discovery name. The default is `Easellet`.
4. Click **Save**.

The bridge reads the frame's configuration and MSD. It sends each image through
its normal OpenDisplay client and waits for the frame's `0x73` or `0x74` result.
The bridge returns that result to its own client. It accepts one client at a time
and refuses an image when the frame is asleep or busy.

The bridge stores the choice in `bridge.toml`:

```
opendisplay_enabled = true
opendisplay_frame = "94a990caed46"
opendisplay_name = "Easellet"
opendisplay_addr = "0.0.0.0:2446"
```

The endpoint is off by default. The bridge uses one endpoint for one frame. The
OpenDisplay protocol has no field that selects a frame, so a second frame needs a
second bridge endpoint.

If both the frame and the bridge advertise, a client discovers two endpoints for
the same panel. To make the bridge the only endpoint, set this key on the card and
reboot:

```
opendisplay.mdns=0
```

The frame still listens on TCP port 2446 after this change. Only its mDNS
advertisement stops.

Checked on the LAN:

```bash
avahi-browse -rt _opendisplay._tcp
opendisplay info --host 192.168.50.93
opendisplay upload --host 192.168.50.93 picture.png
```


## Host helpers

```bash
.venv-tools/bin/python tools/convert.py input.png ~/tmp/frame/frame.el6
python tools/convert.py --no-dither input.png ~/tmp/frame/frame.el6   # image -> .el6
.venv-tools/bin/python tools/easellet-join.py                            # join Wi-Fi, save it
.venv-tools/bin/python tools/easellet-console.py "info" "audio status"    # one-shot commands
```

`tools/easellet-join.py` reads the passphrase with `getpass` (no echo, never printed). It
does the join, the optional SNTP sync, `wifi auto on` and `config save` **in a
single session**. The port must stay open, because closing it resets the chip and
the driver keeps its network configuration in RAM. With `wifi.auto=1` and saved credentials, a cold boot reports
`source=loaded … wifi_auto=connected` with `ip=192.168.50.204` and no join command.

## Voice assistant bridge

Speech recognition and synthesis run on a machine on the LAN, not in the firmware
and not in a cloud service: `bridge/`. The frame never holds a provider key. It
needs no TLS, certificate bundle, base64 or JSON. It uploads µ-law, receives text
and asks for speech.

```bash
./bridge/build.sh test          # bridge unit tests (host toolchain, no models)
./bridge/build.sh run           # stubs: recognition reports duration and peak
./bridge/build.sh serve -- --stt whisper:small \
    --llm cmd:bridge/scripts/llm-local.py \
    --tts cmd:bridge/scripts/tts-piper.sh \
    --wake 'рамк,фоторам,frame'                  # only answer speech aimed at the frame
./bridge/build.sh transcribe small ru speech.wav  # same models, no frame involved
```

### Plugins and jobs

Plugins (`bridge/plugins/README.md`) produce cards or pictures. The Plugins tab
lists each plugin as a closed section and opens one section at a time. This prevents
the page from showing all fields for nine plugins at once. A selector at the top
jumps to a named plugin. The tab previews a plugin and saves it as a **job**: a plugin, its
parameters, the frames, an interval and a priority. The bridge keeps jobs in
`~/.config/easellet-bridge/jobs.toml` (owner-only). Secret parameters are
write-only from the UI. The scheduler runs due jobs every half minute. It keeps the
latest picture for each frame, pushes the picture to frames that are awake, and
leaves it for sleeping frames to fetch on their timed wake (`bridge.url` on the
frame). The bridge never sends a picture again if it is identical to the one that a
frame already shows. The run log is under the jobs.

### The assistant's tools

A question that matches a plugin's `[assistant]` phrases («Рамка, покажи погоду»)
is not sent to the model. The bridge runs that plugin for the frame that asked. The
frame says «Показываю: …». The plugin's picture replaces what the panel shows after
the frame finishes speaking. The `/llm` answer carries `X-Panel: keep`, so the frame
does not paint the transcript over the picture. You can also ask the Assistant tab
in the web UI by text. The browser plays the answer.

The bridge recognizes commands for the frame itself before it checks plugins and
the model (`bridge/src/commands.rs`). The frame carries them out after it speaks
(`X-Action`, from a fixed list in `src/actions.rs`):

| Say | The frame |
|---|---|
| «следующая картинка», «покажи другую фотографию» | Shows the next picture of the slideshow. |
| «предыдущая картинка» | Shows the one before. |
| «покажи картинку номер 5», «покажи пятую картинку» | Shows picture 5 in name order. |
| «включи / выключи слайд-шоу» | Starts or stops the slideshow. |
| «меняй картинки каждые 10 минут», «…каждый час» | Sets the slideshow interval (2-60 min, this session). |
| «какая температура?», «…влажность?», «…заряд батареи?» | Speaks its own SHTC3 or fuel-gauge reading. |

A picture command first starts a stopped slideshow from `gallery.dir`. If the panel
is still refreshing, the frame queues the picture and draws it when the panel is
free. The interval is not saved. Use `config save` on the console to keep it.

The weather plugin takes the place from the question («покажи погоду в Тампере»,
«какая погода в Нижнем Новгороде», "show the weather in Espoo"). It undoes Russian
case endings by trying base forms against Open-Meteo's geocoder. The geocoder must
return that exact name. If the plugin cannot find a place, it says so aloud («Не
нашёл место «…»») and shows the configured place instead.

The agenda plugin reads an iCalendar (`.ics`) calendar: a private address, a
`file://` address, or a path on the bridge host. By voice, the day comes from the
question («что у меня завтра», «расписание на неделю», "what is on tomorrow"), so
the card shows that day or that week instead of the default number of days. The
private address of a calendar is a key, and the bridge stores it as a secret
parameter.

Recognition is steered in two ways. The frame's language (`ai.lang`) gives that
language a head start in Whisper's language detection, so a lone «Рамка» is not
heard as English "Ram". A clear sentence in another language still wins.
`stt_prompt` in `bridge.toml` (Settings tab: «Подсказка распознаванию») is text
that Whisper treats as just heard. Put the wake word and the names that you say in
it («Рамка, покажи погоду в Эспоо.»), and «в Эспоо» stops coming out as «ВСПУ».
Across 30 captures of room noise and chatter, it produces no echo of itself. `build.sh transcribe small auto file.wav…` with `EASEL_STT_HINT` and
`EASEL_STT_PROMPT` replays captures the same way.

### Web UI and settings

The bridge serves a web UI on its own port (`http://<bridge>:8095/`). The UI shows
what the frames asked and were answered (rejections included, with the reason and
the timings) and the bridge's settings. Frames, plugins and pictures from the
bridge follow (`research/bridge-ui-design.md`, TODO P8).

Settings live in `~/.config/easellet-bridge/bridge.toml` (`--config` moves the
file). The file is owner-only because it holds the frames' token and the UI
password. The first `serve-local.sh` run creates the file from the script's
defaults. After that, the file and the UI are what count. A flag or `EASEL_*` variable
given explicitly wins for that run and shows as read-only in the UI. The wake stems,
the token, the model's system prompt and URL, the UI password and extra host names
apply at once. The listen address and the three backends apply after a restart.

The UI is protected like the frame's portal: an optional password (`ui_password`,
HTTP Basic), an `X-Easellet` header on every POST, and a `Host` check (IP
addresses, local names, this machine's own name, `ui_hosts`). The frames' endpoints
keep their bearer token. The two never share credentials.

The local model needs one server that stays running, with the weights resident, so
a sentence does not pay the load cost:

```bash
~/local/19cce691/llama-server \
    -m ~/tmp/small-llms/gguf/Phi-4-mini-instruct-Q4_K_M.gguf \
    --host 127.0.0.1 --port 8096 -c 4096 -t 24 --jinja
```

Recognition is Whisper on the host, in-process. `small` is the smallest model that
carries Russian. A 10 s utterance takes about 9 s in a release build. The crate is
vendored with a one-token fix (`bridge/vendor/rwhisper.patch`) because upstream
0.4.1 never asks the model to transcribe. That defect turns Russian speech into
English prose. Answers come from a local `llama-server` that runs Phi-4-mini through
`bridge/scripts/llm-local.py` (1.3 s for a one-sentence reply on CPU). No provider
key exists anywhere in the path. Speech is piper, a neural Russian voice
(`bridge/scripts/tts-piper.sh`, one 60 MB model fetched once). `espeak-ng` remains
the dependency-free fallback, but its Russian sounds mechanical. See
`bridge/README.md` for details, the model comparison and the measurements.

Then point the frame at the bridge. Either type the address at the console and
persist it,

```
ai url http://192.168.50.93:8095    # live at once; `-` clears it
ai key easellet-lan-2026         # optional, must match --token; never echoed
config save                        # writes both to EASELTEST/CONFIG.TXT
```

or put the same two lines in `EASELTEST/CONFIG.TXT` with the card out of the device:

```
ai.url=http://192.168.50.93:8095
ai.key=<token>
```

`ai status` shows the resolved endpoints. `ai selftest` builds all three requests
offline and decodes a synthetic reply. Both console setters apply immediately and
survive only if `config save` runs, exactly like the volume and gain commands.

Two commands actually talk to the bridge:

| Command | Behavior |
|---|---|
| `ai stt [ms]` | Waits for the KEY button, gives a double-beep cue, captures (default 3000 ms, 500..10000), downmixes, resamples 24k→16k, converts to µ-law, posts to `/stt` and prints the transcript. |
| `ai listen [ms]` | Does the same as `ai stt`, and draws the transcript on the panel. |
| `ai talk [ms]` | Does the whole round trip: KEY, capture, transcript, `/llm`, `/tts`, speech, then both texts on the panel. |
| `ai ask <text>` | Posts to `/llm`, then posts the answer to `/tts`, decodes µ-law, resamples 16k→24k, plays it and shuts the PA down. |
| `ai idle off\|key\|voice` | Selects who can start a turn when nobody types: nobody (default), any KEY press, or sustained loudness on the microphones. `config save` persists the mode as `ai.idle`. |
| `ai threshold <20..4000>` | Sets the detector's floor live and reports the resulting trigger, without a flash. Use it to tune a room during a conversation. |

Both report per-stage timings (`waited_ms` for the KEY press, `cue_ms`, `capture_ms`,
`roundtrip_ms`, or `llm_ms`, `tts_ms`, `play_ms`). A turn that produces nothing
looks identical from the outside, whichever stage failed. `ai stt` refuses a capture
that carries no more energy than an idle room:
`ERR audio utterance: microphone heard nothing (peak_l=… rms_l=…)`. Silence then
costs nothing instead of a round trip and an invented sentence. The prompt in
`ai ask` may be Russian: the console passes UTF-8 through and rejects only control
bytes. See `bridge/README.md` for the wire protocol and the `cmd:` backends that
reach Whisper or Piper without any Rust bindings.

`ai idle` turns the frame from a demonstration into a device that works without a
laptop. `key` makes any KEY press start a full turn, with no laptop and no console. `voice` keeps the
microphone open and starts a turn when somebody actually speaks. The watcher runs
inside the console loop between reads, not in its own thread. The codec and the
radio keep a single owner, and the loop already wakes every 20 ms. Only a panel
refresh moves to a short-lived worker: a Spectra 6 update takes about 31 s, and
blocking the loop for it would close the microphone for half a minute after every
answer. An idle turn therefore speaks, reopens the microphone immediately and
queues the panel text. The refresh itself waits for 15 s without voice. A rapid
series of questions then updates the panel once with the last answer. The frame does
not spend the whole conversation painting answers that are already obsolete. A
second question asked while the ink is still settling is still heard.

The voice mode is a plain energy detector (`src/vad.rs`, host-tested), not a wake
word. Its trigger is `max(4 x measured room noise, floor)` in 16-bit RMS units,
capped at 4000. The floor is 600 by default (about -35 dBFS), and `ai threshold`
moves it live, without a flash, because the room is not part of this crate. The
values measured on this board are worth keeping. Room chatter that is *not* meant
for the frame and a question spoken a metre away both land in 150..460 RMS, so no
gate can tell them apart. The floor only decides how often a capture is worth
attempting. Speech frames from a clear question reach 800..1900 RMS with peaks above
3000. A floor of 300 hears the question. A floor of 600 hears only the louder half
of one. Three further details exist because of what speech looks like:

- **Pre-roll.** The trigger can fire only when a voice is loud. At that point the
  first syllable is in the past. The frame keeps one second of audio and prepends
  it, so «Сколько будет…» does not arrive as «будет…».
- **Hangover.** A stop consonant is tens of milliseconds of quiet. An utterance ends
  only after 700 ms of quiet; otherwise it would be cut mid-sentence.
- **Cap.** A fan or a television can hold the detector above the threshold forever,
  so an utterance is limited to 10 s.

`voice` costs power: the ADC and the ALDO3 audio rail stay on for as long as the mode
is enabled. That is why it is not the default. `ai idle off` (or `audio off`) closes
them again. The frame speaks a voice turn only after the microphone has been closed,
so it cannot transcribe its own answer.

A gate is not enough on its own, and the bridge supplies the missing half. `--wake`
(or `EASEL_WAKE`) names the stems that a transcript must contain before the bridge
answers it. `рамк,фоторам,frame` covers «рамка», «рамку» and the English name.
Without it, a house where people simply talk starts a capture every half minute.
Whisper invents a sentence from the noise or reads the conversation correctly, and
the frame answers something that nobody asked. Noise alone can produce an English
"I'm here to help." With the gate, the detector may open the microphone as often as
the room deserves. Anything not addressed is dropped at `/stt`, and the
frame stays silent. The bridge keeps no record in its history or log. The name is
asked for only when the detector opened the capture. Every `/stt` carries
`X-Trigger: key|voice|console`, and a KEY press or a console command is already the
frame being spoken to. The bridge therefore answers those transcripts as they are,
and, like the name, they open a short window for a spoken follow-up.

A wall-mounted frame has no terminal after a reset, so the mode survives on the card.
`config save` writes `ai.idle=off|key|voice`, and boot applies it before the console
loop starts. Restoring `key` is silent because it opens nothing. Restoring `voice`
reports the monitor setup and powers the ALDO3 rail, exactly as the command does. If
the microphone cannot be opened at boot, the watcher reverts to `off` and reports the
error. The frame never claims to be listening when it is not.

`ai stt`, `ai listen` and `ai talk` are one button-first path. Each polls KEY (GPIO4,
never PWR) and waits up to 30 s for a press. Only then does it bring up the radio, so
a join never has to be held through. `ai stt` prints the transcript alone.
`ai listen` also paints it. `ai talk` continues to the answer. A Spectra 6 refresh
costs tens of seconds, which is why painting is the last stage of a turn: after the
console report exists and, in `ai talk`, after the answer has been spoken.

Text on the panel needs Cyrillic, so it is not a hand-drawn 5x7 font.
`tools/mkfont.py` rasterizes DejaVu Sans at 26 px into `src/font.rs` (169 glyphs,
13 KiB of one-bit bitmaps, committed so a build never needs a host font).
`panel::draw_text`, `panel::wrap_text` and `panel::draw_paragraph` blit and wrap it
in the panel's own 4bpp, 180-degree-rotated layout. The gate checks the blitter bit
for bit against that table natively, and checks that wrapping keeps every line
inside its column. Regenerate the table only when the glyph set or the size changes:

```bash
tools/mkfont.py > src/font.rs
```

## Slideshow

`gallery start` walks the `.bmp` and `.el6` files in one directory in sorted order
and refreshes the panel every interval. The default directory is `EASELTEST` and the
default interval is five minutes. Both fall back to `gallery.dir` and `gallery.secs`
from the settings file, so a bare `gallery start` resumes what the card remembers.
`gallery next` advances once. `gallery auto` at boot comes from `gallery.auto=1`.

Three things shape the implementation:

- **The interval has a floor of 120 s** because one Spectra 6 refresh takes about
  31 s. Clamping to one refresh would mean that the frame draws for 31 s of every
  30 s cycle. The panel would never be still, and nobody could look at a picture that
  it had just finished. The floor is therefore a refresh plus a long dwell: two
  minutes leave roughly ninety seconds of a finished, unchanging picture. The
  firmware clamps the request and the reply says so:
  `OK gallery=on … requested_s=5 clamped_to=120`.
- **An advance waits for a quiet room and a free panel.** A refresh prevents the frame from
  hearing for the next half minute, so the slideshow yields to the voice watcher. The deadline does not move while somebody is talking, and the
  picture appears as soon as the conversation pauses. The firmware counts skips once
  per deadline, not once per 10 ms loop tick.
- **Decoding happens on the main loop, and painting does not.** The card has a
  single owner and a BMP is a 1.15 MB read, so the loader runs where every other
  card access runs. Only the slow refresh goes to the paint worker thread that
  already serves spoken answers.

The firmware reports failures once and postpones the next attempt by a full interval,
so an unreadable file cannot spin the loop. The firmware drops the entry itself for
this round, so a card with mixed sizes and JPEGs still cycles instead of sticking:

```
ERR gallery EASELTEST/BROKEN.BMP: ERR image bmp signature
```

Mixed sizes are the normal case. The vendor card's `06_USE~1` holds three 800x480
BMPs next to 1200x675, 480x480 and 736x1325 ones, and its `01_SYS~1` BMPs are 64x64
label cards. At a 30 s interval, three minutes give `advances` 4 and `skipped` 4,
then the slideshow wraps back to the first entry.

## Portal: upload, gallery and settings in a browser

The frame serves its own page on port 80 as soon as it has a link (`wifi join`,
`wifi.auto=1`, then `web start`; `web stop` turns it off for the session):

```bash
web                # OK web listening port=80 locked=0 auth=none route=/
web status         # same line
python3 -c "import socket;print(socket.gethostbyname('easellet'))"   # or `wifi status`
```

Most of the work happens on the device, because the card and the panel each have
exactly one owner. The web thread touches no card and no panel. It posts a job,
the main loop performs it, and the reply is the HTTP body. The one exception is a
firmware upload, which writes the other app slot on the web thread itself. That slot
has no other owner, and a 1 MB body cannot be handed over as one job (`src/ota.rs`).

| Route | What it does |
| --- | --- |
| `GET /` | Serves the page (`web/index.html` + `tools/el6.mjs`). |
| `GET /gallery` | Serves the slideshow directory as JSON: `{"dir":…,"images":[{"name","bytes"}]}`. |
| `GET /image?path=…` | Decodes one picture to a 192 000-byte frame, BMP or `.el6`. |
| `POST /upload` | Takes 192 000 bytes in, writes them to `<gallery.dir>/UPnnnn.EL6` and paints the picture. |
| `POST /show?path=…` | Decodes an existing picture and starts a refresh. |
| `POST /delete?path=…` | Removes one picture. Refused outside the writable directory. |
| `GET /settings` | Serves the current settings as JSON. Secrets are reduced to `true`/`false`. |
| `POST /settings` | Takes form fields, applies them and writes `EASELTEST/CONFIG.TXT`. |
| `GET /sensors` | Serves battery data (present, USB, charging, mV, %) and room climate as JSON. Read every 30 s. |
| `GET /ota` | Shows which app slot runs, its state, the slot that an upload would replace, and that slot's size. |
| `POST /ota` | Takes a `.bin`, writes it to the inactive slot, which becomes the boot slot. |
| `GET /status` | Serves one line with the frame size and the route list. |

Every POST must carry an `X-Easellet` header (any value). Without it, the frame
answers 403. A page on another site cannot add a custom header without a CORS
preflight, which the frame never answers. This stops a random web page on the LAN
from changing `ai.url` (where audio and `ai.key` go), deleting pictures or clearing
the password. With curl:
`curl -H 'X-Easellet: 1' --data-binary @pic.el6 http://<frame>/upload`.

The frame also answers only requests whose `Host` is an IPv4 address or a local name
(no dot, or ending in `.local`, `.lan`, `.home`, `.home.arpa`, `.internal`). This
defeats DNS rebinding: a page that rebinds its own domain to the frame still sends
that domain as `Host` and gets 403. A request must arrive whole within 30 s, so a
client that trickles bytes cannot hold the one-connection server. A firmware upload
gets 180 s, because every 4 KB of it waits for a flash write.

**A picture can arrive in three ways**, and they are not the same thing:

1. A file dragged from the desktop (`dataTransfer.files`).
2. A picture dragged from another tab, which carries only a URL, with `files` empty.
3. A paste.

All three go through one resolver, so a drag from a browser tab works instead of
highlighting the box and doing nothing. The page fetches a URL from another site,
which the browser allows only if that site sends `Access-Control-Allow-Origin`. When
the site does not, the page says so and asks for the file instead of failing
silently. `tools/check-portal-js.mjs` runs the resolver from the page's own source
against all three shapes in the gate (`portal-js: drop sources ok`).

**Four tabs, because three different people use this page.** «Photo» (drop a
picture, see the frame it will become, send it) and «Gallery» (what is on the wall,
and «Show» to repaint the panel with a particular picture) are the everyday part.
Every control that reconfigures the device (Wi-Fi, audio, the assistant, the
slideshow, the page password) lives behind «Settings». That tab opens with a line
that says it changes the frame for everyone. «Firmware» is the one-off update. The
tab is the URL hash (`#photos`, `#gallery`, `#settings`, `#firmware`), so a link
opens the right place and the back button works.

**The interface speaks English and Russian, English by default.** A page that greets
a stranger in a language that they did not choose is a page that they cannot read.
Both this page and the bridge's own UI therefore open in English, with a selector in
the header that switches live and remembers the choice (`localStorage`). Every static
string is a `data-i18n` attribute. Every string that the script builds goes through
`t()` against a small `STRINGS` dictionary, so adding a language is one object plus
its autonym in `LANGUAGES`, with no code. `ai.lang` is a different thing: that is the
language in which the frame listens and answers.

**The collection can be large.** A card holds up to `storage::MAX_IMAGES` (512)
pictures, and the page never asks for all of them. It holds the names, draws one page
of twelve thumbnails, pages through them, filters by name, and keeps the last 48
decoded frames in a cache so paging back is instant. The page fetches frames one at a
time, because the frame decodes from a single card owner and parallel requests would
only queue on it.

**Deleting asks twice**, in place. The first click arms the button («Delete 12?»,
red). The second click within five seconds acts. There is no modal, because a dialog
that interrupts is a dialog that everybody dismisses without reading. The firmware
refuses a deletion outside `EASELTEST`. The vendor's own pictures are readable and
permanent, and the page therefore shows them without a checkbox and with «not
deletable» instead of a button that is bound to fail. The policy lives in
`image::removable`, with tests for `06_USE~1`, `CONFIG.TXT`, `..` and backslash
tricks.

**Working on the page without the frame.** `tools/portal-mock.py` serves
`web/index.html` and `tools/el6.mjs` over a fake API. The fake API answers with the
same shapes and status codes as `src/web.rs`, with as many generated pictures as you
ask for:

```bash
tools/portal-mock.py --count 60 --port 8130      # then open http://127.0.0.1:8130/
```

It also serves `tools/portal-harness.html`, which drives the real page in a
same-origin iframe. The harness first checks the interface language (English by
default, the switch and the stored choice). It then checks paging, search, the
two-click delete, the refusal outside `EASELTEST`, the tabs and the settings form. The
rest runs in Russian, so the translation is exercised and not merely present:

```
ok язык по умолчанию: en «Photo»      ok переключение на русский: Фотография
ok список: 62 в EASELTEST/UPLOADS        ok после удаления: 52 в EASELTEST/UPLOADS
ok на странице 12 карточек: 12        ok картинка прошивки: ... "не удаляется"
ok вторая страница ...                ok удаление вне EASELTEST: 403 ...
ok вкладка «Прошивка»: работает ota_0 · подтверждена ...
```

**The gallery is shown as the panel draws it.** `el6.mjs` has `unpack` and
`drawFrame`, the exact inverse of `pack`: same 180-degree rotation, same nibble
order, same six colours. The page fetches every card in the grid as a frame and draws
it through that path. A `.el6` written by `tools/convert.py`, one uploaded from the
page, and a vendor BMP then all appear as they are on the wall. The gate pins the
round trip: `pack(unpack(frame), {dither: false})` is byte-identical to `frame`,
A wrong rotation or nibble order cannot pass this test.

Uploads go to the slideshow's own directory (`gallery.dir`, default
`EASELTEST/UPLOADS`, created on first use), so a picture sent from a browser is also a
picture that the frame will cycle through. If the slideshow is running, the new file
jumps the queue instead of waiting a full lap. The firmware writes uploads only
inside `EASELTEST`. If `gallery.dir` points at the vendor's pictures, the slideshow
still shows them, but the firmware refuses an upload.

**Firmware updates go through the same page.** The «Firmware» tab reads `GET /ota`
(slot, state, the slot that an upload would replace, its size). It takes a `.bin`
and arms its button twice: an update reboots the frame, and no single accidental
click should do that to a picture on the wall. The frame streams the body into the
inactive slot. `esp_ota_end` verifies it, the boot slot moves, and the frame
restarts. The page then polls `/status` and reloads itself when the frame answers
again. The slot, its state and the size that the next slot accepts are also on the
console (`ota`). Use that command when the page is not the thing that is broken.

**Settings** mirror the console commands and are validated by the same parser. The
page rewrites the form's fields as `key=value` lines and re-parses them with
`Settings::parse`, then saves. A value that the page accepts therefore cannot fail at
the next boot. Values with a live effect are applied immediately (volume, gain, voice
gate, `ai.idle`, slideshow). Wi-Fi changes say that they apply on the next join. An
empty field means "leave it alone". This is the correct behavior for an untouched
password box.

The answer language is a short list rather than a text box. It contains exactly the
four languages that the bridge's recognizer is allowed to pick
(`EASEL_STT_LANGS=ru,en,fi,fr` in `bridge/src/whisper.rs`), shown in their own scripts:
Русский, English, Suomi, Français. Offering a language that the bridge cannot hear
would be a promise that the frame cannot keep. The frame still answers in the
language of the question. This is the language in which it listens and speaks when
the text does not say. A tag that is *not* on the list (a hand-edited `CONFIG.TXT`,
or a language added to the bridge later) comes back as `de-DE (не из списка)`
instead of blanking the box. An empty field means "keep" and would hide the setting.

**Protection is prepared, not enabled.** `web.password=<8..64 printable bytes>` turns
on HTTP Basic for every route, with any user name and that password. The browser
supplies its own login box. It is off by default because the frame is a home
appliance on a home LAN. Basic over plain HTTP controls access, but it does not
encrypt the traffic. The page says so where the password is set.

The console owns the way out of a forgotten password, because an empty form field
means "keep" and a page that cannot be unlocked cannot clear its own lock:

```bash
web password <8..64 chars>   # set it; saved at once, the listener rebinds
web password off             # remove it; the page is open again
```

Both commands go into the USB log as typed, so the passphrase is only as private as
the cable. That is the same physical access that the card already gives. A set
password is echoed back as a character count, never as the value. Whichever path set
it, the running listener drops and rebinds with the new password immediately, so the
console and the page never disagree about what the lock is.

The form's password box also takes `-` to remove the lock, the same convention that
`ai key` uses.

## Host image converter

```bash
python tools/convert.py input.png ~/tmp/frame/frame.el6
```

The converter requires Pillow. It crops or fits to 800x480, applies Floyd-Steinberg
error diffusion against the panel's exact six RGB colors, and writes the native
4bpp/180-degree-rotated layout that `display raw` uses. An exact six-color source
converts without color drift. The gate checks the source-to-native mapping and the
192000-byte size offline. The firmware and the converter do not write to the card.

## Images: the wire format is `.el6`, and the browser does the conversion

The frame accepts only two things: a 24-bit BMP, or a raw `.el6` (192 000 bytes, 4bpp
indices, 180-degree native order). It does **not** decode JPEG or PNG. That is a
decision, not a gap:

- A phone photo is 12 MP. RGB for one is about 36 MB, and the S3 has no JPEG
  peripheral (that is P4-only). An on-device JPEG decoder would therefore mean a hard
  megapixel cap and a real chance of failing mid-upload.
- The six-colour mapping wants error diffusion, which needs the whole picture in
  floating point. That is another 4.6 MB that belongs on a real machine.
- A `.el6` is already in the panel's own encoding. The upload path therefore has
  nothing to validate beyond its size, and a bad upload cannot half-draw the panel.

So `tools/el6.mjs` does resize, Floyd-Steinberg and packing in the browser, where a
canvas and 8 GB of RAM already exist. The page posts 192 KB instead of megabytes:

```js
import { packImage } from "./el6.mjs";

const canvas = document.createElement("canvas");
canvas.width = 800; canvas.height = 480;
const context = canvas.getContext("2d", { willReadFrequently: true });
const frame = packImage(imageElement, context, { mode: "cover" }); // 192000 bytes
await fetch("/upload", { method: "POST", headers: { "X-Easellet": "1" }, body: frame });
```

`mode: "cover"` fills the panel and centre-crops (what `tools/convert.py` does).
`mode: "contain"` fits the whole picture and letterboxes in white.

The two converters must not drift, so the gate compares them. It gives the same
800×480 RGB input to both and requires a byte-identical `.el6` output in both
dithered and nearest-colour modes. It does this on a gradient, seeded noise, exact
palette blocks, a 64×64 upscale and any photos passed in:

```bash
python3 tools/check-el6-js.py ~/Downloads/*.jpg
#   ok   img20261031123204 dither 192000 bytes sha256:563c091aab7002fd
# OK 6 images, both modes, byte-identical
```

The resize itself is necessarily engine-specific (Pillow's LANCZOS against the
canvas's), so the comparison starts at the pixels. Everything after that is shared
logic and must match exactly. This method also finds the µ-law codec mismatch
against ffmpeg.

## Flashing gate

The board runs an approved image. The flash record is in
`research/device/flash.log`, and the console record is in
`research/device/console-test.log`. Every flash requires explicit approval. There is
deliberately no Cargo runner or automatic flash script. The default single-app partition table differs from the factory layout.
Installing this image therefore requires a review of the offsets and a write of the
bootloader, the table and the application. It is not a blind update of a factory OTA
slot. Factory settings can become inaccessible and must be recovered from the
backup. The SD card has not been backed up and this firmware never formats it.

### The table the frame runs

`partitions.csv` is the layout. The firmware writes it at flash time; the build does
not compile it in:

| Offset | Contents |
| --- | --- |
| `0x0` | `bootloader.bin` (rebuilt: rollback is a bootloader option) |
| `0x8000` | Partition table generated from `partitions.csv` |
| `0xf000` | `otadata` (erased: the bootloader then boots `ota_0`) |
| `0x20000` | `ota_0`, the application image |
| `0x320000` | `ota_1`, empty until the portal installs an update |

There are two 3 MB app slots and no factory slot. An update goes into the slot that
is not running, so a failed upload cannot leave the frame without firmware.
`scripts/check.sh` generates `target/partition-table.bin` from the CSV with ESP-IDF's
`gen_esp32part.py` (the same tool that the IDF build uses). The table is therefore
parsed and its offsets are verified on every run. Write that file and the two images
with esptool. `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y` in `sdkconfig.defaults` makes
the bootloader treat a new image as unverified until the frame marks it valid after
it reaches its console. An image that does not start is then replaced by the
previous one on the next reset.

Later updates go through the portal's **Firmware** tab. Pick `target/xtensa-esp32s3-espidf/release/easellet.bin` and arm the button
twice. The frame writes the other slot, verifies it, reboots into it and reconnects.
The console's `ota` command reports the slot, its state and the size that the next
slot accepts.

Before flashing, verify the device identity and the backup SHA-256 again, and
confirm ROM download recovery. Afterwards validate the USB commands, the KEY/BOOT
readings, the green LED, the PSRAM totals and the reset behavior before you add more
peripherals. Do not hold PWR for a diagnostic button test: it can cut power
independently of the firmware.

## License

GPL-3.0-or-later; see `LICENSE`. The OpenDisplay headers vendored in
`opendisplay-proto/upstream/` come from the GPL-3.0 OpenDisplay firmware.
