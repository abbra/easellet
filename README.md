# PhotoPainter Rev2.0 diagnostics

Minimal Rust/ESP-IDF bring-up for the connected Waveshare ESP32-S3-PhotoPainter.
See `PLAN.md` for the larger roadmap and `research/device/INVENTORY.md` for hardware
identity, battery data and verified factory backup.

**Flashed with user approval; GPIO, I2C diagnostics, display refresh and SD mount pass.**
`help`, `info`, `buttons`, LED, `sensor`, `rtc`, `pmu`, `sd` and `display` respond.
PSRAM reports 8 MiB; green LED and KEY press were confirmed physically. SHTC3
temperature/humidity readings are stable with valid CRCs. The PCF85063 answers but
reports a stopped oscillator (time never set, no backup cell). The PMIC is an
**AXP2101** (chip ID 0x4A), not the TG28 named in the V2 schematic text. Two
narrowly scoped PMIC writes now run at boot: ALDO4 (`EPD_VCC`) and ALDO3
(`Audio_VCC`) are set to 3.3 V and enabled with read-back verification. Charging
and all other rails remain untouched. The ALDO3/Audio_VCC write was added for the
audio stage and is the newest PMIC change; review it before the next flash.
The vendor 16 GB-class FAT card mounts at `/sdcard` and lists its seven top-level
directories without any write or format.

The 7.3-inch ED2208 / Spectra 6 panel refreshes an 800x480 six-color test image in
about 30 s and survives deep sleep plus re-init. Repeated bar refreshes completed in
30.9 s and 30.3 s without the SD card, while the console stayed available. After the
card was reinserted, it remounted at 20 MHz and the vendor image loaded and refreshed
in 33.9 s. The first image was upside down; the driver and converter now apply the
vendor's full 180-degree rotation, and the user confirmed the image is upright and
correct. The first visual check showed a horizontally mirrored image; drawing now
flips the native column order so x=0 is the viewer's left edge; a displayed image
still appeared upside down, so both axes are now flipped (native 180-degree
rotation). User confirmed the corrected bar order (black, yellow, red, blue, green,
white) and the vendor image. The audio stage now exercises ES8311 playback and
ES7210 capture at 24 kHz/16-bit stereo: `audio level`, `audio tone`, `audio rec`
(writes `/sdcard/PFTEST/REC.WAV`) and `audio play <file.wav>` all answer, and the
SD test directory was written with user approval. Playback and capture both use the
one full-duplex I2S port, so both directions are enabled together: ESP-IDF makes the
later-initialized TX channel the clock consumer, and TX alone never drains. Remaining
work: radio, further PMIC configuration, RTC time setting and system sleep.

`TODO.md` holds the live task queue for stages not yet built; `PLAN.md` is the
original roadmap and mostly historical now.

## Build on this machine

```bash
source scripts/env.sh
bash scripts/check.sh
```

Tools live in `.tools/`; no changes to Fedora's Rust installation or shell profile.
Espup also created its normal `~/.espup/esp-clang` symlink. ESP-IDF and its tools
are downloaded under `.embuild/`. `.venv-tools/` holds esptool. These directories
and sensitive flash backups are excluded from Git.

Installed toolchain: espup 0.19.0, Xtensa Rust 1.97.0.0, ldproxy 0.3.4,
esptool 5.6.0. `esp` is a local toolchain alias, not an automatically pinned release;
reinstall that exact version to reproduce this setup. ESP-IDF is pinned to v5.5.5
(commit `b774170ff46c393eeb5e495ea37936038d3f4f4f`). Cargo.lock fixes dependency
resolution, including esp-idf-svc 0.53.0, hal 0.47.0 and sys 0.38.1.
Template reference: esp-rs/esp-idf-template commit
`b814c2b968f84949707ed0a6ab72d262e3fbfbc4`.

Outputs under `target/xtensa-esp32s3-espidf/debug/`:

- `photoframe`: ELF with debugging symbols
- `photoframe.bin`: application image (not a full flash image)
- `bootloader.bin` and `partition-table.bin`: IDF-generated boot layout

The check script runs formatting, native parser/decoder/framebuffer/image-loader tests, cross-target Clippy,
the firmware build and image conversion. `cargo test` alone does not run the parser
tests because the firmware binary has its embedded test harness disabled.

## USB diagnostic protocol (after approved flashing)

Open the USB serial port without resetting DTR/RTS. Send ASCII commands terminated
by LF or CRLF, one at a time. No banner is emitted; start with `help`.

