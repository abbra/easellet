#!/usr/bin/env bash
# Restart the bridge with this house's settings, replacing whatever holds the port.
#
# Doing this by hand has already gone wrong twice: `kill` then `start` leaves the old
# process alive for a moment, the new one dies with AddrInUse, and the frame is then
# being served by the previous build while the log of the new one looks healthy. The
# port is therefore waited for, and /health is checked before this script exits.
#
# Settings live in bridge.toml (EASEL_BRIDGE_CONFIG, default
# ~/.config/easellet-bridge/bridge.toml) and are edited from the web UI. The
# first run seeds that file from the defaults below; after that only what is
# explicitly set in the environment is passed on (and pinned for that run):
# EASEL_ADDR, EASEL_TOKEN, EASEL_STT, EASEL_LLM, EASEL_TTS, EASEL_WAKE, EASEL_LLM_URL. EASEL_BRIDGE_DUMP
# and EASEL_BRIDGE_LOG are debugging aids, not settings.
set -euo pipefail
cd "$(dirname "$0")/../.."

config="${EASEL_BRIDGE_CONFIG:-$HOME/.config/easellet-bridge/bridge.toml}"
# An installation from before the rename keeps its settings and token.
if [ -z "${EASEL_BRIDGE_CONFIG+x}" ] && [ ! -f "$config" ] &&
    [ -f "$HOME/.config/photoframe-bridge/bridge.toml" ]; then
    config="$HOME/.config/photoframe-bridge/bridge.toml"
fi
log="${EASEL_BRIDGE_LOG:-$HOME/tmp/frame/logs/bridge.log}"
args=(--config "$config")
if [ -f "$config" ]; then
    # Only explicit overrides; everything else comes from the file.
    [ -n "${EASEL_ADDR+x}" ] && args+=(--addr "$EASEL_ADDR")
    [ -n "${EASEL_TOKEN+x}" ] && args+=(--token "$EASEL_TOKEN")
    [ -n "${EASEL_STT+x}" ] && args+=(--stt "$EASEL_STT")
    [ -n "${EASEL_LLM+x}" ] && args+=(--llm "$EASEL_LLM")
    [ -n "${EASEL_TTS+x}" ] && args+=(--tts "$EASEL_TTS")
    addr="${EASEL_ADDR:-$(sed -n 's/^addr *= *"\(.*\)"/\1/p' "$config")}"
else
    # First run: these become the file's contents.
    addr="${EASEL_ADDR:-192.168.50.93:8095}"
    args+=(--addr "$addr" --token "${EASEL_TOKEN:-easellet-lan-2026}"
        --stt "${EASEL_STT:-whisper:small}"
        --llm "${EASEL_LLM:-cmd:bridge/scripts/llm-local.py}"
        --tts "${EASEL_TTS:-cmd:bridge/scripts/tts-piper.sh}")
    export EASEL_WAKE="${EASEL_WAKE:-рамк,фоторам,frame}"
    export EASEL_LLM_URL="${EASEL_LLM_URL:-http://127.0.0.1:8098/v1/chat/completions}"
fi
host="${addr%:*}"
port="${addr##*:}"
# Dumps, wavs and logs belong under ~/tmp/frame: they are evidence for the next
# question about a misheard word, and /tmp is cleared by reboots.
mkdir -p "$HOME/tmp/frame/logs"
export EASEL_BRIDGE_DUMP="${EASEL_BRIDGE_DUMP-$HOME/tmp/frame/stt-dumps}"

old=$(ss -lptnH "sport = :$port" | grep -o 'pid=[0-9]*' | cut -d= -f2 | head -1)
if [ -n "$old" ]; then
    echo "stopping pid $old"
    kill "$old"
    for _ in $(seq 1 40); do
        ss -lptnH "sport = :$port" | grep -q . || break
        sleep 0.25
    done
    if ss -lptnH "sport = :$port" | grep -q .; then
        echo "ERR port $port still held after 10 s; refusing to start" >&2
        exit 1
    fi
fi

[ -f "$log" ] && mv "$log" "$log.prev"
setsid nohup ./bridge/target/release/easellet-bridge "${args[@]}" \
    > "$log" 2>&1 < /dev/null &

for _ in $(seq 1 60); do
    if curl -sf -m 2 "http://$host:$port/health" > /dev/null; then
        pid=$(ss -lptnH "sport = :$port" | grep -o 'pid=[0-9]*' | cut -d= -f2 | head -1)
        echo "OK bridge pid=$pid http://$host:$port log=$log"
        grep -E "wake:|listening on|settings:" "$log" || true
        exit 0
    fi
    sleep 1
done

echo "ERR bridge did not answer /health in 60 s; log follows" >&2
tail -20 "$log" >&2
exit 1
