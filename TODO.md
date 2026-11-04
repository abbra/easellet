# TODO — easellet firmware

Working task list and historical bring-up notes. `PLAN.md` contains the original
stage roadmap and a current implementation summary. Completed entries record
the evidence available at the time; paths, limits and measurements in those
entries may describe older firmware. An implementation checkbox does not by
itself claim hardware acceptance.

Current settings and writable files use `/sdcard/EASELLET`, not the former
`EASELTEST` directory. See the README for current operating instructions.

**Approval gates** — ask before each of these, every time: flashing, SD writes
outside `/sdcard/EASELLET`, RTC writes, PMIC writes, SD formatting. Reading is free.

## Baseline: verified on hardware

Every item below has a console command and a recorded acceptance run.

- [x] Inventory, backup, identity — backup SHA-256 `5a4f8ffc…c245e`, Secure Boot and
      Flash Encryption off.
- [x] I²C diagnostics — SHTC3 `0x0887`, PCF85063, AXP2101 `chip_id=0x4a`.
- [x] Display — six-color bars, BMP, raw `.el6`, clear, sleep; 180° native rotation.
- [x] SD — 4-bit SDMMC at 20 MHz, read paths, `EASELTEST` writes, missing-card errors.
- [x] Audio — ES8311 playback (tone/WAV), ES7210 capture with peak normalisation,
      level metering, WAV record, live `audio volume` / `audio gain`.
- [x] Wi-Fi — scan, join, DHCP `192.168.50.204`, SNTP, RTC write.
- [x] Settings — loaded at boot and saved from live state; `wifi.auto=1` joins
      and stays online with no command from the host. The current path is
      `EASELLET/CONFIG.TXT`.
- [x] Voice-assistant foundation — `ai.*` settings, µ-law codec, the LAN bridge
      protocol (`bridge`), `ai status`, `ai selftest`, and the `ai url` / `ai key`
      console setters. Verified on the frame: `config save` persisted
      `ai.url=http://192.168.50.93:8095` plus a 19-byte token, and after a reset
      `ai selftest` reported `capture_1s_bytes=32000 ulaw=16000 stt_body=16000
      llm_body=31 tts_body=28`, `decode frames=16000 stereo_bytes=64000
      transcript=15chars reply=398chars` against the real URL and the real token.
      That self-test did not open a socket; later hardware runs below exercised
      the bridge over the network.

## P1 — Voice assistant (Russian), via a LAN bridge

Answering the question from the previous plan revision settled the architecture: the
pipeline is local. Recognition and synthesis run on a machine on the LAN
(`bridge/`), so the firmware carries **no cloud key, no TLS stack, no certificate
bundle, no base64 and no JSON parser**. The device uploads µ-law, receives text, and
asks for speech.

Source for the conversational shape: `research/vendor-port/assistente_ai_optimized.ino`
(MIT). The shape survives the port; the plumbing does not.

**Done and host-tested:**

- [x] `CONFIG.TXT` keys `ai.url`, `ai.key` (optional bearer token), `ai.lang`,
      `ai.voice`, `ai.model`, validated at load; `ai.url` has no default because a
      wrong default would talk to a stranger's machine. `config` masks the token.
- [x] µ-law companding (`pcm::ulaw_encode`/`ulaw_decode`), error bounded relative to
      the signal — halves the upload.
- [x] Wire protocol (`bridge::Base`, `stt_request`, `llm_request`, `tts_request`,
      `transcript_response`, `reply_response`, `speech_response`) with a 400-character
      reply budget applied inside the request builder, so no path can bypass it.
      Accepts µ-law or WAV back from the bridge; refuses any other type by name.
- [x] `ai status` and `ai selftest` — the latter builds all three requests from the
      real settings, round-trips µ-law in both directions, decodes a synthetic reply
      and reports frames and channel duplication. No socket, no cost.
- [x] `ai url <url>` and `ai key <token>` console setters, so the bridge address and
      token do not need the card pulled out of the device. Validation is shared with
      the file parser (`valid_ai_url`, `key_is_valid`), the token is never echoed, `-`
      clears either one, and `config save` persists them like volume and gain.
- [x] Host service (`bridge/`): µ-law codec, stub backends that report duration and
      peak, `cmd:` backends that pipe to any external program, token auth, limits,
      access log, and its own unit tests. Verified end to end with `curl` and a
      Python client: 401 without the token, transcript with duration and peak,
      model reply, `audio/basic` synthesis at 440 Hz.
- [x] `--features whisper` **works**, on real Russian speech: rwhisper 0.4.1 plus
      `rodio` 0.20 for the `SamplesBuffer` that `Whisper::transcribe` requires.
      Ten seconds of a Russian lecture posted as µ-law to `/stt` comes back as
      Russian text in ~9 s (release build). Verified against a fixed reference:
      `small` transcribes `jfk.wav` verbatim.
- [x] **The crate needed a one-token patch to get there.** Upstream 0.4.1 never puts
      `<|transcribe|>` in the decode prompt, so only the language token conditions the
      decoder and the model has to guess the task — and guessing wrong means translating
      or inventing: Russian came back as English prose on `small`, as a two-word fragment
      on `base`, as nothing on quantized `tiny`. It is vendored in `bridge/vendor/rwhisper` with a documented
      one-line diff (`vendor/rwhisper.patch`), because the field is not reachable from
      the public API. English audio was unaffected by the bug, which is why it can sit
      in a released crate unnoticed.
- [x] `small` is the documented minimum model for Russian; `tiny` and `base` do not
      carry it even with the patch. Release builds only: the same ten seconds took
      120 s in debug versus 8.5 s in release.

**Implementation and hardware milestones:**

- [x] **A. 16 kHz audio path.** Decided against making the rate runtime: esp-idf-hal
      0.47 `I2sDriver` has no `release()`, so rebuilding a driver at another rate
      means `unsafe` pin stealing, and the 24 kHz path is the one that has been
      proven on hardware. Instead `pcm::resample` (Lanczos, anti-aliased on
      downsample) converts 24k→16k on the way up and 16k→24k on the way back.
      Pure functions, four native tests: duration preserved, 440 Hz stays 440 Hz,
      10 kHz rejected as an alias, mono downmix and re-interleave.
- [x] **B. Plain HTTP client.** Done and verified over the wire from the frame:
      `src/http.rs` (`EspHttpConnection`, 2 KiB rx/tx buffers, 60 s timeout,
      per-caller reply cap, non-200 returned rather than raised), `ai stt <ms>` captures
      at 24 kHz, normalises, downmixes, resamples to 16 kHz, µ-law encodes one
      byte per sample, POSTs to `/stt`, and prints the transcript. Hardware run on a 3 s
      utterance: `status=200 pcm_bytes=288000 ulaw_bytes=48000 roundtrip_ms=3059
      transcript="Окей."`, bridge log `POST /stt -> 200 in=48000 out=9 2911 ms`.
      Then three 5 s runs with a real Russian sentence: «Казается в пляшет вокруг
      елки.» (partial capture), «Сколько зайцев пляшут вокруг ёлки?» and «Сколько
      зайцев пляшет вокруг елки?» — runs 2 and 3 verbatim apart from `ё`. Levels
      `peak=1711..3158`, `norm_db=+14.3..+19.6`, `rms_l=157..247` at 30 dB mic gain.
      A first 3 s run returned «Окей.» with `peak=220`, i.e. the frame heard almost
      nothing and whisper filled the gap with a plausible word — worth remembering
      as the reason to gate on measured level rather than on status 200.
- [x] **Where the extra ~3 s per utterance went.** Answered by instrumenting every
      stage and reading the device. `ai stt 5000` now reports
      `cue_ms=987 setup_ms=1050 read_ms=5003 capture_ms=6169 conv_ms=515
      roundtrip_ms=4236 trace_ms=12115`, and it all adds up. The two anomalies found
      on the way were both software-emulated `f64` on a chip with no double:
      `pcm::stats` accumulated in `f64` and cost **882 ms** per five-second capture
      (a five second capture spent 880 ms measuring itself); `pcm::resample`
      computed sample positions in `f64` — position, floor, fraction, phase, six
      soft-double calls per output sample. Both now work in integers: `stats` sums
      squares and cross terms in `i64` (exact where the float sum drifted),
      `resample` carries the phase as a remainder over the reduced ratio, so
      `24000/16000` becomes `num=3, den=2` and the position is the same number
      computed exactly. Result: capture bookkeeping 882 → 117 ms, and the resampler
      357 ms for five seconds, which is 7% of realtime and *not* pathological —
      the host needs 1.6 ms, and the ratio falls out of 240 MHz scalar versus
      3.5 GHz out-of-order. One flash cycle was spent disproving a 200x anomaly
      theory before the emitted assembly showed hardware `mul.s`/`add.s` and the
      timing scaled linearly with duration; the honest conclusion is that ~500 ms
      for 5 s of resampling is ordinary scalar cost and only worth revisiting
      (fixed-point weights, integer MAC) if a TTS answer ten seconds long makes
      the downlink conversion feel slow.
- [x] **Replies larger than the USB TX buffer were silently dropped.** `help` grew to
      1069 bytes and stopped answering entirely. The cause is in IDF, not in this
      code: `usb_serial_jtag_write_bytes` ends in `xRingbufferSend`, which is
      all-or-nothing — a write bigger than the free ring space writes nothing and
      reports 0, which the writer read as "host is gone". `send` now hands the driver
      256-byte slices, so a long reply drains instead of vanishing; the 100 ms timeout
      per slice still bounds an unplugged host. `help` is the only reply currently
      past the boundary, but `ai talk` (transcript + reply + timings) is close enough
      that it would have failed the same way.
- [x] **A 64 KiB reply cap truncated the first spoken answer.** `ai ask` answered
      `ERR http reply ... exceeds 65536 bytes` although the bridge had synthesised a
      perfectly good 112 798-byte reply in 278 ms: espeak needs ~14 s of audio for a
      130-character Russian answer, and the cap was 4 s. The ceiling now belongs to
      the caller — a transcript (2 KiB), an answer (4 KiB) or speech (320 000 bytes,
      20 s) — because one number cannot serve three endpoints that differ by two
      orders of magnitude. Hitting the limit no longer aborts the request either: the
      body is cut, `Reply.truncated` records it, text endpoints treat that as an error
      (half a sentence is not an answer) and `ai ask` plays the first 20 s and prints
      `truncated=true`. 320 000 µ-law bytes become 640 000 of PCM, 960 000 at 24 kHz,
      then 1.9 MB stereo, which is why the bound is a duration rather than "whatever
      fits". Boundary covered natively: `MAX_SPEECH_BYTES == ASSISTANT_RATE * 20` and
      a body of exactly that length decodes.