| Command | Behavior |
|---|---|
| `help` | Supported command list |
| `info` | Firmware version, configured board, actual PSRAM heap totals |
| `buttons` | Snapshot of KEY (GPIO4) and BOOT (GPIO0), active low, and PWR (GPIO5), active high |
| `led on` / `led off` | Green indicator (GPIO42), active low; a refresh blinks it, then restores this |
| `sensor` | SHTC3 ID/CRC, temperature and humidity from I²C 0x70 |
| `rtc` | PCF85063 time or validity flags from I²C 0x51 (read-only) |
| `pmu` | AXP2101 chip ID, status, charge settings, battery percentage and voltage, ALDO3/ALDO4, power-key settings |
| `sd` | Mounted FAT card status and read-only root listing (`/sdcard`) |
| `sd ls [<dir>]` | Bounded read-only listing of a directory below `/sdcard`, or of its root |
| `display bars` | Six vertical color bars, left to right black, yellow, red, blue, green, white |
| `display bmp <file>` | Load an uncompressed 800x480 24-bit BMP from `/sdcard`, map to the six colors, refresh |
| `display raw <file>` | Load an exact 192000-byte host-converted `.pf6` frame from `/sdcard`, refresh |
| `display clear <color>` | Fill the panel with one of those six colors |
| `display sleep` | Panel deep sleep; the next draw re-runs init |
| `display status` | Awake flag and BUSY pin state |
| `audio status` | Codec pair, rate/format, PA state, live volume and mic gain, last capture stats, audio rail report |
| `audio regs` | 16 ES8311 and 14 ES7210 registers under `/dev/ttyACM0` |
| `audio tone [ms]` | 440 Hz stereo tone through the speaker, 100..2000 ms |
| `audio loop [ms]` | Play the tone and capture it at once, reporting capture statistics |
| `audio level <ms>` | Capture and report rms/peak/correlation, 100..5000 ms |
| `audio rec <1..10>` | Double beep cue, then record to `/sdcard/PFTEST/REC.WAV` |
| `audio hex` | First and last captured frames as hex pairs |
| `audio play <file>` | Play a 24 kHz/16-bit/stereo WAV from `/sdcard` |
| `audio volume <db>` | ES8311 DAC volume, -24..+12 dB, applied live |
| `audio gain <db>` | ES7210 microphone gain, 0..37.5 dB, applied at the next capture |
| `audio off` | PA low, ES8311 suspended, ES7210 and I²S stopped |
| `wifi scan` | Nearby SSIDs with RSSI and auth mode; starts the radio on demand |
| `wifi status` | Link state, address, netmask, gateway, DNS, SNTP state |
| `wifi join [<ssid> [password]]` | Join an AP; with no arguments reuse the credentials from the card |
| `wifi time [server]` | SNTP sync, then write the RTC; connects first when credentials are saved |
| `wifi off` | Disconnect and power the radio down |
| `wifi auto on` / `wifi auto off` | Set the boot-join flag; takes effect at the next boot, and `config save` makes it survive a power cycle |
| `config` | Effective settings, their source, and the file path |
| `config save` | Write `/sdcard/PFTEST/CONFIG.TXT`, passphrase included |
| `ai status` | Resolved assistant endpoints, language, voice and model; token redacted |
| `ai selftest` | Build every assistant request from the settings without sending anything |
| `ai stt [ms]` | Capture, µ-law, POST to `/stt`, print the transcript |
| `ai ask <text>` | POST to `/llm` then `/tts`, play the answer |
| `ai url <url>` / `ai key <token>` | Set the bridge endpoint live; `-` clears it; `config save` persists |
| `ai idle off\|key\|voice` | What may start a turn with nobody typing; `config save` persists |
| `ai threshold <20..4000>` | Loudness gate for the voice watcher, in 16-bit sample units |
| `gallery status` | Slideshow state: directory, image count, next picture, interval, counts |
| `gallery start [<dir> [<secs>]]` | Start or restart the slideshow over the `.bmp`/`.pf6` files in one directory |
| `gallery next` | Draw the next picture now, without waiting for the interval |
| `gallery stop` | Stop advancing; the panel keeps what it shows |

The I²C bus is GPIO47 (SDA) / GPIO48 (SCL) at 100 kHz. `diag` decoders
cover CRC, identifiers, BCD time validity, and AXP2101 charge encodings. `panel`
packs 4 bpp six-color pixels for the 800x480 ED2208 panel.

Lines over 160 bytes or containing control characters are rejected in full; printable
bytes pass through, so a Russian `ai ask` prompt arrives as UTF-8. Image paths
must be relative to `/sdcard`; absolute paths, backslashes and `.`/`..` components are
rejected. BMP input must be 800x480, 24-bit, uncompressed, and is read-only.
There is no editing/echo, reboot, raw register access, or shutdown command.
USB transfers have bounded waits. A HAL failure stops in a yielding loop, not an
automatic reboot/power-cycle. Console output is disabled in IDF to avoid competing
with the HAL USB driver; consequently early boot failures may be silent.

Octal PSRAM is configured at 40 MHz with IDF's startup memory test and failure on
missing PSRAM. A successful build is not evidence that the memory test passed.
`info` reports allocator totals, not an exhaustive runtime memory-test result.

The application initializes the SD card when present; it never initializes NVS,
BLE, or Wi-Fi on its own initiative — the radio starts for a `wifi` command, or at
boot only when the settings file says `wifi.auto=1`. Audio codecs stay idle until
an `audio` command.

PMIC handling is split by purpose. The boot path touches only ALDO4 (`EPD_VCC`):
3.3 V, enabled, both registers read back, because the panel is dead without it.
ALDO3 (`Audio_VCC`) belongs to the audio path instead and is **read first**: its
voltage and enable bit are observed, written only if the rail is off or not 3.3 V,
then re-read. `info` and `audio status` print the result, so a rail write is never
implicit:

```
OK audio_rail=ALDO3/Audio_VCC found_mv=3300 found_en=1 writes=0 now_mv=3300 now_en=1
```

`writes=0` means the PMIC already powered the codecs and this firmware changed
nothing. No charging parameter, other regulator, RTC charging or sleep setting is
touched by any path; panel commands write only panel RAM and its vendor init
sequence. Existing PMIC autonomous behavior can continue; this is not a guarantee
that factory charging settings are safe or unchanged across a power cycle.

