# Source this file from bash: source scripts/env.sh
# Toolchain lives in this project; the distro Rust installation is unchanged.
_photoframe_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export CARGO_HOME="$_photoframe_root/.tools/cargo"
export RUSTUP_HOME="$_photoframe_root/.tools/rustup"
export PATH="$CARGO_HOME/bin:$_photoframe_root/.tools/bin:$PATH"
if [[ -f "$_photoframe_root/.tools/export-esp.sh" ]]; then
    source "$_photoframe_root/.tools/export-esp.sh"
fi
unset _photoframe_root
