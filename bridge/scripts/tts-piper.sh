#!/bin/sh
# Photoframe bridge TTS backend: Piper in the language of the answer, converted to the
# exact format the firmware expects — raw signed 16-bit little-endian mono PCM at
# 16 kHz on stdout, text read from stdin.
#
# Usage with the bridge:
#   ./bridge/build.sh serve -- --stt whisper:small --tts cmd:bridge/scripts/tts-piper.sh
#
# piper is a neural synthesiser (VITS), so it sounds like a person rather than like
# espeak-ng's rule-based formant voice — which is what the frame was actually reporting
# as "the speech quality is bad": the bytes reaching the speaker were clean (peak 82%
# of full scale, nothing clipped, played back in 7125 ms for 6.98 s of audio), and the
# same DAC reproduced a natural recording correctly.
#
# Setup (one voice per language, ~60 MB each, no network at speak time):
#   uv tool install piper-tts
#   mkdir -p ~/.local/share/piper-voices && cd ~/.local/share/piper-voices
#   for pair in en/en_US/amy/medium/en_US-amy-medium \
#               fi/fi_FI/harri/medium/fi_FI-harri-medium \
#               fr/fr_FR/siwis/medium/fr_FR-siwis-medium \
#               ru/ru_RU/irina/medium/ru_RU-irina-medium; do
#       base="https://huggingface.co/rhasspy/piper-voices/resolve/main/$pair"
#       name=$(basename "$pair")
#       curl -sSLO "$base.onnx" -O "$base.onnx.json"
#   done
# The detector below uses the distro package `python3-langdetect` (or pip langdetect).
#
# Language selection, in order:
#   1. $VOICES names an installed model file (an explicit `ai voice` wins).
#   2. a confident classification of the answer text by detect-lang.py, which is
#      what keeps a translation answer in the translated language.
#   3. $LANGS, the two-letter code the bridge's recogniser detected (ru/en/fi/fr).
#   4. Russian is the frame's default.
#
# The bridge passes the frame's `ai.voice` hint as $VOICES and the recognised language
# as $LANGS. A stored voice name that matches no file — the default is Google's
# `ru-RU-Standard-B` — falls back to the language-appropriate model rather than
# failing, because a stored setting from before this voice existed should not silence
# the frame.
#
# Overrides: PF_PIPER_MODEL (forces one model), PF_PIPER_LENGTH (piper
# `--length-scale`, default 1.0 — larger is slower), PF_PIPER_VOLUME (`--volume`,
# default 0.85).
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
piper="${PF_PIPER:-$HOME/.local/bin/piper}"
voices="${PF_PIPER_VOICES:-$HOME/.local/share/piper-voices}"
requested="${VOICES:-}"

text=$(cat)
if [ -z "$text" ]; then
    echo "tts-piper: empty input" >&2
    exit 1
fi

hint="${LANGS:-}"
hint="${hint%%-*}"
detected=$(printf '%s' "$text" | python3 "$here/detect-lang.py")
lang=""
case "$detected" in
    ru|en|fi|fr) lang="$detected" ;;
esac
if [ -z "$lang" ]; then
    case "$hint" in
        ru|en|fi|fr) lang="$hint" ;;
        *) lang=ru ;;
    esac
fi

model=""
if [ -n "$requested" ] && [ -f "$voices/$requested.onnx" ]; then
    model="$voices/$requested.onnx"
elif [ -n "$requested" ] && [ -f "$requested" ]; then
    model="$requested"
elif [ -n "${PF_PIPER_MODEL:-}" ]; then
    model="$PF_PIPER_MODEL"
else
    case "$lang" in
        en) model="$voices/en_US-amy-medium.onnx" ;;
        fi) model="$voices/fi_FI-harri-medium.onnx" ;;
        fr) model="$voices/fr_FR-siwis-medium.onnx" ;;
        *) model="$voices/ru_RU-irina-medium.onnx" ;;
    esac
fi

if [ ! -f "$model" ]; then
    echo "tts-piper: no model at $model (language $lang)" >&2
    exit 1
fi
echo "tts-piper: lang=$lang hint=${hint:-<none>} detected=$detected voice=${requested:-<default>} model=$model" >&2

# Volume stops short of full scale on purpose: piper peaks at 32767 (and clips a
# couple of samples) at 1.0, and the frame plays at 0 dB into a small speaker, where
# a full-scale signal is distortion rather than loudness. 0.85 measured 0% clipped.
#
# The model runs at 22050 Hz; the bridge speaks 16000 Hz, so ffmpeg resamples down.
# Piper writes 16-bit mono samples, which is the same sample format on both sides.
printf '%s' "$text" |
    "$piper" --model "$model" \
        --length-scale "${PF_PIPER_LENGTH:-1.0}" \
        --volume "${PF_PIPER_VOLUME:-0.85}" \
        --output-raw 2>/dev/null |
    ffmpeg -nostdin -loglevel error -f s16le -ar 22050 -ac 1 -i pipe:0 \
        -ar 16000 -ac 1 -f s16le pipe:1