Recording is normalized at capture (`pcm::normalize`): the loudest sample is scaled
to -6 dBFS, with at most +24 dB of gain, so distant speech is usable and the ES8311
can stay at unity. Silence is left untouched and a loud file is never attenuated.
Playback defaults to unity, so no file can clip the DAC through a hidden boost.

## Buttons and lights

Read every 20 ms by the main loop (`src/buttons.rs`): a click counts on release,
a hold once it reaches 1.5 s.

| Button | Click | Hold |
| --- | --- | --- |
| BOOT | next picture (starts the slideshow from `gallery.dir` if it is off) | deep sleep now |
| KEY | voice turn when `ai.idle=key` | reserved for the mode menu (P7.3) |
| PWR | deep sleep now | keep holding: the PMIC powers off at about 6 s |

The red light is on while the frame is operable; the green one blinks while the
panel refreshes.

## Deep sleep

The frame sleeps the way the vendor firmware does (`src/power.rs`, `src/sleep.rs`):

- **What wakes it:** BOOT or KEY (GPIO0/GPIO4, low), PWR (GPIO5, high), and a
  timer when one is set.
- **Timer or BOOT wake:** the slideshow continues after the picture shown before
  the sleep (remembered in RTC memory), with no Wi-Fi, and the frame sleeps again
  once the refresh is done. That is the battery slideshow.
- **KEY, PWR or power-on:** an ordinary boot (Wi-Fi, portal, OpenDisplay, voice).
  KEY or console input during a timer/BOOT wake turns it into one.
- **When it sleeps:** on a PWR click, a BOOT hold, `sleep [secs]`, an OpenDisplay
  DeepSleep request, or after `power.awake_secs` with nothing happening if
  `power.sleep_secs` is non-zero. Never during a refresh, a voice turn being
  spoken, or with an OpenDisplay client connected.
- **Before sleeping** it finishes the refresh, waits for the buttons to be
  released, says goodbye over mDNS, stops the radio, the codecs and the panel,
  and turns both lights off.

On battery with an empty cell (5 % or less, or under 3.4 V) the frame paints
«Батарея разряжена» once and sleeps until a button; timed wakes stop until it is
charged. Battery voltage, the SHTC3's temperature and humidity and the chip
temperature also go out in the OpenDisplay MSD record and the mDNS `msd` key.

With `bridge.url=http://<bridge>:<port>` a timed wake joins Wi-Fi and asks the
bridge for this frame's picture (`GET /frames/<id>/next`, the bridge token as
bearer, the ETag of what the panel shows). A new picture is painted; 304 means
nothing changed and the frame sleeps again after ~5 s without a refresh; 204 or
any error falls back to the card's slideshow. Without `bridge.url` a timed wake
stays radio-off, which is the cheaper default.

`power.sleep_secs=0` (the default) never sleeps by itself; a PWR click still
sleeps, waking only on a button. `info` shows `wake=cold|timer|boot|key|pwr` and
the wake count since power-on.

The USB console disappears while the chip sleeps and the port is created again
on every wake. `tools/udev/70-photoframe-esp32s3.rules` gives the logged-in user
access each time:

```bash
sudo install -m 0644 tools/udev/70-photoframe-esp32s3.rules /etc/udev/rules.d/
sudo udevadm control --reload
```

## Settings on the card

There is no key/value store: NVS is never initialized, the radio runs with flash
storage disabled, and the USB-Serial/JTAG peripheral resets the chip whenever a
host opens the port, so RAM settings cannot survive between sessions. The only
durable place is the card, at `/sdcard/PFTEST/CONFIG.TXT` — an 8.3 name because
FATFS is built with `CONFIG_FATFS_LFN_NONE`.

```
# keys: wifi.auto (1 = join at boot and stay online), wifi.ssid,
#       wifi.password, audio.volume_db (-24.0..12.0),
#       audio.gain_db (0.0..37.5), ai.url, ai.key, ai.lang,
#       ai.voice, ai.model, ai.idle=off|key|voice, ai.threshold,
#       gallery.dir, gallery.secs (120..3600), gallery.auto (1 = start at boot),
#       opendisplay.reboot (0 = ignore OpenDisplay clients' restart requests),
#       power.sleep_secs (0 = never sleep by itself, or 60..86400),
#       power.awake_secs (30..3600, default 120)
wifi.auto=1
wifi.ssid=example
wifi.password=...
audio.volume_db=-12.0
audio.gain_db=30.0
ai.idle=key
```

Parsing is bounded (1 KiB, 32 lines) and strict: unknown keys and malformed lines
are rejected with a line number, so a typo is reported instead of ignored. An
invalid file is never fatal and never writes anything — the built-in defaults stay
in force and `info` prints the parse error.

The passphrase is stored in the clear, deliberately. Reading the card requires
physical access to the device, at which point the network is reachable anyway, so
obfuscation would buy nothing and cost a retyping after every boot. `config` prints
`password=<set>`, never the value.

`wifi.auto=1` joins at boot and **stays** connected, so later features find a link
already up; the cost is the radio's idle current. If the RTC holds no valid time,
one SNTP sync runs while the link is up and the clock is written — otherwise none
is attempted. A failed join powers the radio back down, since there is nothing to
serve, and boot continues normally.

