#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh
cargo fmt --check
# Console parser is pure std; test natively without trying to run Xtensa binaries.
for module in clock config diag image pcm bridge vad gallery web buttons power actions firmware; do
    rustc --edition 2024 --test "src/$module.rs" -o ".tools/$module-tests"
    ".tools/$module-tests"
done
# The console re-exports `config::IdleMode`, so its grammar and the settings
# file share one set of mode names. Built as one crate for the same reason as
# the panel harness below.
cat > .tools/console-harness.rs <<'HARNESS'
// The harness includes `config` for its one type, not its constants.
#![allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[path = "../src/gallery.rs"]
mod gallery;
#[path = "../src/console.rs"]
mod console;
HARNESS
rustc --edition 2024 --test .tools/console-harness.rs -o .tools/console-tests
.tools/console-tests
# The text blitter and the generated glyph table belong together: one holds the
# bits the other indexes, so they are built as one crate rather than alone.
cat > .tools/panel-harness.rs <<'HARNESS'
#[path = "../src/font.rs"]
mod font;
#[path = "../src/panel.rs"]
mod panel;
HARNESS
rustc --edition 2024 --test .tools/panel-harness.rs -o .tools/panel-tests
.tools/panel-tests
# The browser-side frame converter and the host one must agree byte for byte,
# or the same picture would look different depending on how it arrived. Both
# halves are pure, so this needs no browser - just Node and Pillow.
if command -v node >/dev/null && python3 -c "import PIL" 2>/dev/null; then
    python3 tools/check-el6-js.py >/dev/null || {
        echo "el6-js: converter disagreement (run tools/check-el6-js.py)"
        exit 1
    }
    echo "el6-js: byte-identical"
else
    echo "el6-js: SKIPPED (needs node and Pillow)"
fi
# The page's own drop resolution, run against the function in the page: a drag
# from another tab carries a URL and no file, which is exactly the case a person
# clicking the file picker never exercises.
if command -v node >/dev/null; then
    node tools/check-portal-js.mjs || exit 1
else
    echo "portal-js: SKIPPED (needs node)"
fi
cargo clippy --locked --all-targets --all-features -- -D warnings
echo "== bridge (host) =="
./bridge/build.sh clippy
./bridge/build.sh test
echo "== opendisplay-proto (host) =="
./opendisplay-proto/build.sh clippy
./opendisplay-proto/build.sh test

# Both profiles are built because they answer different questions: `dev`
# (opt-level "z", debug info) keeps symbols and panic locations readable, while
# `release` (opt-level "s") is much smaller. Only the release image is flashed:
# the dev image passed the 1500 KiB factory partition while the voice assistant
# was being added, and a build that cannot be flashed is still worth building.
# Cargo calls the development profile `dev` but puts its output in `debug`.
#
# The ceiling comes from `partitions.csv`, the table the frame is flashed with,
# not from sdkconfig: the app is built as a raw image and the table is written
# at 0x8000 by esptool, so a build that fits sdkconfig's single-app layout can
# still be too large for a real slot. Two equal app slots are also what OTA
# needs, so a table that lost one fails here instead of at the first upload.
read -r SLOT_BYTES SLOTS <<< "$(python3 - <<'PY'
import re

slots = []
for line in open("partitions.csv", encoding="utf-8"):
    fields = [field.strip() for field in line.split(",")]
    if line.lstrip().startswith("#") or len(fields) < 5 or fields[1] != "app":
        continue
    match = re.fullmatch(r"(0x[0-9a-fA-F]+|[0-9]+)([KkMm]?)", fields[4])
    size = int(match.group(1), 0)
    slots.append(size * {"": 1, "k": 1024, "m": 1024 * 1024}[match.group(2).lower()])

print(max(slots) if slots else 0, len(slots))
PY
)"
if [ "$SLOTS" -lt 2 ]; then
    echo "partitions.csv: $SLOTS app slot(s); OTA needs two (see README)" >&2
    exit 1
fi
echo "partitions.csv: $SLOTS app slots of $SLOT_BYTES bytes"
# The table itself, built by the IDF tool that builds the table the flash uses.
# A CSV whose offsets do not line up fails here instead of halfway through a
# flash, and the file this leaves behind is the one the README says to write at
# 0x8000.
PARTITION_TABLE=$(echo .embuild/espressif/esp-idf/*/components/partition_table/gen_esp32part.py)
if [ -f "$PARTITION_TABLE" ]; then
    .venv-tools/bin/python "$PARTITION_TABLE" --flash-size 16MB partitions.csv target/partition-table.bin
    echo "partition table: $(stat -c %s target/partition-table.bin) bytes at target/partition-table.bin"
else
    echo "partition table: SKIPPED (no ESP-IDF checkout under .embuild)"
fi
for profile in dev release; do
    directory=$([ "$profile" = dev ] && echo debug || echo release)
    cargo build --locked --profile "$profile"
    .venv-tools/bin/python -m esptool --chip esp32s3 elf2image \
        --flash-mode dio --flash-freq 40m --flash-size 16MB \
        --output "target/xtensa-esp32s3-espidf/$directory/easellet.bin" \
        "target/xtensa-esp32s3-espidf/$directory/easellet"
    image="target/xtensa-esp32s3-espidf/$directory/easellet.bin"
    size=$(stat -c %s "$image")
    headroom=$((SLOT_BYTES - size))
    # The size is printed on every build because a partition that is one feature
    # away from full is a fact worth seeing before the next flash, not after.
    printf 'image %s bytes %s headroom=%s fits=%s\n' "$size" "$image" "$headroom" \
        "$([ "$headroom" -ge 0 ] && echo yes || echo NO)"
done
