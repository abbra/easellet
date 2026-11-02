#!/usr/bin/env bash
# Build and test the OpenDisplay protocol crate on the host.
#
# Same arrangement as bridge/build.sh: cargo reads .cargo/config.toml from the
# *current directory* upwards, and the project's config pins the xtensa target and
# `-Zbuild-std`. Running from a scratch directory with the distro toolchain keeps
# the host build clean.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

cargo_bin="${PHOTOFRAME_HOST_CARGO:-/usr/bin/cargo}"
unset CARGO_HOME RUSTUP_HOME RUSTUP_TOOLCHAIN
export PATH="/usr/bin:$PATH"
# Keep build output next to the crate rather than in the throwaway directory.
export CARGO_TARGET_DIR="$here/target"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
cd "$scratch"
case "${1:-test}" in
    test)
        "$cargo_bin" fmt --check --manifest-path "$here/Cargo.toml"
        "$cargo_bin" test --quiet --manifest-path "$here/Cargo.toml"
        # The other direction: py-opendisplay parses what the server answered
        # to its own requests (written by tests/client.rs).
        client_python="$here/../.tools/venv-opendisplay/bin/python"
        if [[ -x "$client_python" ]]; then
            "$client_python" -I "$here/tests/fixtures/check_client_responses.py" \
                "$CARGO_TARGET_DIR/tmp/client-responses.txt"
            "$client_python" -I "$here/tests/fixtures/check_mdns_responses.py" \
                "$CARGO_TARGET_DIR/tmp/mdns-responses.txt"
            "$client_python" -I "$here/tests/fixtures/check_msd.py" \
                "$CARGO_TARGET_DIR/tmp/msd-telemetry.txt" "$here/tests/fixtures/board-config.bin"
        else
            echo "client check: SKIPPED (no .tools/venv-opendisplay)"
        fi
        ;;
    clippy)
        "$cargo_bin" clippy --quiet --all-targets --manifest-path "$here/Cargo.toml" -- -D warnings
        ;;
    fmt)
        "$cargo_bin" fmt --manifest-path "$here/Cargo.toml"
        ;;
    *)
        echo "usage: build.sh [test|clippy|fmt]" >&2
        exit 2
        ;;
esac