`config save` writes the file from live state, so `audio volume -6` followed by
`config save` survives a reboot; the values reported by `audio status` are the ones
persisted. Nothing is written at boot, and only this one file under `PFTEST` is
ever created.

## Host helpers

```bash
.venv-tools/bin/python tools/convert.py input.png ~/tmp/frame/frame.pf6
python tools/convert.py --no-dither input.png ~/tmp/frame/frame.pf6   # image -> .pf6
.venv-tools/bin/python tools/pfjoin.py                            # join Wi-Fi, save it
.venv-tools/bin/python tools/pfconsole.py "info" "audio status"    # one-shot commands
```

`tools/pfjoin.py` reads the passphrase with `getpass` (no echo, never printed) and
does the join, the optional SNTP sync, `wifi auto on` and `config save` **in a
single session** — the port must stay open, because closing it resets the chip and
the driver keeps its network configuration in RAM. Verified outcome: the card held
`wifi.auto=1` with the credentials, and the next cold boot reported
`source=loaded … wifi_auto=connected` with `ip=192.168.50.204` and no join command.

## Voice assistant bridge

Speech recognition and synthesis run on a machine on the LAN, not in the firmware and
not in a cloud service: `bridge/`. The frame never holds a provider key and needs no
TLS, certificate bundle, base64 or JSON — it uploads µ-law, receives text, and asks
for speech.

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

Plugins (`bridge/plugins/README.md`) produce cards or pictures; the Plugins tab
previews them and saves them as **jobs**: a plugin, its parameters, the frames,
an interval and a priority, kept in `~/.config/photoframe-bridge/jobs.toml`
(owner-only; secret parameters are write-only from the UI). The scheduler runs
due jobs every half minute, keeps the latest picture per frame, pushes it to
frames that are awake, and leaves it for sleeping frames to fetch on their timed
wake (`bridge.url` on the frame). A picture identical to the one a frame already
shows is never sent again. The run log is under the jobs.

### The assistant's tools

A question that matches a plugin's `[assistant]` phrases («Рамка, покажи
погоду») is not sent to the model: the bridge runs that plugin for the frame that
asked, the frame says «Показываю: …», and the plugin's picture replaces what the
panel shows once the frame has finished speaking (the `/llm` answer carries
`X-Panel: keep`, so the frame does not paint the transcript over it). The
Assistant tab in the web UI can also be asked by text; the answer is played in
the browser.

Commands for the frame itself are recognised by the bridge before plugins and
the model (`bridge/src/commands.rs`), and the frame carries them out after
speaking (`X-Action`, from a fixed list in `src/actions.rs`):

| say                                            | the frame                           |
|------------------------------------------------|-------------------------------------|
| «следующая картинка», «покажи другую фотографию» | next picture of the slideshow     |
| «предыдущая картинка»                          | the one before                      |
| «покажи картинку номер 5», «покажи пятую картинку» | picture 5 in name order          |
| «включи / выключи слайд-шоу»                   | starts / stops the slideshow        |
| «меняй картинки каждые 10 минут», «…каждый час» | slideshow interval (2-60 min, this session) |
| «какая температура?», «…влажность?», «…заряд батареи?» | speaks its own SHTC3 / fuel-gauge reading |

A picture command starts a stopped slideshow from `gallery.dir` first; with
the panel still refreshing, the picture is queued and drawn when it is free.
The interval is not saved: `config save` on the console keeps it.

The weather plugin takes the place from the question («покажи погоду в
Тампере», «какая погода в Нижнем Новгороде», "show the weather in Espoo"):
Russian case endings are undone by trying base forms against Open-Meteo's
geocoder, which must return that exact name. A place it cannot find is said
aloud («Не нашёл место «…»»), and the configured place is shown instead.

Recognition is steered two ways. The frame's language (`ai.lang`) gives that
language a head start in Whisper's language detection, so a lone «Рамка» is no
longer heard as English "Ram"; a clear sentence in another language still wins.
`stt_prompt` in `bridge.toml` (Settings tab: «Подсказка распознаванию») is text
Whisper treats as just heard: put the wake word and the names you say in it
(«Рамка, покажи погоду в Эспоо.»), and «в Эспоо» stops coming out as «ВСПУ».
Replayed against 30 earlier captures of room noise and chatter it caused no
echo of itself. `build.sh transcribe small auto file.wav…` with `PF_STT_HINT`
and `PF_STT_PROMPT` replays captures the same way.

### Web UI and settings

The bridge serves a web UI on its own port (`http://<bridge>:8095/`): what the
frames asked and were answered (rejections included, with the reason and the
timings), and the bridge's settings. Frames, plugins and pictures from the bridge
follow (`research/bridge-ui-design.md`, TODO P8).

Settings live in `~/.config/photoframe-bridge/bridge.toml` (`--config` to move
it), owner-only because it holds the frames' token and the UI password. The first
`serve-local.sh` run creates it from the script's defaults; after that the file and
the UI are what count, and a flag or `PF_*` variable given explicitly wins for that
run and shows as read-only in the UI. The wake stems, the token, the model's system
prompt and URL, the UI password and extra host names apply at once; the listen
address and the three backends after a restart.

