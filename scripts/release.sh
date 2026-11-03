#!/usr/bin/env bash
# Cut a release: bump the version everywhere, generate the changelog section,
# commit, tag. It does not push, because a release is a decision and the push is
# the one step worth doing by hand.
#
#   scripts/release.sh 0.2.0
#   scripts/release.sh 0.2.0 --no-check    # a rehearsal, on a branch
#
# Run it on a clean `main`: the tag names what CI will build, and CI checks that
# the tag is an ancestor of `main`.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

version="${1-}"
check="${2-}"
case "$check" in
    "" | --no-check) ;;
    *) echo "usage: scripts/release.sh <version> [--no-check]" >&2; exit 2 ;;
esac
if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    echo "ERR '$version' is not MAJOR.MINOR.PATCH; a dev build is not a release" >&2
    exit 2
fi

current="$(python3 tools/check-version.py --print)"
echo "version $current -> $version"

branch="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$check" != --no-check ]]; then
    if [[ "$branch" != main ]]; then
        echo "ERR on branch '$branch'; a release is cut on main" >&2
        exit 1
    fi
    if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
        echo "ERR the tree has uncommitted changes; commit or stash them first" >&2
        exit 1
    fi
    if ! command -v git-cliff >/dev/null; then
        echo "ERR git-cliff is missing (dnf install git-cliff, or cargo install git-cliff)" >&2
        exit 1
    fi
    echo "== tools/check-version.py =="
    python3 tools/check-version.py
    echo "== scripts/check.sh =="
    bash scripts/check.sh
fi

# Every place that names the version. The lock files carry it too, and a bump
# that misses them fails tools/check-version.py, which is the point.
sed_i() { sed -i "$1" "$2"; }
for manifest in Cargo.toml bridge/Cargo.toml opendisplay-proto/Cargo.toml; do
    # Only the line under [package]: dependencies carry versions of their own.
    sed_i "/^\[package\]/,/^\[/ s/^version = \"[^\"]*\"\$/version = \"$version\"/" "$manifest"
done
sed_i "s/^CONFIG_APP_PROJECT_VER=\"[^\"]*\"\$/CONFIG_APP_PROJECT_VER=\"$version\"/" sdkconfig.defaults

# The lock carries the package version as well. cargo reads .cargo/config.toml
# from the current directory upwards, and the repository pins the xtensa target
# there, so this runs from a scratch directory with the host toolchain. A lock
# written by an older cargo can come back with its package entries in a different
# order: that is the whole reason the diff of a release is bigger than one line.
cargo_bin="${EASEL_HOST_CARGO:-/usr/bin/cargo}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
(
    unset CARGO_HOME RUSTUP_HOME RUSTUP_TOOLCHAIN
    export PATH="/usr/bin:$PATH"
    cd "$work"
    "$cargo_bin" update --offline --manifest-path "$here/Cargo.toml" -p easellet -p opendisplay-proto
    "$cargo_bin" update --offline --manifest-path "$here/bridge/Cargo.toml" -p easellet-bridge -p opendisplay-proto
    "$cargo_bin" update --offline --manifest-path "$here/opendisplay-proto/Cargo.toml" -p opendisplay-proto
)

section="$work/changelog-section.md"
git cliff --tag "v$version" --output "$section"
# The generated section goes below the title and the paragraph that explains the
# changelog, and above the releases that are already there.
python3 - "$section" <<'PY'
import sys
from pathlib import Path

section = Path(sys.argv[1]).read_text(encoding="utf-8").strip()
path = Path("CHANGELOG.md")
lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
first = next(index for index, line in enumerate(lines) if line.startswith("## ["))
path.write_text("".join(lines[:first]) + section + "\n\n" + "".join(lines[first:]), encoding="utf-8")
PY

python3 tools/check-version.py "v$version"

git add --update
git commit --signoff --message "chore(release): $version"
git tag --annotate "v$version" --message "Easellet $version"

cat <<NOTE
tagged v$version. Read it once more, then:

    git push origin main v$version

CI builds the tag and drafts the release; the draft is published by hand.
NOTE
