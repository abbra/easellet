#!/usr/bin/env bash
# Build one firmware image with an explicit version, for a person and for CI.
#
# The version a person edits lives in Cargo.toml. This script builds an image
# that says something else only when it is asked to, which is how a `dev` build
# can say `0.1.0-dev+g1a2b3c4` without touching a checked-in file: the version
# goes into a generated sdkconfig defaults file that is applied *after* the
# checked-in one.
#
#   scripts/build-firmware.sh 0.1.0
#   scripts/build-firmware.sh 0.1.0-dev+g1a2b3c4
#
# Prints one `name value` line per fact and nothing else, so a workflow can read
# what it built. The image lands in
# target/xtensa-esp32s3-espidf/release/easellet.bin.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh

version="${1-}"
if [[ -z "$version" ]]; then
    echo "usage: scripts/build-firmware.sh <version>" >&2
    exit 2
fi

mkdir -p target
printf 'CONFIG_APP_PROJECT_VER="%s"\n' "$version" > target/sdkconfig.version.defaults
export ESP_IDF_SDKCONFIG_DEFAULTS="sdkconfig.defaults;target/sdkconfig.version.defaults"

# ESP-IDF merges defaults into a sdkconfig that does not exist yet and warns
# instead of applying them when one does, so a stale copy silently keeps the old
# version. Dropping them makes every invocation build the version it was asked
# for. `scripts/check.sh` deliberately does not do this: it builds whatever the
# checked-in defaults say.
find target -path '*/esp-idf-sys-*/out/sdkconfig' -delete

cargo build --locked --profile release

# A silent override that does not apply is worse than none, so the generated
# sdkconfig is read back rather than trusted.
sdkconfig="$(ls -t target/*/release/build/esp-idf-sys-*/out/sdkconfig | head -1)"
for want in 'CONFIG_APP_PROJECT_VER_FROM_CONFIG=y' "CONFIG_APP_PROJECT_VER=\"$version\""; do
    if ! grep -qxF "$want" "$sdkconfig"; then
        echo "ERR $sdkconfig does not hold $want" >&2
        exit 1
    fi
done

image="target/xtensa-esp32s3-espidf/release/easellet.bin"
.venv-tools/bin/python -m esptool --chip esp32s3 elf2image \
    --flash-mode dio --flash-freq 40m --flash-size 16MB \
    --output "$image" target/xtensa-esp32s3-espidf/release/easellet

read -r slot_bytes slots <<< "$(python3 tools/partitions.py)"
if [[ "$slots" -lt 2 ]]; then
    echo "ERR partitions.csv has $slots app slot(s); OTA needs two" >&2
    exit 1
fi
bytes="$(stat -c %s "$image")"
headroom=$((slot_bytes - bytes))
if [[ "$headroom" -lt 0 ]]; then
    echo "ERR image $bytes bytes does not fit the $slot_bytes-byte slot" >&2
    exit 1
fi

echo "version $version"
echo "sdkconfig $sdkconfig"
echo "bytes $bytes"
echo "headroom $headroom"
echo "sha256 $(sha256sum "$image" | cut -d' ' -f1)"
echo "image $image"