The UI is protected like the frame's portal: an optional password
(`ui_password`, HTTP Basic), an `X-Photoframe` header on every POST, and a `Host`
check (IP addresses, local names, this machine's own name, `ui_hosts`). The
frames' endpoints keep their bearer token; the two never share credentials.

The local model needs one server kept running, with the weights resident so a
sentence does not pay the load cost:

```bash
~/local/19cce691/llama-server \
    -m ~/tmp/small-llms/gguf/Phi-4-mini-instruct-Q4_K_M.gguf \
    --host 127.0.0.1 --port 8096 -c 4096 -t 24 --jinja
```

Recognition is Whisper on the host, in-process: `small` is the smallest model that
carries Russian, a 10 s utterance takes ~9 s in a release build, and the crate is
vendored with a one-token fix (`bridge/vendor/rwhisper.patch`) because upstream 0.4.1
never asks the model to transcribe, which turns Russian speech into English prose.
Answers come from a local `llama-server` running Phi-4-mini through
`bridge/scripts/llm-local.py` (1.3 s for a one-sentence reply on CPU) — no provider
key anywhere in the path. Speech is piper, a neural Russian voice
(`bridge/scripts/tts-piper.sh`, one 60 MB model fetched once); `espeak-ng` remains the
dependency-free fallback and is the reason the frame once sounded mechanical. Details,
the model comparison and the measurements: `bridge/README.md`.

Then point the frame at the bridge. Either type it at the console and persist it,

```
ai url http://192.168.50.93:8095    # live at once; `-` clears it
ai key photoframe-lan-2026         # optional, must match --token; never echoed
config save                        # writes both to PFTEST/CONFIG.TXT
```

or put the same two lines in `PFTEST/CONFIG.TXT` with the card out of the device:

```
ai.url=http://192.168.50.93:8095
ai.key=<token>
```

`ai status` shows the resolved endpoints; `ai selftest` builds all three requests
offline and decodes a synthetic reply. Both console setters apply immediately and
survive only if `config save` runs, exactly like the volume and gain commands.

Two commands actually talk to the bridge:

| Command | Behavior |
|---|---|
| `ai stt [ms]` | Double-beep cue, capture (default 3000 ms, 500..10000), downmix, resample 24k→16k, µ-law, `POST /stt`, print the transcript |
| `ai listen [ms]` | Waits for the KEY button, then as `ai stt`, and draws the transcript on the panel |
| `ai talk [ms]` | The whole round trip: KEY, capture, transcript, `/llm`, `/tts`, speech, then both texts on the panel |
| `ai ask <text>` | `POST /llm`, then `/tts` with the answer, decode µ-law, resample 16k→24k, play it and shut the PA down |
| `ai idle off\|key\|voice` | Who may start a turn with nobody typing: nobody (default), any KEY press, or sustained loudness on the microphones; `config save` persists the mode as `ai.idle` |
| `ai threshold <20..4000>` | Sets the detector's floor live and reports the resulting trigger, without a flash, so a room can be tuned during a conversation |

Both report per-stage timings (`cue_ms`, `capture_ms`, `roundtrip_ms`, or `llm_ms`,
`tts_ms`, `play_ms`), because a turn that produces nothing looks identical from the
outside whichever stage failed. `ai stt` refuses a capture that carries no more
energy than an idle room — `ERR audio utterance: microphone heard nothing (peak_l=…
rms_l=…)` — so silence costs nothing instead of a round trip and an invented
sentence. The prompt in `ai ask` may be Russian: the console passes UTF-8 through
and only rejects control bytes. See `bridge/README.md` for the wire protocol and the
`cmd:` backends that reach Whisper or Piper without any Rust bindings.

`ai idle` is the difference between a demo and a device on a wall. `key` makes any
KEY press start a full turn — no laptop, no console — and `voice` keeps the
microphone open and starts a turn when somebody actually speaks. The watcher runs
inside the console loop between reads rather than in its own thread: the codec and
the radio keep a single owner, and the loop already wakes every 20 ms. Only a panel
refresh moves to a short-lived worker: a Spectra 6 update takes about 31 s, and
blocking the loop for it would close the microphone for half a minute after every
answer. An idle turn therefore speaks, reopens the microphone immediately, and
queues the panel text. The refresh itself waits for 15 s without voice: a rapid
series of questions updates the panel once with the last answer instead of
spending the whole conversation painting answers that are already obsolete. A
second question asked while the ink is still settling is still heard.

The voice mode is a plain energy detector (`src/vad.rs`, host-tested), not a wake
word. Its trigger is `max(4 x measured room noise, floor)` in 16-bit RMS units,
capped at 4000, where the floor is 600 by default (≈ -35 dBFS) and `ai threshold`
moves it live — no flash — because the room is not part of this crate. The values
measured on this board are worth keeping: room chatter that is *not* meant for the
frame and a question spoken a metre away both land in 150..460 RMS, so no gate can
tell them apart. The floor only decides how often a capture is worth attempting.
Speech frames from a clear question reach 800..1900 RMS with peaks above 3 000, so a
floor of 300 hears the question and a floor of 600 hears only the louder half of one.
Three further details exist because of what speech looks like:

- **Pre-roll.** The trigger can only fire once a voice is loud, by which point the
  first syllable is in the past. One second of audio is kept and prepended, so
  «Сколько будет…» does not arrive as «будет…».
- **Hangover.** A stop consonant is tens of milliseconds of quiet; an utterance ends
  only after 700 ms of it, or it would be cut mid-sentence.
- **Cap.** A fan or a television can hold the detector above threshold forever, so an
  utterance is bounded at 10 s.

`voice` costs power: the ADC and the ALDO3 audio rail stay on for as long as the mode
is enabled, which is why it is not the default. `ai idle off` (or `audio off`) closes
them again. A voice turn is spoken to only after the microphone has been closed, so
the frame cannot transcribe its own answer.

A gate is not enough on its own, and the bridge supplies the missing half: `--wake`
(or `PF_WAKE`) names the stems a transcript must contain before it is answered —
`рамк,фоторам,frame` covers «рамка», «рамку» and the English name. Without it, a
house where people simply talk starts a capture every half minute, Whisper invents a
sentence from the noise or reads the conversation correctly, and the frame answers
something nobody asked — including, from a noise-only capture, an English "I'm here
to help." With it, the detector may open the microphone as often as the room
deserves; anything not addressed is dropped at `/stt` and the frame stays silent.
The name is asked for only when the detector opened the capture: every `/stt`
carries `X-Trigger: key|voice|console`, and a KEY press or a console command is
already the frame being spoken to, so those transcripts are answered as they are
(and, like the name, open a short window for a spoken follow-up).

A wall-mounted frame has no terminal after a reset, so the mode survives on the card:
`config save` writes `ai.idle=off|key|voice`, and boot applies it before the console
loop starts. Restoring `key` is silent because it opens nothing; restoring `voice`
reports the monitor setup and powers the ALDO3 rail, exactly as the command does. If
the microphone cannot be opened at boot the watcher reverts to `off` and reports the
error — the frame never claims to be listening when it is not.

`ai listen` and `ai talk` are the button half of the same thing: they poll KEY
(GPIO4, never PWR), wait up to 30 s for a press, and only then bring up the radio, so
a join never has to be held through. A Spectra 6 refresh costs tens of seconds, which
is why painting is the last stage of a turn — after the console report exists and,
in `ai talk`, after the answer has been spoken.

Text on the panel needs Cyrillic, so it is not a hand-drawn 5x7 font: `tools/mkfont.py`
rasterises DejaVu Sans at 26 px into `src/font.rs` (169 glyphs, 13 KiB of one-bit
bitmaps, committed so a build never needs a host font), and `panel::draw_text` /
`panel::wrap_text` / `panel::draw_paragraph` blit and wrap it in the panel's own
4bpp, 180-degree-rotated layout. The blitter is checked bit for bit against that
table natively, and wrapping is checked to keep every line inside its column. The
table is regenerated only when the glyph set or the size changes:

```bash
tools/mkfont.py > src/font.rs
```

## Slideshow

`gallery start` walks the `.bmp` and `.pf6` files in one directory, sorted, and
refreshes the panel every interval. The default directory is `PFTEST` and the
default interval five minutes; both fall back to `gallery.dir` and `gallery.secs`
from the settings file, so a bare `gallery start` resumes what the card remembers.
`gallery next` advances once; `gallery auto` at boot comes from `gallery.auto=1`.

Three things shape the implementation:

- **The interval has a floor of 120 s** because one Spectra 6 refresh takes about
  31 s. Clamping to one refresh would mean the frame is drawing for 31 s of every
  30 s cycle — the panel would never be still, and nobody could look at a picture
  it had just finished. So the floor is a refresh plus a long dwell: two minutes
  leaves roughly ninety seconds of a finished, unchanging picture. The request is
  clamped and the reply says so:
  `OK gallery=on … requested_s=5 clamped_to=120`.
- **An advance waits for a quiet room and a free panel.** A refresh is exactly what
  makes the frame deaf for the next half minute, so the slideshow yields to the
  voice watcher: the deadline does not move while somebody is talking, and the
  picture appears as soon as the conversation pauses. Skips are counted once per
  deadline, not once per 10 ms loop tick.
- **Decoding happens on the main loop, painting does not.** The card has a single
  owner and a BMP is a 1.15 MB read, so the loader runs where every other card
  access runs; only the slow refresh goes to the paint worker thread that already
  serves spoken answers.

Failures are reported once and postpone the next attempt by a full interval, so an
unreadable file cannot spin the loop; the entry itself is dropped for this round, so
a card holding mixed sizes and JPEGs still cycles rather than sticking:

```
ERR gallery PFTEST/BROKEN.BMP: ERR image bmp signature
```

That is the normal case, not an edge case: the vendor card's `06_USE~1` holds three
800x480 BMPs next to 1200x675, 480x480 and 736x1325 ones, and its `01_SYS~1` BMPs are
64x64 label cards. Measured on the board (30 s interval): `advances` 0 → 4 and
`skipped` 1 → 4 in three minutes, wrapping back to the first entry.

## Portal: upload, gallery and settings in a browser

The frame serves its own page on port 80 as soon as it has a link (`wifi join`,
`wifi.auto=1`, then `web start`; `web stop` turns it off for the session):

```bash
web                # OK web listening port=80 locked=0 auth=none route=/
web status         # same line
python3 -c "import socket;print(socket.gethostbyname('photoframe'))"   # or `wifi status`
```

Nine tenths of the work happens on the device, because the card and the panel
each have exactly one owner. The web thread never touches hardware: it posts a
job, the main loop performs it, and the reply is the HTTP body.

| route | what it does |
| --- | --- |
| `GET /` | the page (`web/index.html` + `tools/pf6.mjs`) |
| `GET /gallery` | the slideshow directory as JSON: `{"dir":…,"images":[{"name","bytes"}]}` |
| `GET /image?path=…` | one picture decoded to a 192 000-byte frame, BMP or `.pf6` |
| `POST /upload` | 192 000 bytes in; written to `<gallery.dir>/UPnnnn.PF6`, painted |
| `POST /show?path=…` | decode an existing picture and start a refresh |
| `POST /delete?path=…` | remove one picture, refused outside the writable directory |
| `GET /settings` | current settings as JSON, secrets reduced to `true`/`false` |
| `POST /settings` | form fields, applied and written to `PFTEST/CONFIG.TXT` |
| `GET /sensors` | battery (present, USB, charging, mV, %) and room climate as JSON, read every 30 s |
| `GET /status` | one line: frame size and the route list |

Every POST must carry an `X-Photoframe` header (any value); without it the
frame answers 403. A page on another site cannot add a custom header without a
CORS preflight, which the frame never answers, so this is what stops a random
web page open on the LAN from changing `ai.url` (where audio and `ai.key` go),
deleting pictures or clearing the password. With curl:
`curl -H 'X-Photoframe: 1' --data-binary @pic.pf6 http://<frame>/upload`.

The frame also answers only requests whose `Host` is an IPv4 address or a
local name (no dot, or ending in `.local`, `.lan`, `.home`, `.home.arpa`,
`.internal`), which defeats DNS rebinding: a page that rebinds its own domain
to the frame still sends that domain as `Host` and gets 403. A request must
arrive whole within 30 s, so a client trickling bytes cannot hold the
one-connection server.

**A picture can arrive three ways**, and they are not the same thing: a file
dragged from the desktop (`dataTransfer.files`), a picture dragged from another
tab - which carries only a URL, with `files` empty - and a paste. All three go
through one resolver, so a drag from a browser tab now works instead of
highlighting the box and doing nothing. A URL from another site is fetched by the
page, which the browser only allows if that site sends
`Access-Control-Allow-Origin`; when it does not, the page says so and asks for the
file instead of failing silently. `tools/check-portal-js.mjs` runs the resolver
from the page's own source against all three shapes in the gate
(`portal-js: drop sources ok`).

**Three tabs, because two different people use this page.** «Фотография» (drop a
picture, see the frame it will become, send it) and «Галерея» (what is on the
wall, and «На экран» to repaint the panel with a particular picture) are the
everyday part; every control that reconfigures the device - Wi-Fi, audio, the
assistant, the slideshow, the page password - lives behind «Настройки», which
opens with a line saying it changes the frame for everyone. The tab is the URL hash (`#photos`, `#gallery`, `#settings`), so a link
opens the right place and the back button works.

**The collection can be large.** A card holds up to `storage::MAX_IMAGES` (512)
pictures, and the page never asks for all of them: it holds the names, draws one
page of twelve thumbnails, pages through them, filters by name, and keeps the
last 48 decoded frames in a cache so paging back is instant. Frames are fetched
one at a time, because the frame decodes from a single card owner and parallel
requests would only queue on it.

**Deleting asks twice**, in place: the first click arms the button («Точно
удалить 12?», red), the second within five seconds acts. There is no modal,
because a dialog that interrupts is a dialog everybody dismisses without reading.
A deletion is refused by the firmware outside `PFTEST` - the vendor's own
pictures are readable and permanent - and the page therefore shows them without a
checkbox and with «не удаляется» instead of a button bound to fail. The policy
lives in `image::removable`, with tests for `06_USE~1`, `CONFIG.TXT`, `..` and
backslash tricks.

**Working on the page without the frame.** `tools/portal-mock.py` serves
`web/index.html` and `tools/pf6.mjs` over a fake API that answers with the same
shapes and status codes as `src/web.rs`, with as many generated pictures as you
ask for:

```bash
tools/portal-mock.py --count 60 --port 8130      # then open http://127.0.0.1:8130/
```

It also serves `tools/portal-harness.html`, which drives the real page in a
same-origin iframe and checks paging, search, the two-click delete, the refusal
outside `PFTEST`, the tabs and the settings form:

```
ok список: 62 в PFTEST/UPLOADS        ok после удаления: 52 в PFTEST/UPLOADS
ok на странице 12 карточек: 12        ok картинка прошивки: ... "не удаляется"
ok вторая страница ...                ok удаление вне PFTEST: 403 ...
```

**The gallery is shown as the panel draws it.** `pf6.mjs` gained `unpack` and
`drawFrame`, the exact inverse of `pack`: same 180-degree rotation, same nibble
order, same six colours. Every card in the grid is fetched as a frame and drawn
through that path, so a `.pf6` written by `tools/convert.py`, one uploaded from
the page, and a vendor BMP all appear as they are on the wall. The gate pins the
round trip: `pack(unpack(frame), {dither: false})` is byte-identical to `frame`,
which no wrong rotation or nibble order can survive.

Uploads go to the slideshow's own directory (`gallery.dir`, default
`PFTEST/UPLOADS`, created on first use), so a picture sent from a browser is also
a picture the frame will cycle through - and if the slideshow is running, the new
file jumps the queue instead of waiting a full lap. Uploads are only written
inside `PFTEST`: with `gallery.dir` pointed at the vendor's pictures the
slideshow still shows them, but an upload is refused.

