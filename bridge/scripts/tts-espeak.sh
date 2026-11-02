#!/bin/sh
# Photoframe bridge TTS backend: espeak-ng speaking Russian, converted to the exact
# format the firmware expects — raw signed 16-bit little-endian mono PCM at 16 kHz on
# stdout, text read from stdin.
#
# Usage with the bridge:
#   ./bridge/build.sh serve -- --stt whisper:small --tts cmd:bridge/scripts/tts-espeak.sh
#
# Overrides: PF_TTS_VOICE (espeak-ng voice, default ru), PF_TTS_SPEED (words per
# minute, default 140). espeak-ng sounds robotic, which is the point: it proves the
# whole path without downloading a neural voice, and a command backend can be
# swapped for one later without touching the firmware.
set -eu

voice="${PF_TTS_VOICE:-ru}"
speed="${PF_TTS_SPEED:-140}"

text=$(cat)
if [ -z "$text" ]; then
    echo "tts-espeak: empty input" >&2
    exit 1
fi

# espeak-ng writes a WAV; ffmpeg resamples to the bridge's fixed 16 kHz mono.
printf '%s' "$text" |
    espeak-ng -v "$voice" -s "$speed" --stdout |
    ffmpeg -nostdin -loglevel error -i pipe:0 -ar 16000 -ac 1 -f s16le pipe:1
