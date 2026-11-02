# photoframe-bridge

The LAN service the photo frame talks to for speech. It exists so the firmware does
**not** need a cloud API key, TLS, a certificate bundle, base64 or a JSON parser: the
device uploads companded audio, gets text back, and asks for speech. Everything that
needs a real CPU happens here.

## Wire protocol

Three `POST` endpoints, all plain text or raw audio. This is the whole contract; the
firmware side is `../src/bridge.rs` and its tests pin the same bytes.

| Path   | Request body                  | Response                          |
|--------|-------------------------------|-----------------------------------|
| `/stt` | µ-law, 16 kHz, mono           | `text/plain` UTF-8 transcript      |
| `/llm` | `text/plain` UTF-8 prompt     | `text/plain` UTF-8 reply           |
| `/tts` | `text/plain` UTF-8 text       | `audio/basic` µ-law, or `audio/wav` |
| `/health` | — (GET)                    | `ok`                              |

Headers the device sends, all optional:

- `X-Lang: ru-RU` — recognition and synthesis language.
- `X-Voice: <name>` — hint for the synthesiser.
- `X-Model: <name>` — hint for the model.
- `Authorization: Bearer <token>` — only if the bridge was started with `--token`.

Limits enforced here as well as on the device: 60 s of utterance, 8 KiB of text.
Failures return a 4xx/5xx with a one-line explanation, because the person reading it
is whoever is holding the frame's USB cable.

### Who is being answered

`/stt` refuses a transcript in three cases, all of them as `500 ERR stt: ...` so the
frame says nothing and goes back to listening:

| check | what it stops |
|---|---|
| fewer than 2 letters | Whisper's answer to a hum: `" ."`, a single `"У"` |
| a whole-transcript filler | `"Mm-hmm"`, `"Thank you."`, `"No."` — a noisy capture that then became an English "I'm here to help." |
| no wake stem (`--wake` / `PF_WAKE`) | `"Мама, дай мне сок."` — correct recognition of a conversation the frame was not part of |

The wake stems are compared as lowercase substrings, so `рамк` matches «рамка»,
«рамку» and «рамке», and `frame` matches the English name. Leaving `PF_WAKE` unset
keeps the old behaviour: every transcript is addressed. It cannot live in the
firmware, where it would need a flash per word — and it cannot be done by loudness at
all, because the room and a person speaking to the frame measure the same.

Two consequences of using a name, both learned from a real turn:

- **The name is removed from the question.** «Рамка, из чего делают торт?» reaches
the model as «из чего делают торт?». Left in, it asks about the word «Рамка» as
often as about the cake, and the panel shows the frame's own name in blue at the top
of every answer. Whole words are dropped, not stems, so «фоторамка» does not leave a
«фото» behind.
- **A bare «Рамка» opens a window instead of a turn.** People pause after a name, and
the detector closes its utterance after 700 ms of quiet: «Рамка, — из чего…» arrived
as two captures, the first was the name alone, and the model answered it as though
somebody had asked what a frame is. A transcript that is *only* the name now returns
`ERR stt: wake only` — nothing is spoken — and for `WAKE_WINDOW` (10 s) afterwards a
question needs no name. That is also how a follow-up works: «а почему?» right after an
answer still reaches the model.

µ-law at 16 kHz mono is 16 kB/s: a five-second question is 80 KB, and the device
needs no base64 to send it.

### Keeping the audio the device sent

`PF_BRIDGE_DUMP=<dir>` writes every `/stt` body to `<dir>/stt-<unix-ms>.ulaw` and logs
the path. Without it a failing microphone and a failing recogniser look identical: the
response only ever says `ERR stt: whisper heard nothing`.

Decode it with tools already at hand:

```sh
f=$(ls -t ~/tmp/frame/stt-dumps/*.ulaw | head -1)
ffmpeg -f mulaw -ar 16000 -ac 1 -i "$f" "${f%.ulaw}.wav"
./bridge/build.sh transcribe small ru "${f%.ulaw}.wav"
```

A 250 ms energy plot of the dump separates the cases that matter: speech shows modulated
energy across the window, a button tap shows a single low-frequency burst (a KEY press
measured ~125 Hz), and beep leakage would show a flat 880 Hz tone at the start. This is
what proved that three "whisper heard nothing" runs contained no speech at all.

## Running it

```bash
./build.sh run                              # stubs only, no models, no dependencies
./build.sh run -- --token s3cret            # require a bearer token
./build.sh run -- --stt whisper:small \
                  --llm 'cmd:llama-cli -m model.gguf' \
                  --tts 'cmd:piper --model ru_RU-irina-medium.onnx --output-raw'
```

Then point the frame at it, on the card:

```
ai.url=http://192.168.50.10:8080
ai.key=s3cret          # optional, must match --token
```