**Settings** mirror the console commands and are validated by the same parser:
the form's fields are rewritten as `key=value` lines and re-parsed with
`Settings::parse`, then saved. A value the page accepts therefore cannot fail at
the next boot. Values with a live effect are applied immediately (volume, gain,
voice gate, `ai.idle`, slideshow); Wi-Fi changes say they apply on the next join.
An empty field means "leave it alone", which is what an untouched password box
has to mean.

The answer language is a short list rather than a text box: exactly the four
languages the bridge's recogniser is allowed to pick (`PF_STT_LANGS=ru,en,fi,fr`
in `bridge/src/whisper.rs`), shown in their own scripts — Русский, English, Suomi,
Français. Offering a language the bridge cannot hear would be a promise the frame
cannot keep. The frame still answers in the language of the question; this is the
language it listens and speaks in when the text does not say. A tag that is *not*
on the list — a hand-edited `CONFIG.TXT`, or a language added to the bridge later
— comes back as `de-DE (не из списка)` instead of blanking the box, because an
empty field means "keep" and would hide the setting.

**Protection is prepared, not enabled.** `web.password=<8..64 printable bytes>`
turns on HTTP Basic for every route, with any user name and that password; the
browser supplies its own login box. It is off by default because the frame is a
home appliance on a home LAN. Basic over plain HTTP is a lock on the front door,
not a private channel - the page says so where the password is set.

