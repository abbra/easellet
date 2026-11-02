# Source this file from bash: source scripts/env.sh
# Toolchain lives in this project; the distro Rust installation is unchanged.
_easellet_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export CARGO_HOME="$_easellet_root/.tools/cargo"
export RUSTUP_HOME="$_easellet_root/.tools/rustup"
export PATH="$CARGO_HOME/bin:$_easellet_root/.tools/bin:$PATH"
if [[ -f "$_easellet_root/.tools/export-esp.sh" ]]; then
    source "$_easellet_root/.tools/export-esp.sh"
fi
unset _easellet_root