`./build.sh` exists because this directory lives inside a firmware project whose
`../.cargo/config.toml` pins the xtensa target and `-Zbuild-std`. Cargo reads config
by walking up from the *working* directory, so the script builds from a scratch
directory instead of duplicating configuration.

## Backends

`--stt`, `--llm`, `--tts` each accept `stub` (default) or `cmd:<program> [args...]`.

The stubs are not throwaway placeholders — they are how the device's request shape is
tested before any model is chosen:

- **stub recognition** answers with the duration and peak it received, so
  "0.00 s, peak 0" (digital silence: the wire or the microphone is dead) is
  distinguishable from "2.5 s, peak 9000" (audio arrived; only the model is missing).
- **stub synthesis** answers with a 440 Hz tone rather than silence, so a broken
  playback path cannot look like a broken synthesiser.

`cmd:` backends take the text or the prompt on stdin and write their answer on stdout;
`cmd:` recognition receives raw 16-bit mono 16 kHz PCM (already expanded from µ-law)
on stdin. Arguments are split on whitespace and executed directly, never through a
shell.

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

Hints reach the backends as environment variables: the recogniser gets `LANGS`
(the frame's `ai.lang`), the synthesiser gets `VOICES` (its `ai.voice`). A backend
with only one voice ignores them, which is why the espeak script below works
unchanged.

## Speech that sounds like a person

`scripts/tts-piper.sh` runs piper, a neural VITS synthesiser, and resamples its
22 050 Hz output to the 16 kHz the assistant path runs at. It is the recommended
voice; `scripts/tts-espeak.sh` stays as the dependency-free fallback.

Why it exists: the frame was reported as "the speech quality is bad, looks like the
rate is wrong", and every part of the path except the voice turned out to be
correct — the same DAC reproduced a natural room recording, the µ-law reply from the
bridge peaked at 82% of full scale with zero clipped samples, and 6.98 s of audio was
played in 7 125 ms. The remaining suspect was espeak-ng itself: a rule-based formant
synthesiser whose Russian sounds mechanical and whose pacing is uneven. Piper fixed
it, and nothing in the firmware changed — the `cmd:` slot took the new voice.

Setup (one voice, ~60 MB, nothing fetched at speak time):

```bash
uv tool install piper-tts
mkdir -p ~/.local/share/piper-voices && cd ~/.local/share/piper-voices
base=https://huggingface.co/rhasspy/piper-voices/resolve/main/ru/ru_RU/irina/medium
curl -LO "$base/ru_RU-irina-medium.onnx" -O "$base/ru_RU-irina-medium.onnx.json"
```

Measured here for an 80-character Russian answer: 1.8–2.1 s of synthesis for 7 s of
audio (espeak: 0.45 s, but robotic), peak 86% of full scale, nothing clipped.

Voice selection: `$VOICES` from the frame is matched against `<voices-dir>/<name>.onnx`,
so a second model (`ru_RU-dmitri-medium`, male, same repository) is selectable by
name. A name that matches nothing — the stored default is Google's
`ru-RU-Standard-B`, a leftover from the sketch this feature came from — falls back to
irina rather than failing, because an old setting should not silence the frame. The
script names the model it chose on stderr, so a `POST /tts -> 200` in the access log
is never the only evidence of who actually spoke:

```
tts-piper: voice=ru-RU-Standard-B model=~/.local/share/piper-voices/ru_RU-irina-medium.onnx
  tts cmd: voice=ru-RU-Standard-B pcm=222542 bytes -> ulaw=111271
```

Volume stops at 0.85 on purpose: at 1.0 piper peaks at full scale and clips a couple
of samples, and the frame plays at 0 dB into a small speaker, where that is
distortion rather than loudness.

## Answers that are actually answers

`scripts/llm-local.py` puts a real language model behind `/llm` instead of the stub
sentence. It posts to a local `llama-server` and prints the reply, so the whole
pipeline — recognition, model, voice — is offline. No provider key, no account, no
request leaving the LAN.

```bash
# One server, weights resident. A short answer then costs ~1.3 s; reloading the
# model per sentence with `cmd:llama-cli` would cost ~15 s.
~/local/19cce691/llama-server \
    -m /home/abokovoy/tmp/small-llms/gguf/Phi-4-mini-instruct-Q4_K_M.gguf \
    --host 127.0.0.1 --port 8096 -c 4096 -t 24 --jinja

./bridge/build.sh serve -- --stt whisper:small \
    --llm cmd:bridge/scripts/llm-local.py \
    --tts cmd:bridge/scripts/tts-piper.sh
```

Measured on this host (24 cores, no GPU), Russian prompt "Почему небо голубое?",
answer quality read by ear and eye:

| model | size | answer | verdict |
|---|---|---|---|
| Phi-4-mini-instruct Q4_K_M | 2.3 GB | 1.3 s, *«Это связано с длиной волны солнечного света.»* | chosen |
| granite-4.2-3b Q4_K_M | 2.1 GB | 9.6 s | 80 tokens of English reasoning, no answer |
| LFM2.5-8B-A1B Q4_K_M | 4.8 GB | 5.7 s | same: `reasoning_content` only |
| T-lite-instruct 8B Q4_K_M | 4.6 GB | ~20 s | Russian-tuned but 2.8 t/s, unusable aloud |
| Qwen3.8-27B Q4_K_M | 15.9 GB | — | no GPU, far slower than speech |

The two models that "thought" first are exactly wrong for this device: the frame wants
one sentence, and a reasoning trace spends the whole token budget on text that is
never spoken. The system prompt therefore asks for one or two sentences with no
lists, markdown or formulas — the reply is synthesised and painted on a Spectre-6
panel, both of which punish chat-window habits.

`llm-local.py` also strips markdown markers and wrapping quotes, and when a model
answers only into `reasoning_content` it says so instead of reporting an empty reply —
that message is what identified the two rejected models.

Through the frame, with the stub replaced:

```
ai ask Почему небо голубое?
  OK ai.ask reply_chars=44 speech_bytes=54614 seconds=3.41
     llm_ms=3454 tts_ms=1555 conv_ms=514 play_ms=3547 total_ms=9074
  reply="Это связано с длиной волны солнечного света."
```

The frame's `ai.model` is logged as `model_hint=` because it cannot be honoured: the
model is whatever the server loaded. The script names the real one on stderr, so both
appear in the log:

```
  llm cmd: model_hint=gemini-2.5-flash reply=44 chars
llm-local: model=Phi-4-mini-instruct-Q4_K_M.gguf tokens=11 speed=7.6t/s
```

## `--features whisper`: rwhisper in-process

`rwhisper` is candle-based Whisper in pure Rust — no whisper.cpp build, no ffmpeg, no
Python, no server to keep running. The model is multilingual and fixed to Russian at
load time, because a per-request language would mean reloading the weights.

```bash
./build.sh whisper                                  # release build, models linked in
./build.sh serve -- --stt whisper:small             # tiny | base | small | medium
./build.sh transcribe small ru /path/to/speech.wav  # same models, no device involved
```

**It is only usable in a release build.** Ten seconds of speech took 8.5 s in release
and 120 s in debug on this machine, which is exactly what rwhisper's own "models will
run extremely slowly in debug mode" warning is about.

### Model size is not a preference

| model | 10 s of Russian speech | notes |
|-------|------------------------|-------|
| `tiny` | empty or a two-word fragment | too weak for Russian; unusable |
| `base` | one garbled word | still unusable |
| `small` | correct Russian, one word misheard | the documented minimum, ~470 MB |

Measured on one core at 16 kHz: `small` transcribes 10 s of speech in ~8.5 s, so budget
roughly real time on a modern laptop and several times that on a small NAS. The first
run downloads the weights into the kalosm cache (`~/.cache/kalosm` by default), which is
why the bridge prints a loading line before it starts listening.

### The crate is vendored, and why

`vendor/rwhisper` is rwhisper 0.4.1 with one token added to the decode prompt; see
`vendor/rwhisper.patch`. Upstream never emits the `<|transcribe|>` token, so the model
is conditioned only by the language token and behaves as if asked to transcribe English.
Observable result before the patch: English audio transcribes fine, and Russian comes
back as **English prose** on `small` and as fragments on `base` — a failure that looks
like a bad model, not a bad prompt. One line changed, Russian works.

If a later release fixes it, drop the vendored copy, restore
`rwhisper = "0.4"` in `Cargo.toml`, and confirm with
`./build.sh transcribe small ru <file.wav>`.

### Dependencies

`rwhisper` depends on `rodio`, which pulls `alsa-sys`, so the host needs ALSA
development headers — on Fedora `sudo dnf install alsa-lib-devel`, otherwise the build
stops at `Package alsa was not found in the pkg-config search path`. `rodio` is also a
direct optional dependency here, at the same version, because
`Whisper::transcribe` takes a `rodio::Source` rather than an iterator, so samples are
wrapped in a `SamplesBuffer`. No decoders are requested: the bridge only ever hands over
raw PCM.

The `cmd:` route above reaches the same models with no Rust dependencies at all, and
stays the answer for a host where the weights do not fit or the ALSA headers are
unavailable.

## Why here and not in the firmware

Speech-to-text and text-to-speech both have monthly free allowances in the cloud, but
both still require a billing account to exist, and the device would carry a secret,
a TLS stack and a certificate bundle for the privilege. Running the models on a
machine that is already on the network costs nothing, keeps the transcript on the LAN,
and shrinks the firmware: no base64, no JSON, no keys on the card.

The tradeoff is honest: the frame needs that machine to be up. Recognition and
synthesis stop working when it is asleep, while everything else on the frame —
display, SD, audio, Wi-Fi, settings — is unaffected.
