#!/usr/bin/env python3
"""Check that one version number is written in every place that names it.

`Cargo.toml` is the only source a person edits; `scripts/release.sh` writes the
rest. This script is the reason a forgotten copy is caught before a tag is
pushed rather than after: the frame's `ota` command reports the version Cargo
was built with, while the app description reports `CONFIG_APP_PROJECT_VER`, and
an image whose two slots disagree about themselves is a bug nobody can see from
the outside.

    tools/check-version.py            # every copy agrees
    tools/check-version.py v0.2.0     # ... and names this tag
    tools/check-version.py --print    # just the version, for scripts

Exit status is 0 when every copy agrees, 1 when one does not, 2 when a file
cannot be read or does not hold a version at all.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATES = ("Cargo.toml", "bridge/Cargo.toml", "opendisplay-proto/Cargo.toml")
LOCKS = ("Cargo.lock", "bridge/Cargo.lock", "opendisplay-proto/Cargo.lock")
# The packages that carry the repository's version. Anything else in a lock file
# (500 dependencies) has its own and is not our business.
PACKAGES = ("easellet", "easellet-bridge", "opendisplay-proto")

# `[package]` only: `[dependencies]` has `version = "=0.53.0"` lines that must
# not be mistaken for the package's own.
_MANIFEST_VERSION = re.compile(r'^\[package\][^\[]*?^version\s*=\s*"([^"]+)"', re.M | re.S)
_LOCK_PACKAGE = re.compile(r'^\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"', re.M)
_PROJECT_VER = re.compile(r'^CONFIG_APP_PROJECT_VER="([^"]+)"', re.M)
# The newest section of CHANGELOG.md; older entries keep their version.
_CHANGELOG = re.compile(r"^## \[(\d+\.\d+\.\d+[^\]]*)\]", re.M)


def fail(message: str) -> None:
    print(f"ERR {message}", file=sys.stderr)
    sys.exit(2)


def read(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as error:
        fail(f"{path.relative_to(ROOT)}: {error}")


def manifest_version(path: str) -> str:
    match = _MANIFEST_VERSION.search(read(ROOT / path))
    if not match:
        fail(f"{path}: no version under [package]")
    return match.group(1)


def lock_versions(path: str) -> list[tuple[str, str]]:
    found = [(name, version) for name, version in _LOCK_PACKAGE.findall(read(ROOT / path))]
    return [(name, version) for name, version in found if name in PACKAGES]


def sdkconfig_version() -> str:
    match = _PROJECT_VER.search(read(ROOT / "sdkconfig.defaults"))
    if not match:
        fail("sdkconfig.defaults: no CONFIG_APP_PROJECT_VER")
    return match.group(1)


def changelog_version() -> str:
    match = _CHANGELOG.search(read(ROOT / "CHANGELOG.md"))
    if not match:
        fail("CHANGELOG.md: no '## [x.y.z]' section")
    return match.group(1)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("tag", nargs="?", help="a tag to compare, e.g. v0.2.0")
    parser.add_argument("--print", dest="print_only", action="store_true",
                        help="print the version and stop")
    arguments = parser.parse_args()

    version = manifest_version(CRATES[0])
    if arguments.print_only:
        print(version)
        return 0

    # (what to call it in the report, what it says)
    places: list[tuple[str, str]] = []
    for path in CRATES[1:]:
        places.append((path, manifest_version(path)))
    for path in LOCKS:
        for name, locked in lock_versions(path):
            places.append((f"{path} ({name})", locked))
    places.append(("sdkconfig.defaults", sdkconfig_version()))
    places.append(("CHANGELOG.md", changelog_version()))

    wrong = [f"{where} says {found}, expected {version}"
             for where, found in places if found != version]
    # A dev build says `0.1.0-dev+g1a2b3c4`, which belongs in the generated
    # sdkconfig only (scripts/build-firmware.sh), never in a checked-in file, so
    # the tag is the one place a suffix cannot appear.
    if arguments.tag is not None and arguments.tag != f"v{version}":
        wrong.append(f"tag {arguments.tag}, expected v{version}")

    if wrong:
        for line in wrong:
            print(f"MISMATCH {line}", file=sys.stderr)
        print(f"ERR version {version} is not in every place; "
              f"run scripts/release.sh <version>", file=sys.stderr)
        return 1

    print(f"version {version}: {len(places)} copies agree "
          f"({', '.join(where for where, _ in places)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
