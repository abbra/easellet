#!/usr/bin/env bash
# Restart the bridge with this house's settings, replacing whatever holds the port.
#
# Doing this by hand has already gone wrong twice: `kill` then `start` leaves the old
# process alive for a moment, the new one dies with AddrInUse, and the frame is then
# being served by the previous build while the log of the new one looks healthy. The
# port is therefore waited for, and /health is checked before this script exits.
#
# Settings live in bridge.toml (PF_BRIDGE_CONFIG, default
# ~/.config/photoframe-bridge/bridge.toml) and are edited from the web UI. The
# first run seeds that file from the defaults below; after that only what is
# explicitly set in the environment is passed on (and pinned for that run):
# PF_ADDR, PF_TOKEN, PF_STT, PF_LLM, PF_TTS, PF_WAKE, PF_LLM_URL. PF_BRIDGE_DUMP
# and PF_BRIDGE_LOG are debugging aids, not settings.
set -euo pipefail
cd "$(dirname "$0")/../.."

config="${PF_BRIDGE_CONFIG:-$HOME/.config/photoframe-bridge/bridge.toml}"
log="${PF_BRIDGE_LOG:-$HOME/tmp/frame/logs/bridge.log}"
args=(--config "$config")
if [ -f "$config" ]; then
    # Only explicit overrides; everything else comes from the file.
    [ -n "${PF_ADDR+x}" ] && args+=(--addr "$PF_ADDR")
    [ -n "${PF_TOKEN+x}" ] && args+=(--token "$PF_TOKEN")
    [ -n "${PF_STT+x}" ] && args+=(--stt "$PF_STT")
    [ -n "${PF_LLM+x}" ] && args+=(--llm "$PF_LLM")
    [ -n "${PF_TTS+x}" ] && args+=(--tts "$PF_TTS")
    addr="${PF_ADDR:-$(sed -n 's/^addr *= *"\(.*\)"/\1/p' "$config")}"
else
    # First run: these become the file's contents.
    addr="${PF_ADDR:-192.168.50.93:8095}"
    args+=(--addr "$addr" --token "${PF_TOKEN:-photoframe-lan-2026}"
        --stt "${PF_STT:-whisper:small}"
        --llm "${PF_LLM:-cmd:bridge/scripts/llm-local.py}"
        --tts "${PF_TTS:-cmd:bridge/scripts/tts-piper.sh}")
    export PF_WAKE="${PF_WAKE:-рамк,фоторам,frame}"
    export PF_LLM_URL="${PF_LLM_URL:-http://127.0.0.1:8098/v1/chat/completions}"
fi
host="${addr%:*}"
port="${addr##*:}"
# Dumps, wavs and logs belong under ~/tmp/frame: they are evidence for the next
# question about a misheard word, and /tmp is cleared by reboots.
mkdir -p "$HOME/tmp/frame/logs"
export PF_BRIDGE_DUMP="${PF_BRIDGE_DUMP-$HOME/tmp/frame/stt-dumps}"

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
setsid nohup ./bridge/target/release/photoframe-bridge "${args[@]}" \
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
