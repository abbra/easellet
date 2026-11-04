# easellet-bridge

Easellet Bridge is a LAN service for the photo frame. It handles speech
recognition, language-model requests and speech synthesis on a host computer,
so the frame does not need to run those models.

The bridge also provides a web interface, scheduled plugin content, OpenDisplay
support and firmware updates. For the frame itself, see the
[project README](../README.md); for writing plugins, see the
[plugin guide](plugins/README.md).

## Find your way around

- [Run the bridge and connect a frame](#running-it)
- [Configure speech recognition, a model and a voice](#backends)
- [Use the OpenDisplay server](#opendisplay-server) or
  [manage firmware releases](#firmware-releases)
- [Understand the frame-to-bridge protocol](#wire-protocol)
- [Troubleshoot speech captures](#keeping-the-audio-the-device-sent)

The frame and bridge communicate over the LAN using plain HTTP. A bearer token
can restrict access, but it does not encrypt the traffic.

## Wire protocol

The speech interface has three `POST` endpoints and a `GET` health check.
Requests and responses use plain text or raw audio. The firmware implementation
is in `../src/bridge.rs`, with tests for the same protocol.

| Path | Request body | Response |
|------|--------------|----------|
| `/stt` | µ-law, 16 kHz, mono | `text/plain` UTF-8 transcript |
| `/llm` | `text/plain` UTF-8 prompt | `text/plain` UTF-8 reply |
| `/tts` | `text/plain` UTF-8 text | `audio/basic` µ-law, or `audio/wav` |
| `/health` | — (GET) | `ok` |

The device sends these headers. All are optional:

- `X-Lang: ru-RU` sets the recognition and synthesis language.
- `X-Voice: <name>` gives a hint to the synthesizer.
- `X-Model: <name>` gives a hint to the model.
- `Authorization: Bearer <token>` is used only if the bridge was started with
  `--token`.

The bridge enforces the same limits as the device: 60 s of utterance and 8 KiB of
text. A failure returns a 4xx/5xx code with a one-line explanation. The person who
reads it is the person who holds the frame's USB cable.

### Who is being answered

`/stt` refuses a transcript in three cases. All three return `500 ERR stt: ...`, so
the frame says nothing and goes back to listening:

| Check | What it stops |
|---|---|
| Fewer than 2 letters | Whisper's answer to a hum: `" ."`, a single `"У"`. |
| A whole-transcript filler | `"Mm-hmm"`, `"Thank you."`, `"No."`. A noisy capture once became an English "I'm here to help." |
| No wake stem (`--wake` / `EASEL_WAKE`) | `"Мама, дай мне сок."` is correct recognition of a conversation that the frame was not part of. |

The bridge compares wake stems as lowercase substrings. `рамк` therefore matches
«рамка», «рамку» and «рамке», and `frame` matches the English name. If `EASEL_WAKE` is
not set, the old behavior remains: the bridge treats every transcript as addressed.
The stem cannot live in the firmware, where it would need a flash per word. Loudness
cannot do this job either, because the room and a person who speaks to the frame
measure the same.

A refused capture leaves no trace. The bridge does not add it to the history that
the web UI shows, and it does not write the transcript to the log. Speech that the
frame merely overheard is therefore not retained. The log line says only why the
bridge dropped the capture. The one place where a voice stream is ever written to
disk is the explicit `EASEL_BRIDGE_DUMP` below.

Using a name has two consequences, both learned from a real turn:

- **The bridge removes the name from the question.** «Рамка, из чего делают торт?»
  reaches the model as «из чего делают торт?». If the name stays in, the model asks
  about the word «Рамка» as often as about the cake. The panel also shows the frame's
  own name in blue at the top of every answer. The bridge drops whole words, not
  stems, so «фоторамка» does not leave a «фото» behind.
- **A bare «Рамка» opens a window instead of a turn.** People pause after a name,
  and the detector closes its utterance after 700 ms of quiet. «Рамка, — из чего…»
  therefore becomes two captures. The first capture is the name alone, and the
  model would answer it as though somebody had asked what a frame is. A transcript
  that is *only* the name returns `ERR stt: wake only`, and nothing is spoken. For
  `WAKE_WINDOW` (10 s) afterwards, a question needs no name. This is also how a
  follow-up works: «а почему?» right after an answer still reaches the model.

µ-law at 16 kHz mono is 16 kB/s. A five-second question is 80 KB, and the device
needs no base64 to send it.

### Keeping the audio the device sent

`EASEL_BRIDGE_DUMP=<dir>` writes every `/stt` body to `<dir>/stt-<unix-ms>.ulaw` and
logs the path. It is off unless the operator asks for it, because it retains speech
that the frame was not part of. `serve-local.sh` does not enable it. Without the
dump, a failing microphone and a failing recognizer look identical: the response
only ever says `ERR stt: whisper heard nothing`.

Decode the dump with tools that you already have:

```sh
f=$(ls -t ~/tmp/frame/stt-dumps/*.ulaw | head -1)
ffmpeg -f mulaw -ar 16000 -ac 1 -i "$f" "${f%.ulaw}.wav"
./bridge/build.sh transcribe small ru "${f%.ulaw}.wav"
```

A 250 ms energy plot of the dump separates the cases that matter. Speech shows
modulated energy across the window. A button tap shows a single low-frequency burst
(a KEY press measures about 125 Hz). Beep leakage shows a flat 880 Hz tone at the
start. Digital silence shows no energy.

## Running it

Run these commands from the `bridge/` directory:

```bash
./build.sh run                              # stubs only, no models, no dependencies
./build.sh run -- --token s3cret            # require a bearer token
./build.sh run -- --stt whisper:small \
                  --llm 'cmd:llama-cli -m model.gguf' \
                  --tts 'cmd:piper --model ru_RU-irina-medium.onnx --output-raw'
```

Then put the bridge address and, if configured, its token in the frame's
`EASELLET/CONFIG.TXT` on the SD card:

```
ai.url=http://192.168.50.10:8080
ai.key=s3cret          # optional, must match --token
```

`./build.sh` exists because this directory lives inside a firmware project whose
`../.cargo/config.toml` pins the xtensa target and `-Zbuild-std`. Cargo reads its
config by walking up from the *working* directory. The script therefore builds from
a scratch directory instead of duplicating the configuration.

## OpenDisplay server

The bridge can act as the OpenDisplay device for one frame. Clients such as Home
Assistant then connect to the bridge, not to the frame. The bridge keeps the
protocol state and sends the image to the frame through its normal OpenDisplay
client.

Use the **OpenDisplay server** section on the Frames tab. Select one frame, enable
the server and set the discovery name. The bridge saves these keys in
`bridge.toml`:

| Key | Meaning |
|---|---|
| `opendisplay_enabled` | Run the listener. The default is `false`. |
| `opendisplay_frame` | Frame id to serve. |
| `opendisplay_name` | mDNS instance name. The default is `Easellet`. |
| `opendisplay_addr` | Listen address. The default is `0.0.0.0:2446`. |

The bridge advertises `_opendisplay._tcp` and answers `ConfigRead`, `ReadMsd`,
`FirmwareVersion`, `DirectWrite` and `Reboot`. It forwards `DeepSleep` and
`Reboot` to the frame. A configuration write changes the bridge's virtual display
only. The frame keeps its own configuration.

The bridge refuses an image when the selected frame is asleep, offline or already
refreshing. It does not pretend that a refresh completed. The image is accepted,
then the bridge waits for the frame's real `0x73` or `0x74` result and returns that
result to the client.

The bridge filters its own mDNS answer from frame discovery. Without that filter,
the bridge would list itself as a frame and create a loop.

Both endpoints advertise by default. To make the bridge the only endpoint, set
`opendisplay.mdns=0` on the frame and reboot it. The frame then keeps its TCP
listener but stops its mDNS advertisement.

## Firmware releases

The bridge can read the repository's releases, so the frame card knows which
versions exist and what each image hashes to. It never writes to the repository.

| Key | Default | Meaning |
|---|---|---|
| `firmware_repo` | empty | `owner/name`. Empty turns the feature off. |
| `firmware_channel` | `stable` | `stable` or `dev`. The dev channel is the moving pre-release built from `main`. |
| `firmware_token` | empty | For a private repository. The API says whether it is set, never its value. |
| `firmware_check_h` | `6` | Hours between automatic refreshes. `0` means only when a person asks. |
| `firmware_index_url` | empty | Any URL serving the same `firmware-index.json`, for a home without GitHub. |

The list is one asset, `firmware-index.json`, attached to every release
(`tools/firmware-index.py` writes it in CI). So the bridge reads
`releases/latest/download/firmware-index.json`, or
`releases/download/firmware-dev/firmware-index.json` for the dev channel, and not
the GitHub API: no rate limit, no token, one request.

The index is cached in `$XDG_CACHE_HOME/easellet-bridge/firmware/index.json` with
the time it was fetched. A bridge that cannot reach GitHub still shows the list it
had and how old it is.

| Route | Meaning |
|---|---|
| `GET /api/firmware` | The settings, the cached list, when it was fetched, and the last error. Refreshes in the background when the list is due. |
| `POST /api/firmware/refresh` | Fetch now. `200` with the list, `502` with the reason. |
| `GET /api/frames/<id>/ota` | The frame's own `GET /ota`, proxied and cached for 30 s: the running version, the other slot and the slot size. |
| `POST /api/frames/<id>/firmware` | `{"version": "...", "confirm": true}`. Answers `202` and writes on its own thread. |
| `GET /api/frames/<id>/firmware` | `{"version", "phase", "message", "started_ms"}`: `downloading`, `writing`, `restarting`, `done`, `failed`, or `idle`. |

An install is four steps: download to the cache and check the hash, `POST /ota` to
the frame with a 240 s timeout (the frame allows its own socket 180 s), wait for
the frame to come back with the new version, and say what happened. A reset in the
middle of the POST is the normal ending, not a failure: the frame restarts as soon
as it has written the slot, so the bridge asks it rather than guessing.

The version it waits for is the one in the frame's image description, which is the
same string `firmware-index.json` names a release by. `Cargo.toml`'s number is only
a fallback, for an image that carries no description.

The bridge refuses an install when the version is already running, when an install
on that frame is already in flight, when the frame is asleep, when the image is
larger than the frame's slot, and when the release was built for a different
partition table. It does not refuse an older version: installing one is the
recovery path for a bad release, and the bootloader's rollback covers the other.

## Backends

`--stt`, `--llm` and `--tts` each accept `stub` (the default) or
`cmd:<program> [args...]`.

The stubs are not throwaway placeholders. They are how the device's request shape is
tested before any model is chosen:

- **Stub recognition** answers with the duration and peak that it received. You can
  therefore tell "0.00 s, peak 0" (digital silence: the wire or the microphone is
  dead) from "2.5 s, peak 9000" (audio arrived; only the model is missing).
- **Stub synthesis** answers with a 440 Hz tone instead of silence, so a broken
  playback path cannot look like a broken synthesizer.

`cmd:` backends take the text or the prompt on stdin and write their answer on
stdout. `cmd:` recognition receives raw 16-bit mono 16 kHz PCM on stdin, already
expanded from µ-law. The bridge splits arguments on whitespace and executes the
program directly, never through a shell.

Examples:

```bash
# Whisper through the original C++ binary, no Rust bindings needed.
--stt 'cmd:whisper-cli -m models/ggml-small.bin -l ru -nt -f /dev/stdin'

# Piper for synthesis, through the ready-made script (see "Speech that sounds like
# a person" below): raw 16 kHz PCM on stdout, which the bridge compands to µ-law.
--tts 'cmd:./scripts/tts-piper.sh'

# Any prompt-in/text-out model, including a local llama.cpp server wrapper.
--llm 'cmd:./ask-local.sh'
```

Hints reach the backends as environment variables. The recognizer gets `LANGS` (the
frame's `ai.lang`). The synthesizer gets `VOICES` (its `ai.voice`). A backend with
only one voice ignores them, which is why the espeak script below works unchanged.

## Speech that sounds like a person

`scripts/tts-piper.sh` runs piper, a neural VITS synthesizer, and resamples its
22 050 Hz output to the 16 kHz that the assistant path uses. It is the recommended
voice. `scripts/tts-espeak.sh` stays as the dependency-free fallback.

espeak-ng is a rule-based formant synthesizer. Its Russian sounds mechanical, and
its pacing is uneven. The rest of the path does not limit the quality: the same DAC
reproduces a natural room recording, the µ-law reply from the bridge peaks at 82%
of full scale with zero clipped samples, and the frame plays 6.98 s of audio in
7 125 ms. Piper is a neural VITS synthesizer, and its Russian is natural. The
`cmd:` slot accepts either voice without a firmware change.

Setup (one voice, about 60 MB, nothing fetched at speak time):

```bash
uv tool install piper-tts
mkdir -p ~/.local/share/piper-voices && cd ~/.local/share/piper-voices
base=https://huggingface.co/rhasspy/piper-voices/resolve/main/ru/ru_RU/irina/medium
curl -LO "$base/ru_RU-irina-medium.onnx" -O "$base/ru_RU-irina-medium.onnx.json"
```

Measured here for an 80-character Russian answer: 1.8-2.1 s of synthesis for 7 s of
audio (espeak: 0.45 s, but robotic), peak 86% of full scale, nothing clipped.

Voice selection: the bridge matches `$VOICES` from the frame against
`<voices-dir>/<name>.onnx`. A second model (`ru_RU-dmitri-medium`, male, same
repository) is therefore selectable by name. A name that matches nothing falls back
to irina rather than failing, because an old setting should not silence the frame.
The stored default is Google's `ru-RU-Standard-B`, which matches no local model.
The script falls back to irina for it rather than failing. The script names the
model that it chose on stderr, so a `POST /tts -> 200` in the access log is never
the only evidence of who actually spoke:

```
tts-piper: voice=ru-RU-Standard-B model=~/.local/share/piper-voices/ru_RU-irina-medium.onnx
  tts cmd: voice=ru-RU-Standard-B pcm=222542 bytes -> ulaw=111271
```

Volume stops at 0.85 on purpose. At 1.0 piper peaks at full scale and clips a
couple of samples. The frame plays at 0 dB into a small speaker, where that is
distortion rather than loudness.

## Answers that are actually answers

`scripts/llm-local.py` puts a real language model behind `/llm` instead of the stub
sentence. It posts to a local `llama-server` and prints the reply, so the whole
pipeline (recognition, model, voice) is offline. There is no provider key, no
account, and no request leaves the LAN.

```bash
# One server, weights resident. A short answer takes 1.4–2.1 s on this host.
~/local/19cce691/llama-server \
    -m ~/models/crane/qwen3-4b/Qwen3-4B-Instruct-2507-Q4_K_M.gguf \
    --host 127.0.0.1 --port 8096 -c 4096 -t 24 --jinja

./bridge/build.sh serve -- --stt whisper:small \
    --llm cmd:bridge/scripts/llm-local.py \
    --tts cmd:bridge/scripts/tts-piper.sh
```

Earlier comparison on this host (24 cores, no GPU), Russian prompt "Почему небо голубое?",
answer quality judged by listening and reading. The current model is
Qwen3-4B-Instruct-2507 Q4_K_M, measured at 1.4–2.1 s for a one-sentence answer.

| Model | Size | Answer | Verdict |
|---|---|---|---|
| Phi-4-mini-instruct Q4_K_M | 2.3 GB | 1.3 s, *«Это связано с длиной волны солнечного света.»* | previously chosen |
| granite-4.2-3b Q4_K_M | 2.1 GB | 9.6 s | 80 tokens of English reasoning, no answer |
| LFM2.5-8B-A1B Q4_K_M | 4.8 GB | 5.7 s | same: `reasoning_content` only |
| T-lite-instruct 8B Q4_K_M | 4.6 GB | ~20 s | Russian-tuned but 2.8 t/s, unusable aloud |
| Qwen3.8-27B Q4_K_M | 15.9 GB | — | no GPU, far slower than speech |

Models that produce a reasoning trace first are wrong for this device. The frame
wants one sentence, and a reasoning trace spends the whole token budget on text that
is never spoken. The system prompt therefore asks for one or two sentences with no
lists, markdown or formulas. The reply is synthesized and painted on a Spectre-6
panel. Both outputs need short, plain text.

`llm-local.py` also strips markdown markers and wrapping quotes. When a model
answers only into `reasoning_content`, it says so instead of reporting an empty
reply. That message names the failure, so an empty reply is not reported as a valid
answer.

Earlier frame run with Phi-4-mini, after replacing the stub:

```
ai ask Почему небо голубое?
  OK ai.ask reply_chars=44 speech_bytes=54614 seconds=3.41
     llm_ms=3454 tts_ms=1555 conv_ms=514 play_ms=3547 total_ms=9074
  reply="Это связано с длиной волны солнечного света."
```

The frame's `ai.model` is logged as `model_hint=` because the bridge cannot honor
it: the model is whatever the server loaded. The script names the real model on
stderr; this earlier Phi-4-mini run shows both in the log:

```
  llm cmd: model_hint=gemini-2.5-flash reply=44 chars
llm-local: model=Phi-4-mini-instruct-Q4_K_M.gguf tokens=11 speed=7.6t/s
```

## Exact arithmetic

The bridge answers one number, one operator and one number itself, before it calls
any model (`src/arithmetic.rs`). Models can return a wrong result in a confident
voice. Measured on this host for «458 умножить на 714», Phi-4-mini answers `326 412`
and the current model answers `327 732`; the correct value is `327 012`. A wrong
number spoken in the same confident voice is worse than no answer. The recognizer is
deliberately narrow: Russian operator words and the usual symbols, a decimal comma
or point, a signed operand, at most 15 digits, one operation. Anything wider (three
operands, parentheses, units, a different word) goes to the model unchanged. A
calculator that silently drops part of the question is worse than none. Division by
zero gets «На ноль делить нельзя.» instead of a model's guess.

Through the frame: `llm_ms=32` and `reply="458 умножить на 714 равно 327012."`.
The model takes 1.4 s for the same question. A control question still costs the
usual `llm_ms=892`.

## `--features whisper`: rwhisper in-process

`rwhisper` is candle-based Whisper in pure Rust: no whisper.cpp build, no ffmpeg, no
Python, no server to keep running. The model is multilingual and fixed to Russian at
load time, because a per-request language would mean reloading the weights.

```bash
./build.sh whisper                                  # release build, models linked in
./build.sh serve -- --stt whisper:small             # tiny | base | small | medium
./build.sh transcribe small ru /path/to/speech.wav  # same models, no device involved
```

**It is only usable in a release build.** Ten seconds of speech take 8.5 s in
release and 120 s in debug on this machine. This is exactly what rwhisper's own
warning means: "models will run extremely slowly in debug mode".

### Model size is not a preference

| Model | 10 s of Russian speech | Notes |
|-------|------------------------|-------|
| `tiny` | empty or a two-word fragment | too weak for Russian; unusable |
| `base` | one garbled word | still unusable |
| `small` | correct Russian, one word misheard | the documented minimum, ~470 MB |

Measured on one core at 16 kHz: `small` transcribes 10 s of speech in about 8.5 s.
Budget roughly real time on a modern laptop and several times that on a small NAS.
The first run downloads the weights into the kalosm cache (`~/.cache/kalosm` by
default). That is why the bridge prints a loading line before it starts listening.

### The crate is vendored, and why

`vendor/rwhisper` is rwhisper 0.4.1 with one token added to the decode prompt. See
`vendor/rwhisper.patch`. Upstream never emits the `<|transcribe|>` token, so the
model is conditioned only by the language token and behaves as if asked to transcribe
English. With the unpatched upstream crate, English audio transcribes fine, and
Russian comes back as **English prose** on `small` and as fragments on `base`. That
failure looks like a bad model, not a bad prompt. The added token makes Russian
transcribe correctly.

If a later release fixes it, drop the vendored copy, restore `rwhisper = "0.4"` in
`Cargo.toml`, and confirm with `./build.sh transcribe small ru <file.wav>`.

### Dependencies

`rwhisper` depends on `rodio`, which pulls `alsa-sys`. The host therefore needs ALSA
development headers. On Fedora, run `sudo dnf install alsa-lib-devel`; otherwise the
build stops at `Package alsa was not found in the pkg-config search path`. `rodio`
is also a direct optional dependency here, at the same version, because
`Whisper::transcribe` takes a `rodio::Source` rather than an iterator. The bridge
therefore wraps samples in a `SamplesBuffer`. No decoders are requested: the bridge
only ever hands over raw PCM.

The `cmd:` route above reaches the same models with no Rust dependencies at all. It
stays the answer for a host where the weights do not fit or the ALSA headers are
unavailable.

## Why here and not in the firmware

Speech-to-text and text-to-speech both have monthly free allowances in the cloud.
Both still require a billing account, and the device would carry a secret, a TLS
stack and a certificate bundle for the privilege. Running the models on a machine
that is already on the network costs nothing. It keeps the transcript on the LAN and
shrinks the firmware: no base64, no JSON, no keys on the card.

The tradeoff is clear: the frame needs that machine to be up. Recognition and
synthesis stop working when it is asleep. Everything else on the frame (display, SD,
audio, Wi-Fi, settings) is unaffected.
