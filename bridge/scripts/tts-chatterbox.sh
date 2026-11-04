#!/bin/sh
# The bridge sends text on stdin and expects raw 16 kHz PCM on stdout.
set -eu
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
python="${EASEL_CHATTERBOX_PYTHON:-$HOME/.local/share/easellet-bridge/chatterbox/bin/python}"
exec "$python" "$here/tts-chatterbox.py"