The console owns the way out of a forgotten password, because an empty form field
means "keep" and a page that cannot be unlocked cannot clear its own lock:

```bash
web password <8..64 chars>   # set it; saved at once, the listener rebinds
web password off             # remove it; the page is open again
```

Both go into the USB log as typed, so the passphrase is only as private as the
cable - which is the same physical access the card already gives. A set password
is echoed back as a character count, never as the value. Whichever path set it,
the running listener drops and rebinds with the new password immediately, so the
console and the page never disagree about what the lock is.

The form's password box also takes `-` to remove the lock, the same convention
`ai key` uses.

## Host image converter

```bash
python tools/convert.py input.png ~/tmp/frame/frame.pf6
```

Requires Pillow. The converter crops/fits to 800x480, applies Floyd-Steinberg
error diffusion against the panel's exact six RGB colors, and writes the native
4bpp/180-degree-rotated layout used by `display raw`. An exact six-color source converts
without color drift; the source-to-native mapping and 192000-byte size are checked
offline. No card writes are performed by the firmware or the converter.

## Images: the wire format is `.pf6`, and the browser does the conversion

The frame only ever accepts two things: a 24-bit BMP, or a raw `.pf6`
(192 000 bytes, 4bpp indices, 180-degree native order). It does **not** decode
JPEG or PNG, and that is a decision rather than a gap:

