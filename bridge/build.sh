#!/usr/bin/env bash
# Build and test the bridge.
#
# Cargo reads .cargo/config.toml by walking up from the *current directory*, not from
# the manifest, and this project's parent config pins the xtensa target, `-Zbuild-std`
# and the ESP-IDF environment. Building from a scratch directory therefore keeps the
# host build clean without duplicating config or fighting the merge rules.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# The firmware pins the xtensa toolchain inside the project (.tools/rustup), and
# scripts/env.sh puts it first on PATH. The bridge is a host program and belongs to
# the distro Rust, so both the toolchain and the cargo/rustup homes are cleared here
# rather than inherited. Override with EASEL_HOST_CARGO if that is wrong.
cargo_bin="${EASEL_HOST_CARGO:-/usr/bin/cargo}"
unset CARGO_HOME RUSTUP_HOME RUSTUP_TOOLCHAIN
# The rustup shims in .tools/cargo/bin would otherwise still be found first and then
# fail with "no default toolchain", so put the distro bin directory ahead of them.
export PATH="/usr/bin:$PATH"
"$cargo_bin" --version
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
cd "$scratch"
target_dir="${CARGO_TARGET_DIR:-${CARGO_BUILD_TARGET_DIR:-$here/target}}"
[[ "$target_dir" = /* ]] || target_dir="$scratch/$target_dir"
case "${1:-test}" in
    test)
        # The firmware's `cargo fmt --check` at the repository root does not reach
        # this package: it is a separate manifest, not a workspace member.
        "$cargo_bin" fmt --check --manifest-path "$here/Cargo.toml"
        "$cargo_bin" test --quiet --manifest-path "$here/Cargo.toml"
        python3 -m unittest discover -s "$here/scripts" -p 'test_*.py'
        # `cargo test` builds the test harnesses and leaves the executable alone,
        # and tools/bridge-smoke.py starts that executable to ask the real server
        # what it serves. Building it here keeps one command enough for both.
        "$cargo_bin" build --quiet --manifest-path "$here/Cargo.toml"
        ;;
    run)
        shift
        [ "${1:-}" != -- ] || shift
        "$cargo_bin" build --quiet --manifest-path "$here/Cargo.toml" --target-dir "$target_dir"
        cd "$here/.."
        exec "$target_dir/debug/easellet-bridge" "$@"
        ;;
    fmt)
        "$cargo_bin" fmt --manifest-path "$here/Cargo.toml"
        ;;
    clippy)
        "$cargo_bin" clippy --quiet --manifest-path "$here/Cargo.toml" -- -D warnings
        ;;
    # Recognition is only usable in a release build: the same ten seconds of speech
    # took 8.5 s here in release and 120 s in debug, which is what rwhisper's own
    # "models will run extremely slowly in debug mode" warning is about.
    whisper)
        "$cargo_bin" build --release --manifest-path "$here/Cargo.toml" --features whisper
        ;;
    serve)
        shift
        [ "${1:-}" != -- ] || shift
        "$cargo_bin" build --quiet --release --manifest-path "$here/Cargo.toml" \
            --target-dir "$target_dir" --features whisper
        cd "$here/.."
        exec "$target_dir/release/easellet-bridge" "$@"
        ;;
    # Recognition without the frame: `build.sh transcribe small ru file.wav`.
    transcribe)
        shift
        exec "$cargo_bin" run --quiet --release --manifest-path "$here/Cargo.toml" \
            --features whisper --example transcribe -- "$@"
        ;;
    *)
        echo "usage: build.sh [test|fmt|clippy|whisper|serve|transcribe|run -- args...]" >&2
        exit 2
        ;;
esac