- [x] **"The speech quality is bad, looks like the rate is wrong."** Traced to the
      voice, not to the hardware, and only after clearing the four innocent stages
      with measurements: the same DAC reproduced a natural room recording correctly
      (`audio play EASELTEST/REC.WAV`, 144 000 frames); our µ-law decoder is
      byte-identical to ffmpeg's on the bridge's real reply (57 997 samples,
      `max_diff=0`); the 16k→24k resampler keeps pitch (440 Hz in, 440 Hz out); and
      the audio that arrived was clean (peak 82% FS, zero clipped samples) and was
      played at the right speed (6.98 s in 7 125 ms). That leaves espeak-ng, a
      rule-based formant synthesiser whose Russian is mechanical with uneven pacing —
      and an answer to open choice 1 that had been sitting there since the plan.
- [x] **Piper: a Russian voice that sounds like a person.** `bridge/scripts/tts-piper.sh`
      runs piper (VITS) at 22 050 Hz, resamples to 16 kHz, peaking at 86% FS with
      nothing clipped; `--volume 0.85` is the default because 1.0 hits full scale and
      clips a couple of samples into a 0 dB DAC and a small speaker. Synthesis costs
      1.8–2.1 s for a 7 s answer (espeak: 0.45 s, but robotic). No firmware change, no
      flash: the `cmd:` slot took the new voice. Voice selection is wired too — the
      `cmd:` synthesiser now receives the frame's `ai.voice` as `$VOICES` (the
      recogniser already received `LANGS`), the script matches it against
      `<voices-dir>/<name>.onnx`, and a name that matches nothing (the stored default
      is Google's `ru-RU-Standard-B`, a leftover from the sketch) falls back to irina
      and says so on stderr rather than failing, because an old setting should not
      silence the frame. `ru_RU-dmitri-medium` is now a one-command change.
- [x] **The stub answer is gone; `/llm` reaches a real model.** Through
      `bridge/scripts/llm-local.py` → `llama-server` → Phi-4-mini-instruct Q4_K_M,
      entirely on the LAN. Through the frame: `ai ask Почему небо голубое?` answered
      `reply="Это связано с длиной волны солнечного света."` in
      `llm_ms=3454 tts_ms=1555 play_ms=3547 total_ms=9074`; a two-sentence question
      answered 156 characters in `llm_ms=9752 tts_ms=3173`. No firmware change and no
      flash — the `cmd:` slot took it, which is what the abstraction was for.
      Two things the wiring had to admit. `ai.model` cannot be honoured, because the
      model is whatever the server loaded, so the bridge now logs `model_hint=` and
      the script names the real model on stderr. And a model that answers only into
      `reasoning_content` yields an empty reply, so the script says exactly that
      instead of "empty reply" — that message is what convicted granite and LFM2.5.
- [x] **The USB console node lost its ACL when the device re-enumerated.**
      `PermissionError: /dev/ttyACM0` with the node `root:dialout` and this user not
      in `dialout`: the logind ACL disappears on replug. `sudo setfacl -m u:$USER:rw
      /dev/ttyACM0` restores it; a udev rule would make it stick and is not written
      yet. Worth knowing before blaming the firmware for an unreachable console.
- [x] **Every utterance lost its first word, and the beep was the reason.**
      «Сколько будет 458 умножить на 714» arrived as «будет 458 умножить на 714», and
      the earlier «Казается в пляшет вокруг елки» was the same defect seen from a
      different angle: `utterance` played the cue, waited `CUE_SETTLE_MS` (300 ms), and
      only then called `capture`, which starts the ADC and throws away
      `SETTLE_FRAMES` (24 000 frames = a full second). That is ~1.38 s of dead air
      between the beep and the first captured sample, so a speaker who starts talking
      when the beep stops loses their first word every time — one syllable for a short
      word, one word for «Сколько».

      Fixed by reordering rather than by shrinking a constant, because the settle is
      needed: the ADC must be warm before speech starts, it just must not be warm
      *instead of* speech. `capture` is now `start_reader` + `read_frames` +
      `stop_reader`, and `utterance` runs `start_reader` *before* the cue. The cue is
      played by a new `cue_duplex`, which writes and reads in alternating chunks the
      way the loopback test does: `play_pcm` ends with `stop()`, so warming up before
      it would have been undone, and reading during the beep keeps the DMA ring from
      filling with it. After the beep `CUE_TAIL_FRAMES` (2 400 = 100 ms) is dropped,
      so the utterance begins just after the cue instead of 1.38 s after it.
      `record`, `level` and `loop` keep their previous behaviour; `cue_ms` now means
      the cue plus that 100 ms tail. Release image 1 267 936 → 1 269 088 bytes.
      Flashed and verified: `ai listen 8000` returned
      `transcript="Сколько будет 458 умножить на 714?"` — first word included, where
      the same phrase used to arrive as «будет 458 умножить на 714». The captured
      audio, dumped by the bridge, shows speech from 1.50 s to 2.50 s with nothing
      clipped at the front.
- [x] **`ai stt` waits for KEY.** It used to beep and record
      immediately, so an operator told to "press KEY during `ai stt`" pressed *after* the
      beep and the recording held 6 s of silence and one 125 Hz knock — the missing
      beep-on-press was the firmware correctly ignoring a button that command never read.
      The evidence that settled it was the envelope of the dumped utterance (one 0.5 s
      burst at ~125 Hz, no formants anywhere), not a guess about timing; the stage-A
      change was innocent and the operator was wrong. All three console turn commands
      (`ai stt`, `ai listen`, `ai talk`) now poll KEY (GPIO4, 30 s) before the cue, and a
      timeout says which one ran (`ERR ai.stt: no KEY press within 30 s`). Seen on
      hardware (flashed 2026-11-02): `ai stt 3000` pressed ~10 s in reported
      `waited_ms=9959` and `transcript="какая погода в Хельсинки?"`, and the bridge
      recorded the turn with `trigger=key` — where the old build recorded before the
      press.
- [x] **The debug image fits the partition, and `check.sh` says so.** The remedy is in
      place: the script prints `headroom=` and `fits=` for both profiles. Current builds:
      debug 1 221 408 bytes (headroom 314 592, `fits=yes`), release 1 128 400. The
      1 539 072-byte number this item was written around is stale.
- [x] **300 ms warm-up so the beep answers the press.** `utterance` settles on
      `UTTERANCE_SETTLE_FRAMES = 7_200` (300 ms); `record`, `level` and `loop` keep the
      full `SETTLE_FRAMES`, where the extra second costs nothing. Flashed 2026-11-02 and
      seen: `ai stt 3000` reported `setup_ms=350` before a 739 ms cue, where the old
      build spent a second warming up before the beep.

- [x] **Arithmetic is computed, not guessed.** Asked out loud
      «Сколько будет 458 умножить на 714?», the whole loop ran correctly — whisper heard
      `"будет 458 умножить на 714."`, the panel drew it, piper spoke the answer — and
      the answer was `324,012` instead of `327,012`. At `temperature=0` Phi-4-mini is
      *consistently* wrong (`326,412`, five runs of five), which is worse than noisily
      wrong: it reads like a fact. At 0.4 it was wrong five different ways, one run
      answering a subtraction nobody asked for. The cached `Qwen3.8-27B` gets it right
      (`327012`, with thinking disabled) but runs at 0.5 t/s here, so that answer takes
      45 s — longer than the speech it drives. A 7–8 B instruct model would land at
      ~5 s per answer and still be unreliable. Settled as a deterministic calculator:
      `bridge/src/arithmetic.rs` answers exactly one number, one operator and one
      number — Russian operator words and symbols, comma decimals, signed operands,
      15-digit bound — before any model is called, and leaves anything wider to the
      model untouched. Seen on the frame: `ai ask Сколько будет 458 умножить на 714?`
      → `458 умножить на 714 равно 327012.` in `llm_ms=32` (the model had said
      `327 732` in 1422 ms), and a control question («Столица Финляндии?») still
      reached the model (`llm_ms=892`). Seven native tests cover the wrong multiply,
      symbols, decimal commas, a signed operand, division by zero, and the wider and
      ordinary questions that must not be captured.
- [x] **C+E. One press, the whole turn.** `ai talk 8000` on hardware, pressed by
      hand, ran every stage in order and reported each one:
      `waited_ms=58` (KEY) → `capture_ms=9240` (8.00 s, `norm_db=6.6`,
      `peak_l=7702`) → `roundtrip_ms=3185` → `transcript="Это фоторамка."` →
      `paint_ms=31070` (`heard_lines=1 answer_lines=2 bottom_y=224`) →
      `llm_ms=35 tts_ms=434` → `play_ms=7125` for `seconds=6.98`. The panel was then
      confirmed by eye: heading, the utterance, and the answer, correctly rotated and
      laid out. An earlier run with nobody at the button answered
      `ERR ai.listen: no KEY press within 30 s`, so the limit works, and a press that
      landed in silence correctly reported `ERR ai.stt status=500 … whisper heard
      nothing` rather than inventing a sentence.
      KEY (GPIO4, polled, 30 s limit) → cue → capture → `/stt` → transcript drawn on
      the panel; `ai talk` continues through `/llm` → `/tts` → speech and then draws
      the question and the answer. Painting is last on purpose: a Spectra 6 refresh
      costs tens of seconds, so it happens after the console report exists and, in
      `ai talk`, after the voice has said its piece. Text needed a font with
      Cyrillic, which a hand-drawn 5x7 table cannot provide: `tools/mkfont.py`
      rasterises DejaVu Sans at 26 px into `src/font.rs` (169 glyphs, 13 KiB of
      one-bit bitmaps, committed), and `panel::draw_text`/`wrap_text`/`draw_paragraph`
      blit into the panel's rotated 4bpp layout. Native coverage: the blitter is
      compared bit for bit against the generated table, wrapping keeps every line
      inside its column, a word too wide for the column is broken rather than
      dropped, an unknown code point costs the width of `?` and draws nothing, and a
      paragraph stops at its line limit and at the bottom edge. `paint_turn` is the
      only untested part — it needs the panel, so the next hardware run is what
      reports `paint_ms`.
- [x] **Idle watcher and voice trigger.** `ai idle off|key|voice` decides who may
      start a turn with nobody typing. Both modes run inside the console loop
      between reads, not in a thread: the codec, panel and radio keep one owner, and
      the loop already wakes every 20 ms. `key` latches a press and needs it
      released once, with a 2 s cooldown so one press is one turn; a watcher-started
      turn listens 4 s instead of the console default 3 s. `voice` keeps the ADC and
      the ALDO3 rail open, assembles utterances with `src/vad.rs` (energy detector,
      1 s pre-roll, 700 ms hangover, 10 s cap, gate at `max(4 x noise floor, 600)`
      capped at 4000; 12 native cases: a quiet room never triggers and leaves the
      gate at 600, speech over that noise still triggers, a steady loud room stops
      triggering, a quiet voice after a loud room is heard again, the trigger needs
      160 ms of voice, the utterance starts before the trigger, the pre-roll is
      bounded and survives, a long noise is cut at the cap, a threshold above the
      signal stays quiet, reset forgets, rms is correct). The 600 floor is the third
      value tried on hardware: 80 and 120 sat at or below this room's own noise and
      set the frame talking to televisions and doors, while 600 stays quiet and still
      hears a question across the room.
      `rms` uses f32 because the S3 has no double-precision FPU. The microphone is
      closed before the answer is spoken, so the frame cannot transcribe itself, and
      reopened on the next tick. A Spectra 6 refresh takes ~31 s, so an idle turn now
      queues its text and returns to the loop immediately; a short-lived `epd-paint`
      thread (`Painter`) starts the refresh only after 15 s without voice, so a
      conversation produces one paint of its final answer rather than a queue of
      obsolete ones. The microphone reopens while the panel is still drawing.
      Console turns stay synchronous because the person at the terminal is already
      waiting for the report, and a console display command waits for any background
      refresh (and drops the queued text) before using the panel. The mode persists
      as `ai.idle` in `CONFIG.TXT`
      (`config save`) and is applied at boot; `voice` reports when it opens the
      microphone, and a failure reverts to `off` and says why instead of pretending
      to listen. `IdleMode` lives in `config` and is re-exported by `console`, so the
      console grammar and the file parser share one set of names (the console test
      harness includes both modules).
      **Background refresh is verified on hardware**: three questions arrived 20 s
      and 17 s apart while the panel was refreshing, all captured and answered, and
      the frame stayed online. **Quiet deferral is not yet on hardware**: this build
      waits 15 s after the last voice before starting the refresh.
- [x] **Flashed the release profile.** 2026-11-02: release written at `0x10000`
      (1 128 400 bytes), hash verified, booted `wake=cold`; `check.sh` prints
      `headroom=`/`fits=` for both profiles so a profile that no longer fits cannot slip
      through. The dev-size numbers in this item are stale — the current debug build
      fits too (see above).
- [x] **Partition ceiling.** Settled by `partitions.csv`: two 3 MB app slots and
      no factory, which is what OTA needs anyway (P7.9). `scripts/check.sh` now
      reads the ceiling from that file instead of sdkconfig, and refuses a table
      with fewer than two app slots. The change is a flash-time one — bootloader,
      table at `0x8000`, app at `0x20000`, `otadata` erased — so it is done with
      the next approved flash, not by this build.
- [x] **Speech gate before every upload.** `pcm::heard_speech` (peak ≥ 300 **and**
      rms ≥ 60) refuses a capture that carries no more energy than an idle room, so
      silence becomes `ERR audio utterance: microphone heard nothing (peak_l=… )`
      locally instead of three seconds of network plus three seconds of whisper — and
      instead of an invented sentence. Both numbers are required because a click
      spikes the peak and a hum lifts the rms; five native cases cover idle, speech,
      distant speech, a click and a hum. Thresholds are placeholders in the honest
      sense: 220/21 was measured silence and 1711..3158/157..247 was measured speech,
      so 300/60 sits between them with margin on both sides.
- [x] **Stage timings split.** `Utterance` now carries `cue_ms` and `capture_ms`
      next to `roundtrip_ms`, so a 5 s turn reports where its ~8 s went instead of one
      number. `capture_ms` is expected to exceed the requested duration: it includes
      the discarded one-second ADC settle.
- [x] **A real synthesised voice for the bridge.** `bridge/scripts/tts-espeak.sh`
      turns `cmd:` into Russian speech: espeak-ng (`-v ru -s 140`) piped through
      ffmpeg into raw 16-bit mono 16 kHz, which is exactly what `CommandBackend`
      expects on stdout. Over HTTP it answered `POST /tts -> 200 in=65 out=57997
      91 ms`, so `/tts` no longer needs to answer with a tone. Robotic on purpose:
      the point is that the loop speaks, and a neural voice dropped into the same
      `cmd:` slot later without touching firmware — which is exactly what happened,
      see the piper entry below.
- [x] **D. LLM → TTS → speaker, `ai ask <text>`.** `/llm` → `/tts` → µ-law decode →
      16k→24k resample → `audio.play_speech` with the PA on, off afterwards. The rate
      comes from the reply (`Speech.sample_rate`), not from a constant, so a host that
      answers WAV instead of µ-law works unchanged. Verified on hardware:
      `ai ask` reports `llm_ms=60 tts_ms=1812 conv_ms=1065 play_ms=7265`, i.e. 7.12 s
      of speech spoken in 7.27 s, and the answer is audible in the room.
- [x] **Console accepts UTF-8.** The framing rejected anything outside ASCII, which
      made `ai ask` useless for a Russian assistant — the prompt is two bytes per
      letter. Control bytes and `DEL` still invalidate a line, everything printable
      passes through, and the 160-byte limit still bounds it (60 Russian letters fit;
      a test asserts it).
- [x] **C. Capture → STT → screen.** KEY (GPIO4) → cue → capture → `/stt` →
      transcript on the panel. There is no VL53L0X on this board, so the sketch's
      100 mm distance trigger did not port; a press is the trigger instead.
- [x] **E. Full round trip on a button.** One press: record, recognise, answer,
      speak, then both texts on the panel. Every stage reports its own duration, so a
      silent failure at any of the four no longer looks identical from outside.
      Measured turn and its numbers: the C+E entry above.

**Open choices, not blockers:**

1. **Settled: piper**, through `bridge/scripts/tts-piper.sh`. espeak-ng was the
   first voice precisely because it is one command away, and it was then mistaken for
   a hardware fault by ear ("looks like the rate is wrong") — the measurements that
   cleared the DAC and convicted the voice are in the P1 entries above. piper needs
   one 60 MB model fetched once and adds ~1.4 s per answer; `espeak-ng` stays as the
   fallback for hosts without it, which is what the `cmd:` abstraction was for.
2. **Settled: a local model**, `bridge/scripts/llm-local.py` in front of
   `llama-server` with Phi-4-mini Q4_K_M behind it. The Gemini free-tier key was the
   cheaper option and was not taken: with weights already on this host, "free" and
   "offline" are the same answer, and the key would have been the only credential in
   the whole system. Measured on 24 cores without a GPU, Russian prompt "Почему небо
   голубое?": Phi-4-mini 1.3 s for a correct one-sentence answer; granite-4.2-3b
   9.6 s and LFM2.5-8B-A1B 5.7 s both spent the entire budget on English
   `reasoning_content` and never answered, which disqualifies them when the answer is
   read aloud; T-lite-instruct 8B is Russian-tuned but runs at 2.8 t/s (~20 s per
   answer); the 27B Qwen needs the GPU this host does not have. Reasoning models are
   wrong for this device by construction — a trace is text nobody hears.
3. **Settled: `rwhisper` in-process**, with the vendored patch and `--stt whisper:small`
   as the documented default. The `cmd:` route stays supported for hosts where the
   weights do not fit or the ALSA headers are missing. Cost measured here: ~470 MB of
   weights in `~/.cache/kalosm`, ~9 s per 10 s utterance on one core, 16 s for silence
   (the decoder retries with higher temperatures before giving up).

   **Updated: the model changed to `Qwen3-4B-Instruct-2507-Q4_K_M`.** Same host, same
   `llama.cpp` b11938, no GPU: 21.6 t/s against Phi-4-mini's 5.5, 1.4-2.1 s per
   one-sentence answer, and the frame's own turn now reports `llm_ms=2140` for
   `total_ms=11734`. Weights live in `~/models/crane/qwen3-4b/` (the directory name
   is a leftover from the Crane experiment below, not a hint about the engine).
4. **Settled against: Crane on ROCm**, although the engine itself is a good fit when
   it works. `crane-serve 26.9.0` with `Qwen3-4B` on the Radeon 890M answered in
   1.4-1.8 s at 20.8 tok/s, so the GPU is worth having; what rules it out is that it
   dies with a `[gfxhub] page fault` while admitting a request, `/health` having said
   `ok` a moment earlier. It survived a 20-request soak and then faulted on the next
   turn, so it is not a configuration mistake that a flag fixes (`--max-concurrent 1`,
   `--gpu-memory-limit 8G`, `--context 2048` all still fault). Its Gemma GGUFs are
   unusable for a different reason: `▁` is never turned back into a space, and
   Cyrillic comes out as `U+FFFD`/`€`, i.e. the tokenizer is fed mojibake. Five
   defects, each with a reproduction, are written up in `~/todo/crane-report.md`.
   Revisit when Crane fixes its detokenizer and its iGPU memory handling.

## P2 — Gallery (slideshow from the card)  *(done, verified on hardware)*

- [x] `gallery start [<dir> [<secs>]]`, `next`, `stop`, `status`; `.bmp` and `.el6`
      from one directory, sorted, wrapping. `gallery.rs` holds the scheduling as
      pure logic with 9 native tests; `storage::images` does the listing.
- [x] Persisted as `gallery.dir`, `gallery.secs` (120..3600, clamped with the reply
      saying so) and `gallery.auto` (starts at boot). A test in the console harness
      keeps the file's bounds equal to the scheduler's, since the two are compiled
      separately.
- [x] Advances only when `Idle::quiet()` and the panel is ready, because the
      refresh is what makes the frame deaf. Refusals are counted once per deadline.
- [x] Decode on the main loop (one owner for the card), refresh on the paint
      worker (`PaintJob::Frame`).
- [x] **Acceptance on hardware** (2026-11-01, image 1 322 816 bytes):
      `sd ls` alone now lists the root (8 entries). `gallery start 06_USE~1 30`
      reported `images=6`, `advances` climbed 0 → 4 over three minutes, wrapped
      (`next=4.BMP` after `SYS_DE~1.BMP`), and `skipped` climbed 1 → 4, so the
      room-quiet/panel-free gate is live. `gallery next` walked a decoy
      (`ERR BMP dimensions must be 800x480, got 736x1325`) and then painted
      `800X480.BMP`; `gallery stop`, `status` and a `next` while stopped behaved.
      `gallery start nosuchdir` → `ERR sd list /sdcard/nosuchdir: Invalid argument`;
      `gallery start 01_SYS~1 15` → `requested_s=15 clamped_to=30` (the floor was
      30 s for that run; it is 60 s now, see below).
- [x] **Pictures were already on the card.** `06_USE~1` holds three real
      800x480 BMPs (`4.BMP`, `800X480.BMP`, `SYS_DE~1.BMP`, 1 152 054 bytes each)
      mixed with 1200x675 / 480x480 / 736x1325 decoys — the mixed-card case the
      postpone-on-failure rule exists for. `05_USE~1` has the same set plus
      `800X480.PNG`. The eight 12 KB files in `01_SYS~1` are 64x64 label cards.
- [ ] **Answer wins the panel mid-slideshow.** Needs a KEY press, so it is the one
      acceptance line still unwatched: start the slideshow, `ai talk 8000`, confirm
      the deferred refresh follows the answer instead of fighting it.
- [x] **Floor raised 30 s → 60 s → 120 s.** A 31 s refresh inside a 30 s interval
      meant the panel was always drawing and a finished picture was never visible.
      The floor is now refresh plus a long dwell; tests step off `MIN_INTERVAL_SECS`
      instead of a literal 30, which is how the two went out of step.
- [x] `gallery next` includes the failing path in decode errors
      (`src/main.rs`). The pathless error above records the earlier hardware run.

## P2a — Upload path: the wire format is `.el6`, converted in the browser

- [x] `tools/el6.mjs`: pure resize rectangle (`cover`/`contain`), Floyd-Steinberg
      against the panel's six colours, and the 4bpp 180-degree native packing.
      No canvas, no DOM, so the page and Node call the same code.
- [x] `tools/check-el6-js.py` proves it matches `tools/convert.py` byte for byte
      (both dithered and nearest-colour) on a gradient, seeded noise, exact
      palette blocks, a 64x64 upscale and real phone photos (2304x4096):
      `OK 6 images, both modes, byte-identical`. In the gate as `el6-js`.
- [x] `convert.py` split into `convert()` and `frame_from_rgb()` so the two
      converters can be fed identical pixels; `--no-dither` added to both.
- [x] The portal serves an upload page; `web/index.html` converts images with
      `packImage` and sends the packed frame to `POST /upload`.
- [ ] Any-size BMP decode on the frame, for uploads and scripting that do not go
      through the page. Streaming (`Read`, not `Read + Seek`) so the same decoder
      can read the card and a request body; box-average downscale, nearest
      upscale, `contain`/`cover`.

## P2b — Fetch an image over HTTP and draw it

- [ ] Bring the link up first: `ensure_online` already prefers stored credentials,
      and the HTTP path must reuse it rather than growing a second join path.
- [ ] `display url <http://host/path.bmp>` — same decoder as `display bmp`, fed from
      a buffer instead of a file, so the two paths cannot drift.
- [ ] Plain HTTP only, and that is now permanent rather than a first step: the
      assistant went local (P1), so nothing in this firmware needs TLS. If an image
      ever has to come from an HTTPS origin, terminate it on the bridge or a proxy
      on the LAN and fetch plain HTTP from the frame.
- [ ] Bound the download: a `Content-Length` above the decoder's limit is refused
      before the body is read, and a body that lies about its length is cut off.
- [ ] Read into PSRAM, not the heap the panel framebuffer already uses.

**Acceptance:** a 800x480 BMP served from the LAN draws correctly; an HTML error
page is rejected with a clear message rather than drawn as noise; a URL that never
answers times out instead of hanging the console.

## P3 — Upload page

- [x] The frame serves the portal and accepts an exact-size `.el6` frame at
      `POST /upload`; the browser performs image conversion.
- [x] The server checks the upload length before reading the body. The portal
      also provides gallery, settings and OTA routes.
- [x] The portal supports an optional HTTP Basic password, `X-Easellet` checks
      on POSTs and a local `Host` restriction. The default is no password;
      plain HTTP does not encrypt credentials or traffic.
- [ ] Confirm on hardware that an upload during refresh is refused clearly and
      an oversized body is rejected without exhausting PSRAM.

## P4 — Scheduled refresh  *(depends on P2)*

- [ ] Decide whether the proposed `refresh.url` and `refresh.interval_min`
      settings are still needed alongside the existing bridge picture fetch.
- [ ] Wake → join → fetch → draw → refresh → sleep, with the RTC as the clock.
- [x] ESP deep sleep with button and timer wake is implemented (P7.1).
      Timed wake can fetch a bridge picture when `bridge.url` is configured;
      otherwise it uses the card slideshow. This is not the proposed
      `refresh.url`/RTC-based workflow.
- [ ] Report the last attempt and its outcome through `info`, since nobody watches
      the console during a wake.

**Acceptance:** three unattended cycles at a short interval, each drawing a
different image; time between cycles matches the configured interval; no flash
writes occur (NVS stays unused).

## P5 — Loose ends worth fixing

- [ ] **Boot join timeout.** With `wifi.auto=1` and no access point, every boot
      waits out the full 15 s, and the reason is invisible because opening the USB
      port resets the chip. A shorter boot-specific timeout (3-5 s) with a retry
      afterwards would cut that to a tolerable delay.
- [ ] **RTC self-heal is unexercised.** `wifi_auto=connected time=rtc_valid` skips
      SNTP, which is correct but means the healing branch has not run in this build.
      Testing it needs an RTC invalidation — an approval item, not a code change.
- [ ] **Passphrase with spaces is unrepresentable.** The console has no quoting, so
      `wifi join` cannot send a passphrase containing a space. Probably acceptable;
      confirm rather than assume.
- [x] **`PLAN.md` is stale.** The roadmap now distinguishes implemented code
      from the original acceptance gates.
- [ ] **BMP row buffer allocation.** `decode_bmp` allocates a `Vec` per call; for a
      long-lived frame this is fine, but the HTTP path will call it repeatedly, so
      it may belong in the caller.
- [x] **`ai ask` disarmed the voice watcher.** Playback stops the I2S driver when it
      finishes, but `play_pcm` left `monitoring` set, so the next idle tick read a
      stopped driver (`ERR audio monitor read: ESP_ERR_INVALID_STATE`) and
      `voice_poll` turned the watcher off — `ai idle` then said `off`. Whether the
      ring also fed the frame its own answer never got that far. `play_pcm` now
      closes the monitor first, so `voice_poll` reopens the microphone after the
      answer exactly as it does after a cued turn. Seen on hardware (flashed
      2026-11-02): `ai ask Сколько будет 458 умножить на 714?` reported
      `seconds=6.77 play_ms=6917`, no `ERR audio monitor read` line followed, and
      `ai status` still said `ai.idle=voice monitor=on state=idle` with `frames`
      advancing (845→1106).
- [x] **Both web pages are localised, English by default.** A page that opens in
      a language nobody chose is unreadable to a stranger, and both the frame
      portal and the bridge UI opened in Russian. Every static string is now a
      `data-i18n` attribute and every string the script builds goes through `t()`
      against a small `en`/`ru` dictionary; the header selector switches live,
      stores the choice in `localStorage`, and the next visit starts with it. A
      new language is one object in `STRINGS` plus its autonym in `LANGUAGES`.
      `ai.lang` stays untouched — that is the language the frame listens and
      answers in, not the page's. Checked in a browser: `portal-harness.html`
      passes 22 checks against `tools/portal-mock.py`, opening in English («Photo»),
      switching to «Фотография», keeping the stored choice and then running the
      rest in Russian; the bridge UI was driven through all four tabs in both
      languages including the rewritten settings-field metadata. Bridge 70 tests,
      `scripts/check.sh` and `tools/check-portal-js.mjs` stay green.

## P6 — OpenDisplay protocol (LAN first, BLE later)

Goal: py-opendisplay, the Home Assistant integration and the opendisplay.org
toolbox drive the frame unchanged. Design and protocol notes:
`research/opendisplay-plan.md`, `research/opendisplay-handoff.md` (read the
corrections at the top of the handoff first). Upstream references are cloned under
`research/ref/` (`opendisplay-firmware` is GPL-3.0, `py-opendisplay` is MIT). The
project is GPL-3.0-or-later (`LICENSE`), so upstream firmware code may be reused
with attribution; the plan's "implement from headers only" constraint is gone.

Layout: `opendisplay-proto/` is a separate host-tested crate like `bridge/`
(`opendisplay-proto/build.sh test`), `no_std`, no alloc, no `unsafe`, sans-IO. The
firmware will depend on it by path.

Settled while checking the repo:
- The firmware is ESP-IDF `std` with threads, not Embassy: the core's platform hooks
  are plain synchronous traits, the LAN server is a thread with `std::net`, and BLE
  will be NimBLE through IDF.
- Report `DisplayConfig.rotation = 2` (180°). Clients rotate the source image by it
  before packing (py-opendisplay `device.py`, upstream "host bakes rotation"), so
  incoming bytes are already in the panel's native order and go into the el6 frame
  unchanged. The nibble codes already match colour scheme 4.
- Phase order changed from the plan: PIPE (0x80-0x82) is forbidden on LAN, so it
  moves after first light, right before BLE.

### Phase 1 — core crate, everything LAN needs (host only)
- [x] Crate scaffold, `build.sh`, upstream headers vendored with their commit
      (`opendisplay-proto/upstream/SOURCE.md`). In the `scripts/check.sh` gate.
- [x] Constants (`wire.rs`): opcodes, status bytes, per-opcode NACK enums, limits.
      `tests/header_sync.rs` parses the vendored headers and fails on any drift in
      opcodes, constants, packet ids or struct sizes (checked by breaking one value).
- [x] Framing: `Request::parse` (borrowed views), `Response` built on the stack.
- [x] Config blob (`config.rs`): CRC16 with the zeroed-length quirk, packet walk by
      the size table, unknown packets kept as an unparsed tail, `BlobWriter`, and
      zerocopy structs for system, manufacturer, power, display, security, names.
- [x] Config read in ≤100-byte frames; config write reassembly (202-byte first
      frame, 200-byte chunks, at most 20 frames, CRC checked before storing).
- [x] Direct write (`image.rs`): raw and zlib inflated straight into the frame;
      END with optional BE etag; 0x73/0x74 through `Server::refresh_finished`.
- [x] `Server` dispatcher without security, driven through the `Device` trait.
- [x] Golden vectors (`tests/server.rs`), hand-written from the header and from
      py-opendisplay's request builders, plus a 512-byte-window zlib fixture made
      by `tests/fixtures/make_fixtures.py` (stdlib zlib, the client's settings).
- [x] LAN framing (`lan.rs`): prefix check, 1..=4094.
- [x] Vectors from py-opendisplay itself, both directions. The client is installed
      editable from `research/ref/py-opendisplay` into `.tools/venv-opendisplay`.
      `tests/fixtures/make_client_vectors.py` has the client parse our config
      (`board-config.bin`, kept equal to the test's `board_config()`), then build
      every request from it, including an image through `prepare_image()`. Its own
      assert shows rotation 2 puts the viewer's top-left at the end of the buffer.
      `tests/client.rs` replays the requests; `check_client_responses.py` runs our
      responses through the client's parsers (`build.sh test` does it when the venv
      exists; "client accepted 860 responses").

**Acceptance (met 2026-11-01):** `opendisplay-proto/build.sh test` (37 tests plus the
client check) and `clippy -D warnings` clean; a client-prepared 192,000-byte frame
goes through START/DATA/END over LAN-sized and BLE-sized chunks and matches byte for
byte.

Firmware note: `FirmwareVersion.sha` must not be empty; the client rejects a
version without a hash.

Behaviour choices to revisit if a client disagrees: no auto-complete when the last
raw byte arrives (clients accept either); unknown or unsupported opcodes get a bare
`[0xFF][echo]`; AUTHENTICATE without a key answers `NOT_CONFIG`; a zero etag means
none.

### Phase 2 — firmware integration
- [x] Path dependency; the crate builds for `xtensa-esp32s3-espidf` with no warnings
      and `scripts/check.sh` passes.
- [x] Board config builder in the crate (`board::photopainter_config`): ESP32-S3,
      Wi-Fi, Waveshare, USB power, panel 35, 800x480, scheme 4, rotation 2, zip +
      streaming decompression + direct write, pins 0xFF, names. The client fixtures
      now use it, so py-opendisplay checks the real config.
- [x] `Server` boxed on its own thread (24 KB stack); `Device::read_config` returns
      a borrowed blob, which took 4 KB out of `Server` (now about 15 KB).
- [x] `src/opendisplay.rs`: the `Device` impl and the TCP 2446 thread, shaped like
      `web.rs`. A staging frame (PSRAM, allocated on first image) fills on the
      thread; `Job::Show` hands it to the main loop, which paints only when the
      panel is free (same rule as a portal upload, nothing saved to the card).
      `Painter::refresh_later_then` reports the refresh result back for 0x73/0x74.
      Names a client writes go to `EASELTEST/OPENDISP.BIN`; everything else it writes
      is ignored. Deep sleep answers "unsupported"; MSD carries only the company id;
      firmware version is the crate version with the crate name as the hash.
- [x] Size: release image 1,453,440 → 1,512,032 bytes (+58.6 KB; miniz_oxide 17 KB,
      protocol crate 6 KB, firmware module 11 KB, the rest std/lwIP pulled in by the
      thread). Headroom in the 1,536,000-byte factory partition: 23,968 bytes. The
      handoff's "~300 KB headroom" was wrong: it was 82 KB before this work.
- [x] On hardware 2026-11-01 (release image, app at 0x10000, logs
      `research/device/flash-opendisplay*.log`). The first flash boot-looped
      (`RTC_SW_CPU_RST` right after `web listening`): building `Server` overflowed
      the 24 KB thread stack (`serve` frame 14,944 B + `Server::new` 10,544 B, read
      from the ELF's `entry` instructions). Fixed with an out-of-line `new_server`
      and a 40 KB stack; `serve` is now 368 B. Release image 1,511,424 bytes,
      headroom 24,576.

Known interplay: a running slideshow replaces an OpenDisplay picture at its next
advance, as it does a portal upload.

### Phase 3 — LAN first light
- [x] TCP 2446 server thread, one client, new connection evicts the old, 30 s idle
      (built in phase 2; starts with the web portal's link, stops with the radio).
- [x] mDNS `_opendisplay._tcp`, done 2026-11-01 as a small Rust responder instead
      of IDF's managed component (15-30 KB; only 24 KB was left). Crate module
      `mdns.rs` (no_std, host-tested: queries with compressed names, legacy
      unicast, enumeration, announce and goodbye); python-zeroconf parses its
      answers to zeroconf's own browse/resolve queries in `build.sh test`
      (`check_mdns_responses.py`). The firmware polls it from the OpenDisplay
      thread, so no extra stack: two announcements a second apart, a goodbye on
      stop. Name `OD` + low three MAC bytes (upstream's scheme): `ODCAED44`. TXT
      `mac` (base MAC + 2 = 94:a9:90:ca:ed:46), `tls=0`, `fw`, `cm=04`, `pv=2.2`.
      Cost 9,152 bytes; release image 1,520,576, headroom 15,424.
      On hardware: `opendisplay scan --lan` and `avahi-browse` both find
      `ODCAED44` at 192.168.50.204:2446 with every TXT key; `avahi-resolve` answers
      the A record. (`opendisplay info --host ODCAED44.local` fails on this PC only
      because its nsswitch has no mDNS; HA and the scanner use the records.)
- [x] TXT `msd` (28 hex digits of telemetry), real PMU and SHTC3 data (P7.5).
- [ ] Home Assistant: add the frame through the OpenDisplay integration's discovery.
- [x] Flash budget, applied 2026-11-02 (`research/size-optimization.md`): release
      image 1,520,576 → 1,111,440 bytes, 424,560 free. the DHCP server
      stays on (linking needs it); IPv6 is off since the power-save fix below; main task stack 12 KB;
      `info` reports stack and internal-RAM low-water marks. The OpenDisplay idle
      timeout no longer drops a client waiting for its refresh.
- [x] Security review fixes (2026-11-02), checked on the frame:
  - Portal POSTs need an `X-Easellet` header (403 without), so another site's
    page cannot change `ai.url`/`ai.key`, delete pictures or clear the password
    through a LAN visitor's browser. DNS rebinding can still read GET routes;
    a `Host` check would close that.
  - OpenDisplay Reboot is a main-loop job (never mid card write) and
    `opendisplay.reboot=0` refuses it. Seen: `rst:0xc (RTC_SW_CPU_RST)` after
    `opendisplay reboot --host`, only once the AI request in progress ended.
- [x] Remaining review items (2026-11-02), checked on the frame:
  - `Host` must be an IPv4 address or a local name (403 for `evil.example`);
    closes DNS rebinding reads.
  - Whole request within 30 s: a byte-every-4-s client was dropped at 33 s and
    the page answered again. Head read in 512-byte chunks, not a byte per recv.
  - `http::post` stops at its limit instead of draining the rest.
  - Uploads only inside `EASELTEST` (`image::is_writable_dir`); `gallery.dir`
    elsewhere still drives the slideshow.
  - Listings read name, size and attributes in one FATFS `f_readdir` pass
    (`Storage::pictures`) instead of two `stat`s per picture; the 512 kept are
    the alphabetically first (`gallery::keep_first`). Uploads pick their number
    from one `read_dir` (`gallery::first_free_upload`). Verified: listing and
    sizes unchanged, upload became UP0003 and deleted cleanly. Speed-up not
    measured: the card holds two uploads.
- [ ] Watch internal RAM before BLE: its low point was 42,608 bytes free, and
      the BLE controller and host want 40-80 KB of it.
  - [x] OpenDisplay thread stack (40 KB) in PSRAM via `esp_pthread_cfg_t.stack_alloc_caps`
        (2026-11-02): `internal_free` with the servers running 116,343 → 159,395;
        low point after boot + one LAN upload 85,612. Not yet re-measured after a
        slideshow session with the voice watcher (where 42,608 was seen).
  - [ ] Next candidates: `CONFIG_SPIRAM_TRY_ALLOCATE_WIFI_LWIP=y`; the web (12 KB)
        and paint (16 KB) stacks only after checking they never hand stack buffers
        to SPI/DMA or run with the flash cache off; NimBLE with external allocation.
- [x] mDNS missed multicast queries while Wi-Fi modem sleep was on (default
      `WIFI_PS_MIN_MODEM`): idle frame 0/12 answers, 9/12 while pinged. Same on
      2026-11-01's build, so `opendisplay scan --lan` had been intermittent all
      along, and "IPv6 off breaks multicast" was this instead. Fixed 2026-11-02:
      `Net::set_power_save(false)` when the OpenDisplay server starts (radio
      stays awake until `wifi off`); then `LWIP_IPV6=n` (release image
      1,089,008 bytes, 446,992 free). Idle 12/12, scan 3/3, avahi, web page,
      upload, and rediscovery after `wifi off`/`wifi join` all checked.
- [x] (Superseded) a custom partition table is now `partitions.csv`, written at
      flash time by esptool rather than named in sdkconfig — the route
      esp-idf-sys documents. Raw uploads over LAN are only 47 frames of 4,092
      bytes, so zlib stays.

**Acceptance:** py-opendisplay over IP reads info and config, uploads an image, the
panel shows it and 0x73 arrives. *Met on the wire 2026-11-01:* `opendisplay info
--host 192.168.50.204` shows the generated config; `opendisplay upload --host` sent
192,000 → 18,992 bytes compressed in 5 chunks, got "Display refresh complete",
29.9 s end to end (`research/device/opendisplay-first-light.log`); the server
answered again afterwards. Visual check passed: an edge test (4 px border, a
different colour per side, corner labels 12 px in) shows all four borders and labels
in place, so orientation and alignment are right. (A first test card looked shifted
only because its own labels were placed at x=9 and ran past x=799.)

### Later phases
- [ ] PIPE (SACK, 33-slot reorder queue), needed only for BLE.
- [ ] BLE GATT 0x2446 via NimBLE and MSD advertising; remeasure internal RAM and
      image size before choosing an implementation.
- [ ] Security: auth, session KDF, CCM envelope, replay window.
- [x] OpenDisplay DeepSleep is implemented; protocol PowerOff remains unsupported.
- [ ] Evaluate bridge dithering via `epaper-dithering-core`.

## P7 — Vendor parity (power, buttons, provisioning, pictures, AI tools)

Goal: everything the Waveshare firmware does (`research/Instructions-For-Use.txt`,
`research/vendor/01_Example/xiaozhi-esp32`), except that the "cloud" is our LAN
bridge, never Xiaozhi or Volcengine directly. Survey of gaps: 2026-11-02.

**Approval gates still apply to every phase:** flashing, PMIC writes (only if
P7.1 turns rails off; buttons need none), RTC writes, SD writes outside `/sdcard/EASELLET`.

**Decisions to confirm before the phase that needs them** (recommendation first):
1. Sleep: ESP deep sleep with ext1 wake on BOOT (GPIO0) / KEY (GPIO4), ext0 on
   PWR (GPIO5) and the timer, as the vendor does; PMIC sleep only if measurement
   says so.
2. Modes: keep the vendor's three (Basic / Network / AI), selected by a long KEY
   press, but as *profiles* of settings we already have (`gallery.auto`,
   `wifi.auto`, `ai.idle`, sleep) rather than separate code paths.
3. Weather and generated pictures are rendered and dithered **on the bridge**
   and sent as `.el6`; the frame never decodes JPEG. On-device JPEG only if a
   bridge-less path is wanted later.
4. Wake word stays on the bridge (VAD + the existing «Рамка» stem in
   `bridge/src/server.rs`), not ESP-SR/WakeNet: no Russian model, ~300 KB+ flash.
5. SoftAP provisioning comes back (`ESP_WIFI_SOFTAP_SUPPORT=y`, ~+25-50 KB of the
   442 KB headroom), WPA2 with a per-device password shown on the panel.
6. OTA needs two app slots: `partitions.csv` now defines them (two 3 MB slots,
   no factory), and the table is written by the next approved flash.

### P7.0 — Investigation (read-only, no flashing)  *(done 2026-11-02)*
Findings: `research/p7-investigation.md`.
- [x] PWR also reaches **GPIO5** (active high, through BSS138 T1), as the vendor
      reads it: a click needs no PMIC IRQ and no PMIC write. Wake: BOOT+KEY on
      ext1 any-low, PWR on ext0 high. Long hold = PMIC hardware off after
      OFFLEVEL (reset value 6 s); the firmware sees it coming but cannot veto it.
- [x] LEDs active low: red GPIO45 = "operable", green GPIO42 = blink during a
      refresh. eFuse forces VDD_SPI to 3.3 V, so GPIO45's strap is moot.
- [x] Vendor deep sleep: ESP deep sleep, ext1 + timer, all PMIC rails left on,
      waits for BOOT release before sleeping. Ours adds panel/codec/PA power-down.
- [x] Battery: percent already read; VBAT in REG34/35 (1 mV/bit), ADC on at reset.
- [x] RTC memory: `.rtc.data` survives deep sleep in IDF 5.5; state < 100 B.
- [x] P7.1/P7.2 recorded `pwr_pressed`, PMIC register/voltage reporting and a
      BOOT-held wake without entering download mode.
- [ ] Sleep-current measurement: USB power meter if available, else battery
      percent over 24 h.

### P7.1 — Sleep and timed wake  *(done 2026-11-02, checked on the frame)*
- [x] `power.sleep_secs` (0 = never, default; 60..86400) and `power.awake_secs`
      (30..3600, default 120). Settings file limit raised to 4 KB / 64 lines, with
      a test that a file with every key at its longest reads back.
- [x] `src/power.rs` (host-tested policy), `src/sleep.rs` (ext1 BOOT+KEY any-low,
      ext0 PWR high, timer; state in `.rtc.data` at 0x5000_0000).
- [x] Timer/BOOT wake: no Wi-Fi, no microphone; continue the slideshow after the
      remembered picture, sleep when painted. KEY or console input turns it into
      a full boot (`join_at_boot`, `restore_idle`).
- [x] Sleep on PWR click, BOOT hold, `sleep [secs]`, OpenDisplay DeepSleep
      (override >= 60 s), or `awake_secs` idle; never mid-refresh, mid voice
      turn or with an OpenDisplay client. Shutdown: finish refresh, wait for
      buttons up, mDNS goodbye, radio off, codecs off, panel sleep, lights off.
- [x] `info`: `wake=… wakes=… sleep_secs=… awake_secs=… idle_s=…`.
- [x] Seen on the frame: `sleep 60` -> timer wakes at +60 s, each refreshed and
      slept 33 s later; KEY wake joined Wi-Fi (`wake=key wakes=4`); PWR click
      slept; BOOT woke, painted, slept (BOOT held at wake did *not* enter download
      mode); PWR woke (`wake=pwr wakes=6`); `opendisplay sleep --host` slept within
      1 s and PWR woke it (`wakes=7`).
- [x] `tools/udev/70-easellet-esp32s3.rules` (uaccess for 303a:1001), installed:
      every wake re-creates the tty, and without it the user lost access each time.
- [ ] Not exercised on hardware: the `power.awake_secs` idle sleep (needs
      `power.sleep_secs` in `CONFIG.TXT`); same code path as the PWR click.
- [ ] py-opendisplay `sleep --host` prints a short-read error because the LAN
      transport, unlike BLE, does not treat the dropped link as success; upstream
      ESP32 firmware does not ACK either. Client-side quirk, command works.
- [ ] Sleep current not measured (P7.0: USB meter or battery percent over 24 h).
- [ ] Buttons are still not sampled while an AI turn blocks the loop (P7.2 item).

### P7.2 — Buttons and lights  *(done 2026-11-02, checked on the frame)*
- [x] `src/buttons.rs` (host-tested): 30 ms debounce, click on release, one hold
      at 1.5 s (the PMIC's own long-press threshold), a press already down at boot
      or consumed by a console command is ignored until released.
- [x] BOOT click = next picture (starts the slideshow from `gallery.dir` if it
      was off; refresh on the paint worker). Seen: `OK boot next
      path=EASELTEST/UPLOADS/UP0001.EL6 paint=started`.
- [x] KEY click = AI turn when `ai.idle=key`, now on release (hold is reserved);
      KEY hold logs "mode menu arrives with P7.3". Both seen.
- [x] PWR (GPIO5, active high) click and hold detected; they only log until
      P7.1 gives them sleep. The PMIC's own power-off is `pmu`:
      `pwrkey_off=1 pwrkey_off_action=power-off pwrkey_off_hold_s=6`.
- [x] Red on once the frame is operable; green blinks 100/100 ms while the panel
      refreshes, then returns to `led on|off`. Confirmed by eye.
- [x] `buttons` shows `pwr_pressed`; `pmu` shows `vbat_mv` (4172 on USB, full),
      REG22/REG27 decoded.
- [ ] Buttons are not sampled while an AI turn blocks the loop (seconds), so a
      press entirely inside a turn is lost; the voice watcher also fires on the
      click sound. Fix with P7.1/P7.3: sample buttons inside long waits, or move
      turns to a worker thread.
- [x] A KEY turn was refused by the bridge without «Рамка». Fixed 2026-11-02:
      the frame sends `X-Trigger: key|voice|console` with `/stt`; the bridge asks
      for the wake word only from `voice` (or an older frame with no header), and
      a key/console turn opens the same follow-up window as saying the name.
      Seen: KEY click, «Сколько будет 2 плюс 2?» -> `/stt` 200, `/llm`, `/tts`, answer spoken.

### P7.3 — Modes with spoken prompts
- [ ] `mode=basic|network|ai` in `CONFIG.TXT`; each mode is a profile:
      basic = slideshow + sleep, radio off; network = portal + OpenDisplay, awake
      while connected, else wake every 30 s like the vendor; ai = bridge assistant.
- [ ] Mode menu: KEY long enters, KEY click cycles, KEY long confirms, with a
      prompt per step and a panel screen showing the choice.
- [ ] Prompts: short Russian clips synthesized once by the bridge's TTS and cached
      as µ-law files in `EASELTEST/VOICE/`; a beep when the file is missing.

### P7.4 — Provisioning without a computer
- [ ] SoftAP `PhotoPainter-XXXX` when there are no credentials, the join fails
      at boot, or the menu asks for it; panel shows SSID, password and URL
      (a QR code is a nice-to-have).
- [ ] Setup page on the AP: scan list, SSID/password form, written to
      `CONFIG.TXT` through `Settings::parse`, then join and drop the AP.
- [ ] Same `X-Easellet`/`Host` rules; the AP's own address must count as local.
- [ ] Size check: re-measure the image after re-enabling SoftAP.

### P7.5 — Battery and sensors in the open  *(done 2026-11-02, checked on the frame)*
- [x] Every 30 s: battery (`power::Battery` from REG00/01/34/35/A4, read-only),
      SHTC3, and the S3's own temperature sensor. `GET /sensors` JSON; the
      portal header shows battery (% / V / charging / USB) and room climate.
- [x] OpenDisplay: `msd::Telemetry` in the crate encodes the v1 MSD (battery
      10 mV, chip temp, reboot flag, counter, SHTC3 in the SHT40 3-byte block at
      slot 7); `ReadMsd` and the mDNS `msd` TXT key (28 hex digits) carry it.
      Config now says battery powered, `deep_sleep_time_seconds` =
      `power.sleep_secs`, and declares the I2C bus, the SHTC3 (as SHT40, the
      format it publishes) and the AXP2101. `check_msd.py` decodes the encoder's
      output with py-opendisplay in the gate.
- [x] Seen: `/sensors` battery 4171 mV 100 % on USB, 27.73 °C 32.37 %, chip 36.4
      °C; py-opendisplay over TCP: power_mode 1, sensors SHT40@7 + AXP2101,
      MSD battery 4170 mV, chip 36 °C, reboot flag, ambient 27.7 °C 32.3 %;
      avahi TXT `msd=0000000000000043950a0098a113`.
- [x] Low battery (`Battery::low`: on battery, <= 5 % or < 3.4 V): paint
      «Батарея разряжена» once per discharge (RTC flag), sleep on buttons only;
      a picture wake on an empty cell does not paint. Not exercised on hardware:
      needs a drained cell without USB.
- [x] SHTC3 temperature and humidity are available as assistant readings (P8).

### P7.6 — Pictures
- [ ] Portrait 480x800 BMP on the card: rotate while decoding.
- [ ] Portal: a portrait/landscape choice for uploads (`el6.mjs` rotates before
      dithering), so portrait photos are not cropped to a strip.
- [x] The assistant can request a picture by number (`src/actions.rs`).
- [ ] Add picture-number selection and a picture count to the console and portal;
      check whether a spoken picture-count command is needed.
- [ ] Ratings: `EASELTEST/RATINGS.TXT` (name=1..5); the slideshow weights by rating;
      reset by voice or console.
- [ ] P2b `display url` for `.el6`/BMP from the bridge (needed by P7.8).

### P7.7 — Status screens
- [ ] Boot, network-mode, AP-setup and AI-mode screens: the vendor's BMPs when
      present on the card (`01_sys_init_img`, `02_sys_ap_img`, `04_sys_ai_img`),
      our text screen otherwise.

### P7.8 — AI parity through the bridge
- [x] The bridge can send a fixed-list `X-Action`; the frame validates and
      executes supported actions after speaking (`src/actions.rs`).
- [x] Next, previous, show N, slideshow on/off and interval actions are
      implemented. Temperature, humidity and battery readings are supported.
- [ ] Add picture count and ratings/reset actions if vendor parity requires them.
- [x] The weather plugin fetches forecasts and the bridge renders and delivers
      cards; this does not depend on `display url`.
- [x] The `ai-image` plugin uses an OpenAI-compatible image endpoint and the
      bridge renders its result. A real image-service run remains open in P8.
- [ ] Decide whether generated images should also be saved to the card slideshow.
- [ ] Spoken replies say "queued" for anything that paints, as the vendor learned.

### P7.9 — OTA
- [x] Part 1 (2026-11-02): firmware update from the portal. `partitions.csv`
      (two 3 MB slots, no factory), `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`,
      `src/ota.rs` (`info`, `install` streaming 4 KB at a time into the inactive
      slot, `mark_running_valid`), `GET /ota` + `POST /ota` in `src/web.rs` with a
      180 s allowance and an early `0xE9` check, `ota` console command, portal
      «Firmware» tab (slot, state, next slot, size, armed twice, waits for the
      frame to come back), mock API and harness line, README flashing section.
      Host tests: `firmware` (image magic, refusal wording, HTTP statuses),
      console `ota`, the page/server route agreement and the single-`post()`
      rule; the portal harness gained «Прошивка» (slot read from the frame, a
      chosen file enabling the button, the slot's own size refusing 4 MB).
      `scripts/check.sh` reads the ceiling from `partitions.csv`, refuses fewer
      than two app slots, and generates `target/partition-table.bin` with IDF's
      `gen_esp32part.py` (`ota_0` at `0x20000`, `ota_1` at `0x320000`, 3 MB each).
      Release image 1,169,264 bytes, headroom 1,976,464 in a slot; bootloader
      rebuilt with `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`.
- [x] The OTA partition table was flashed and portal uploads to both slots were
      recorded; `ota_0` reported `state=valid` after reboot
      (`research/device/flash-ready-label.log`).
- [ ] With approval for any required flash or reset, verify rollback after a
      broken image or reset before the new image is marked valid.
- [x] The bridge Frames tab offers indexed firmware releases and installs through
      the frame portal. Hardware acceptance remains separate from implementation.

### P7.10 — OpenDisplay server on the bridge *(done 2026-11-02, checked on the LAN)*
- [x] One selected frame is exposed through the bridge (`bridge/src/opendisplay.rs`).
      The bridge binds `opendisplay_addr` (default `0.0.0.0:2446`), advertises
      `_opendisplay._tcp`, and terminates the protocol instead of proxying TCP:
      `ConfigRead`, `ReadMsd`, `FirmwareVersion`, `DirectWrite`, `Reboot`, and
      `DeepSleep` are forwarded to the frame. A config write changes only the
      bridge's virtual names. The image is accepted, then the bridge waits for the
      frame's real `0x73`/`0x74` and sends that result to its client.
- [x] The Frames tab has an **OpenDisplay server** section: enable, frame selector,
      discovery name and Save. The settings are `opendisplay_enabled`,
      `opendisplay_frame`, `opendisplay_name`, `opendisplay_addr`; off by default.
      `/api/opendisplay` reports the saved choice and the listener state.
- [x] The bridge filters its own mDNS answer from frame discovery; without the
      filter it listed itself as a second frame and could loop through itself.
- [x] `opendisplay.mdns=0` on the frame stops its advertisement while its TCP
      listener remains. This makes the bridge the only endpoint to discover. Not
      flashed or checked on hardware yet.
- [x] Host tests: 77 bridge tests (4 new device/config/name tests, 2 new UI route
      tests), clippy clean, `scripts/check.sh` green. Seen on the LAN:
      `avahi-browse -rt _opendisplay._tcp` shows `Easellet` at `192.168.50.93:2446`;
      `opendisplay info --host 192.168.50.93` reads the 800x480 config and firmware
      `easellet-bridge`; `opendisplay upload --host 192.168.50.93` finished with
      `Done` after the frame's refresh. Only one frame is now listed in the UI.
- [ ] Home Assistant discovery still sees both endpoints unless the frame's
      `opendisplay.mdns=0` is flashed and set.

**Suggested order:** P7.0 -> P7.2 (lights, BOOT next) -> P7.1 -> P7.5 -> P7.4 ->
P7.6 -> P7.3 -> P7.8 -> P7.7 -> P7.9. Sleep before modes, because each mode is
mostly a sleep policy; buttons first, because sleep needs wake sources to test.

## P8 — Bridge web UI and plugins

Design (draft, awaiting review): `research/bridge-ui-design.md`. The bridge becomes
the control point for all frames: a web UI (frames, plugins, assistant, settings,
log) with the frame portal's protection, and plugins (subprocesses, JSON in/out)
that produce cards or pictures the bridge renders per frame and delivers by push
(OpenDisplay/portal) or pull (frame fetches on its timed wake).

- [x] Review the design's section 9 decisions (all accepted 2026-11-02).
- [x] B1 (2026-11-02): 4 worker threads (frame API under one lock, UI free of it:
      `/api/status` answered in 0.5 ms during a 2.6 s Whisper run);
      `bridge.toml` (file < EASEL_* < flags; seeded on first run; 0600; live keys
      applied per request); UI shell with password, `X-Easellet`, `Host`
      checks; Settings tab (pinned keys read-only, restart badges, write-only
      secrets); Assistant tab (turns assembled per peer from /stt, /llm, /tts,
      rejections with reasons). `serve-local.sh` now runs from the file.
- [x] B2 (2026-11-02): `mdns::browse_query`/`parse_response` and `client`
      (config read, MSD, zlib DirectWrite) in `opendisplay-proto`, round-trip
      tested against the server; bridge `frames.rs`: legacy mDNS browse +
      `[[frame]]` manual entries, status via portal `/sensors`, panel via
      OpenDisplay config, refreshed every minute; portal proxy (gallery, image,
      show, upload, delete) and OpenDisplay display. Seen on the frame: found
      ODCAED44 with panel 800x480/scheme 4/rot 2 and battery; display 192000 B
      -> 2455 zlib B in 121 ms; upload -> UP0003, delete OK.
  - [ ] Not yet looked at: the Frames tab screenshot (taken, not reviewed);
        manual add/remove exercised only in unit tests.
- [x] B3 (2026-11-02): `bridge/src/render.rs`: Spectra 6 dither + packing,
      byte-identical to `convert.py` (test runs the Python on gradients,
      palette blocks and noise, dithered and not); `notice` and `list` cards
      in exact palette colours (DejaVu Sans via `ab_glyph`, coverage
      thresholded, no anti-aliasing), wrapping and ellipsis; PNG previews.
      `POST /api/render/preview`, `POST /api/frames/<id>/card`; "Сообщение на
      рамку" box in the Frames tab. Seen: a notice card sent to ODCAED44
      (4222 zlib bytes, 389 ms) and refreshed.
  - [ ] Other panel sizes and colour schemes (cards are 800x480 Spectra only).
  - [x] The `forecast` card layout is implemented.
  - [ ] Other proposed layouts (`status`, `metrics`, `photo`) remain open.
- [x] B4 plugin runner and scheduler are implemented; remaining acceptance and
      persistence work is listed below.
  - [x] Part 1 (2026-11-02): `bridge/src/plugins.rs`: manifests (strict,
        name = directory, program inside it), parameter validation against the
        manifest, runner (cleared environment, scratch dir, timeout kills,
        stdout/stderr caps), outputs card/text/image (scratch-only path or
        base64)/none, per-plugin-and-frame state. `render::photo_frame`
        (cover/contain). `/api/plugins`, `/api/plugins/<name>/preview|show`;
        Plugins tab with generated forms; `easellet-bridge plugin list|run
        --preview`. SDK `plugins/sdk/easellet_plugin.py`, examples `hello`
        (Python) and `system` (sh), author guide `plugins/README.md`. Seen:
        `hello` previewed and shown on ODCAED44 (2651 zlib B, 270 ms).
  - [x] Part 2 (2026-11-02): `bridge/src/jobs.rs`: jobs in `jobs.toml`
        (0600, secrets write-only), interval scheduler (every 30 s), outbox per
        frame and job (priority, then newest), FNV content hash as ETag,
        push to awake frames, run log; `GET /frames/<id>/next` (bearer token,
        200/304/204). UI: "Сохранить как задание", jobs list (run now, delete),
        journal. Firmware: `bridge.url` (opt-in), timer wake joins, pulls with
        the RTC-kept ETag, paints on 200, sleeps on 304, falls back to the card
        slideshow on 204/errors. Seen: identical run -> "unchanged"; frame wakes
        at +60 s: 200 (painted, 36 s awake), then 304, 304 (5 s awake each,
        no refresh).
  - [x] Screens (2026-11-06): `bridge/src/compose.rs` composes several jobs'
        latest results on one panel (`single` `stack` `columns` `hero` `grid`),
        with rectangle-aware compact tiles, picture cells dithered in isolation,
        a stale marker, and one refresh per screen change instead of one per job.
        `[[screen]]` in `jobs.toml`, `/api/screens`, `/frames/<id>/screen.png`
        and a Screen section on each frame card. A spoken answer holds the frame
        for ten minutes. Seen: system and CI side by side, hero and grid, the
        preview and save flows in English and Russian, and a narrow viewport.
  - [ ] Fixed clock times for jobs (needs local time on the bridge).
  - [ ] Outbox, run log and plugin state are in memory. Screen definitions
        persist in `jobs.toml`, but queued pictures and composed content do not
        survive a bridge restart. Persisting composed content could also keep
        its ETag stable.
- [x] B5 (2026-11-02): shipped plugins `weather` (Open-Meteo, geocoding
      cached in state; new `forecast` layout with drawn icons), `news` (RSS/Atom,
      "nothing new" when the headlines repeat), `ci` (GitHub Actions, optional
      secret token, "nothing new" when unchanged), `note`, `photo-folder`
      (sequential/random, copies into scratch), `ai-image` (OpenAI-compatible
      `/v1/images/generations`, tested against a local mock only). One-line
      fields are now cut by characters (a long branch name no longer vanished
      behind a short word). Seen: weather shown on ODCAED44; news dedup.
  - [x] `ci` shows each run's PR number (2026-11-02): `pull_requests` when
        GitHub fills it (same-repo branches), `#N` from the title (merge queue),
        else `pulls?head=<fork owner>:<branch>` matched on the run's commit, at
        most 4 lookups per run (60 requests/h without a token), cached per
        commit in the plugin's state.
  - [x] `ci` reworked (2026-11-02, user request): the latest open PRs (sorted
        by update, optional base branch), each with counts of its head commit's
        check runs and commit statuses (failed / running / passed / other),
        paged past 100 runs; drawn as right-aligned tallies (new `counts` on
        list items). `api` parameter for GitHub Enterprise. Checked against a
        mock GitHub with freeipa-shaped data, then live on freeipa/freeipa with
        a token (6 PRs, ~13 requests, 7 s; counts match the raw API data).
        GitHub's error message now reaches the preview (a 403 was freeipa's
        366-day token lifetime cap); the card warns 14 days before the token
        expires (`github-authentication-token-expiration`).
  - [x] `ci` reads a Forgejo (or Gitea) repository too (2026-11-04): new `forge`
        choice (`github` default, `forgejo`), `api` names the instance. Forgejo
        has no check runs, so the tally is its commit statuses alone (one row
        per context; `status`, not GitHub's `state`; warning and skipped are
        "other"); the pulls list takes `sort=recentupdate&limit=` and honours
        `base`; the token goes as `token` and an instance has no 60/hour limit,
        so every PR on the card is checked and no expiry line is drawn. Seen
        live on codeberg.org:forgejo/forgejo (5 PRs, 3.5 s, one red, one blue),
        GitHub with a freeipa token and anonymously, the `none` dedup, and the
        refusals (Forgejo without `api`, unknown forge, 404 on both forges).
  - [ ] `ai-image` against a real image service.
  - [x] Found while testing: a KEY press during a 5 s picture wake was lost,
        so the frame stayed in its timed loop; a KEY/PWR press there now cancels
        the sleep and wakes the frame fully (flashed; not yet re-tested by hand).
- [x] B6 (2026-11-02): plugins as the assistant's tools. `bridge/src/intents.rs`
      matches a question against manifests' `[assistant] intent` + `phrases`
      by stems (host-tested); `/llm` hands a match to the plugin for the asking
      frame (by peer address; a saved job's params, else defaults; optional
      `text_param` gets the question), answers «Показываю: …» with `X-Panel:
      keep`, and pushes the picture once the frame's turn ends (retries 1 min).
      Firmware honours `X-Panel: keep` (speaks, does not paint the transcript).
      Assistant tab: ask by text, answer spoken in the browser (WAV), recorded
      in the history as `bridge-ui`. Seen: «Рамка, покажи погоду» -> /llm 11 ms
      via the tool, weather pushed 3.7 s later and shown.
  - [x] Weather by voice for a named place (2026-11-02): the plugin reads the
        place from the question (hidden `question` param via `text_param`),
        undoes Russian case endings by geocoder lookups with exact-name match,
        says `say` aloud (the place shown, or that it found none). Recognition:
        language hint as a detection prior and `stt_prompt` (vendored rwhisper,
        `Steering`). Firmware: a short burst of voice (< 1 s, a name) waits
        1.5 s of quiet instead of 0.7 s, so «Рамка… покажи погоду» is one
        capture; before, the question's start was lost while the name alone
        was recognised with the microphone closed. Espoo, Tampere and (after
        the detector fix was flashed) «Рамка … покажи погоду в Вашингтоне» seen
        on hardware.
  - [ ] Microphone in the browser: needs HTTPS (or localhost) for getUserMedia.
  - [x] Frame commands from P7.8 (2026-11-02): `bridge/src/commands.rs`
        matches next / previous / show N (digits, number words, ordinals) /
        slideshow on|off / interval, and room temperature, humidity, battery;
        `/llm` answers with `X-Action` (+ `X-Panel: keep`). Firmware
        `src/actions.rs` parses only that list and fills `{temp}` `{humidity}`
        `{battery}` from its sensors with agreeing nouns; `run_action` uses the
        gallery (`aim`, `hurry`, `previous_position`, `set_interval`). Host-
        tested both sides. Seen on hardware: temperature spoken, next, third
        picture, slideshow off.
  - [ ] Ratings (P7.6) have no storage yet, so no voice command for them.

## Out of scope for now

- BLE diagnostics (original plan stage 6) — untouched; Wi-Fi covers remote control.
- On-device wake-word detection (ESP-SR/WakeNet): the bridge's wake stem does
  that job (P7 decision 4).
- The vendor's clouds (Xiaozhi, Volcengine): their features come through the
  bridge instead (P7.8).
- Echo cancellation; the ES7210 has the AEC reference wired (MIC3), but nothing
  uses it.
- Charger configuration: the charger path stays at factory defaults; P7.5 only
  reads battery state.

## Settled decisions (do not re-litigate without a reason)

- Settings live in `EASELLET/CONFIG.TXT`, not NVS. The Wi-Fi passphrase is
  stored in plain text on the card; anyone who can read it can recover it.
- `wifi.auto=1` keeps the link up after boot rather than syncing time and dropping
  it, because later features want the link.
- Persistence happens in the same serial session as the join, because closing the
  port resets the chip and the driver holds its network configuration in RAM.
- `ai.lang` defaults to `ru-RU`; the assistant can also use the bridge's other
  supported languages.
- Connection details come from `CONFIG.TXT` (`ai.url`, `ai.key`, `ai.lang`,
  `ai.voice`, `ai.model`), never from a compiled-in header, so a card can be moved
  between builds. There is no cloud key anywhere: `ai.key` is the bridge's own
  optional bearer token, and the frame never holds a provider secret.
- µ-law on the wire in both directions, as the sketch does: half the bytes for the
  same bandwidth, and the endpoints accept it directly.
- No distance sensor on this board, so the trigger is the KEY button and the
  microphone is the only input. The sketch's VL53L0X threshold has no equivalent
  here.
- Plain HTTP only, no TLS in the firmware: `CONFIG_ESP_HTTP_CLIENT_ENABLE_HTTPS=n`
  and no certificate bundle is linked (checked by searching the image for a CA name —
  zero hits in both profiles). A `https://` URL therefore fails at `Base::parse`, which
  is the desired failure. **If TLS is ever wanted, the user's route is a CA bundle on
  the SD card**, not the compiled-in `esp_crt_bundle`: the card already carries
  `CONFIG.TXT`, so `EASELTEST/CA.PEM` would be one more file and no bigger firmware. That
  work re-enables the HTTP client's HTTPS path, so it is a decision to take
  deliberately, not a side effect.

## Portal (browser upload, gallery, settings)

- [x] `GET /gallery`, `GET /image?path=`, `POST /show?path=` served from the main loop
- [x] `unpack`/`drawFrame` in `tools/el6.mjs`, round-trip pinned by the gate
- [x] uploads land in the slideshow directory as `UPnnnn.EL6` (8.3, created on first use)
- [x] `GET`/`POST /settings`: form -> `key=value` text -> `Settings::parse` -> saved
- [x] live application of volume, gain, gate, `ai.idle`, slideshow interval
- [x] `web.password` (HTTP Basic, off by default); `-` in the form removes it
- [x] `web password <pw>` / `web password off` on the console, so a forgotten
      lock can be cleared without the page (verified on the board: 401 -> 200)
- [x] a password saved from either side rebinds the listener at once
- [x] drop from another tab: a cross-document drag carries a URL and no file,
      so `files[0]` was undefined and the box highlighted in silence; the page
      now resolves file / uri-list / text/html / clipboard and explains a CORS
      refusal. Pinned by `tools/check-portal-js.mjs` in the gate
- [x] removal: `POST /delete?path=` -> `image::removable` (picture + inside
      `EASELTEST`, tests for `..`, backslash, absolute, `CONFIG.TXT`, vendor dirs);
      the page asks twice in place (armed button, 5 s) instead of a modal, and
      offers no button at all for a picture the firmware would refuse
- [x] a large collection: names only, twelve per page, search by name, 48-frame
      decoded cache, frames fetched one at a time; `storage::MAX_IMAGES` raised
      to 512 and the listing cap now counts pictures instead of directory entries
- [x] admin behind its own tab («Настройки»), photos and gallery in front; the
      tab is the URL hash, so links and the back button work
- [x] `tools/portal-mock.py` + `tools/portal-harness.html`: the page runs against
      a fake API with 60 generated pictures, and the harness drives the real page
      (`ok на странице 12 карточек`, `ok после удаления`, `ok удаление вне EASELTEST: 403`)
- [x] a refused delete re-reads `/gallery` before believing itself: a 403 can
      simply mean the picture was already removed from another tab or the
      console, and a card that no longer exists must not stay on the page
      (seen live: stale `UP0003` card went away, chip fell to 2)
- [ ] the firmware's refusal text is English (`No such file or directory`) and
      the page shows it verbatim; map the two or three common reasons to a
      Russian sentence in `remove()`
- [x] the answer language is a list of the four the bridge hears
      (`EASEL_STT_LANGS=ru,en,fi,fr`), not a text box; a tag outside the list is
      shown as `(не из списка)` rather than blanking a field that means "keep"
- [ ] a bulk "select all pages" — today "выбрать страницу" is one page at a time
- [ ] remember `EASEL_WAKE` and the other bridge flags in `bridge/scripts/serve-local.sh`
- [ ] delete the same picture from the console (`sd rm`), for when the page is
      unreachable; the policy function already exists

## Scratch space

Everything the host side writes while working on the frame lives under
`~/tmp/frame/` — `stt-dumps/` (the µ-law uploads), `stt-wav/` (the same audio
decoded for `build.sh transcribe`), `logs/` (bridge and experiment logs), and
images on their way to the SD card. `/tmp` is cleared on reboot and a capture that
reproduces a misheard word is evidence worth keeping until the next comparison.

## Recognition: `small` stays, measured

`whisper-medium` was fetched and compared on the same five captures (a second
bridge on port 8097, stub backends, so only the recogniser differed). It is not
better where it matters and costs 3.4× the time:

| capture | `small` | `medium` |
| --- | --- | --- |
| «Сколько будет 2 плюс 2?» | «Рамка. Сколько будет 2 плюс 2?» | «Ранка. Сколько будет 2 плюс 2?» |
| «…торт Наполеон?» | «…торт на поливун?» | «…торт на поле луны.» |
| bare name | «Ранка.» | «Ранка.» |
| room conversation | «О, мой стикер.» | «Мой стикер.» |
| 3.4 s question, end to end | 3.2 s | 10.6 s |

Both models hear the name as «Ранка» and neither knows «Наполеон», which says the
limit is the audio — an 8-bit µ-law capture of a child at a metre from a
two-microphone board — not the model size. The answer is therefore not a bigger
model: the bridge tolerates the misheard name (`off_by_one`), and the frame asks
again less often if the speaker is closer.