- a phone photo is 12 MP; RGB for one is ~36 MB, and the S3 has no JPEG
  peripheral (that is P4-only), so an on-device JPEG decoder would mean a hard
  megapixel cap and a real chance of failing mid-upload;
- the six-colour mapping wants error diffusion, which needs the whole picture in
  floating point - another 4.6 MB that belongs on a real machine;
- a `.pf6` is already in the panel's own encoding, so the upload path has nothing
  to validate beyond its size, and a bad upload cannot half-draw the panel.

So `tools/pf6.mjs` does resize, Floyd-Steinberg and packing in the browser, where
a canvas and 8 GB of RAM already exist, and the page posts 192 KB instead of
megabytes:

```js
import { packImage } from "./pf6.mjs";

const canvas = document.createElement("canvas");
canvas.width = 800; canvas.height = 480;
const context = canvas.getContext("2d", { willReadFrequently: true });
const frame = packImage(imageElement, context, { mode: "cover" }); // 192000 bytes
await fetch("/upload", { method: "POST", headers: { "X-Photoframe": "1" }, body: frame });
```

`mode: "cover"` fills the panel and centre-crops (what `tools/convert.py` does);
`mode: "contain"` fits the whole picture and letterboxes in white.

The two converters must not drift, so the gate compares them: same 800×480 RGB
in, byte-identical `.pf6` out, in both dithered and nearest-colour modes, on a
gradient, seeded noise, exact palette blocks, a 64×64 upscale and any photos
passed in:

```bash
python3 tools/check-pf6-js.py ~/Downloads/*.jpg
#   ok   img20261031123204 dither 192000 bytes sha256:563c091aab7002fd
# OK 6 images, both modes, byte-identical
```

The resize itself is necessarily engine-specific (Pillow's LANCZOS against the
canvas's), so the comparison starts at the pixels; everything after that is
shared logic and must match exactly. This is the same trick that caught the
µ-law codec mismatch against ffmpeg.

## Flashing gate

The initial flash was explicitly approved and completed; see `research/device/flash.log`
and `research/device/console-test.log`. Future flashes still require approval.
There is deliberately no Cargo
runner or automatic flash script. The default single-app partition table differs
from the factory layout; installing this image requires reviewing offsets and
writing the bootloader/table/application, not just blindly updating a factory OTA
slot. Factory settings may become inaccessible and must be recovered from backup.
The SD card has not been backed up and is never formatted by this firmware.

Before flashing, verify the device identity and backup SHA-256 again, and confirm
ROM download recovery. Afterwards validate USB commands, KEY/BOOT readings, green
LED, PSRAM totals and reset behavior before adding more peripherals. Do not hold
PWR for a diagnostic button test: it can cut power independently of firmware.

## License

GPL-3.0-or-later; see `LICENSE`. The OpenDisplay headers vendored in
`opendisplay-proto/upstream/` come from the GPL-3.0 OpenDisplay firmware.
