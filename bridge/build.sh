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
case "${1:-test}" in
    test)
        # The firmware's `cargo fmt --check` at the repository root does not reach
        # this package: it is a separate manifest, not a workspace member.
        "$cargo_bin" fmt --check --manifest-path "$here/Cargo.toml"
        "$cargo_bin" test --quiet --manifest-path "$here/Cargo.toml"
        ;;
    run)
        shift
        exec "$cargo_bin" run --quiet --manifest-path "$here/Cargo.toml" -- "$@"
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
        exec "$cargo_bin" run --quiet --release --manifest-path "$here/Cargo.toml" \
            --features whisper -- "$@"
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
