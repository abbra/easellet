#!/usr/bin/env python3
"""Merge one release into `firmware-index.json`, the file the bridge reads.

A release is a tag; the bridge wants one file that answers "which versions
exist, what is each one's hash, which commit is it". This tool builds that file:
the previous release's index plus this release, newest first. The tag job and the
`dev` job both call it, and it is the only place that decides the file's shape,
so the schema is described once.

    tools/firmware-index.py --version 0.2.0 --tag v0.2.0 \
        --image easellet-0.2.0.bin --bytes 1170432 --sha256 3f9c… \
        --url https://github.com/o/r/releases/download/v0.2.0/easellet-0.2.0.bin \
        --slot-bytes 3145728 --idf v5.5.5 --rust 1.97.0.0 --commit 1a2b3c4 \
        --notes https://github.com/o/r/releases/tag/v0.2.0 \
        --previous firmware-index.json --out firmware-index.json

    tools/firmware-index.py --self-test

Exit status is 0 on success, 2 when an input is refused. A refused input is a
release that would have been published with a lie in it, so it is worth failing
loudly: a hash that is not a hash, a size that is zero, a previous index from a
newer schema than this tool knows.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

SCHEMA = 1
# One board, one panel, one partition table. A second board is a second index,
# not a field to guess at.
BOARD = "easellet-spectra6-800x480"
CHANNELS = ("stable", "dev")

_VERSION = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z][0-9A-Za-z.+-]*)?$")
_SHA256 = re.compile(r"^[0-9a-f]{64}$")


class Refused(Exception):
    """An input that would have produced a wrong index."""


def refuse(condition: bool, message: str) -> None:
    if not condition:
        raise Refused(message)


def check_entry(entry: dict) -> dict:
    """Refuse an entry that a frame or a person would read and misbelieve."""
    refuse(_VERSION.match(entry["version"]) is not None,
           f"version {entry['version']!r} is not x.y.z or x.y.z-suffix")
    refuse(entry["image"]["bytes"] > 0, "the image has no bytes")
    refuse(_SHA256.match(entry["image"]["sha256"]) is not None,
           f"sha256 {entry['image']['sha256']!r} is not 64 lowercase hex digits")
    refuse(entry["image"]["url"].startswith(("https://", "http://")),
           f"url {entry['image']['url']!r} is not absolute")
    refuse(entry["slot_bytes"] > 0, "the slot has no bytes")
    return entry


def merge(previous: dict | None, entry: dict, channel: str,
          generated: str) -> dict:
    """Previous index plus one release, newest first.

    Re-running a release replaces its own entry instead of adding a second one,
    because a release is uploaded with `--clobber` and a person who retries the
    job should not end up with two versions that claim the same number.
    """
    refuse(channel in CHANNELS, f"channel {channel!r} is not one of {CHANNELS}")

    releases: list[dict] = []
    if previous is not None:
        refuse(previous.get("schema") == SCHEMA,
               f"the previous index is schema {previous.get('schema')!r}, "
               f"this tool writes schema {SCHEMA}")
        refuse(previous.get("board") in (None, BOARD),
               f"the previous index is for board {previous.get('board')!r}")
        releases = [release for release in previous.get("releases", [])
                    if release.get("version") != entry["version"]]

    if channel == "dev":
        # The dev release is a moving target: exactly one build, replaced on
        # every green main.
        releases = []
    else:
        # A dev build says so in its version, and the stable list is what the
        # bridge offers by default, so it never carries one.
        releases = [release for release in releases
                    if "-" not in str(release.get("version"))]

    releases.append(entry)
    releases.sort(key=lambda release: str(release.get("date", "")), reverse=True)
    return {
        "schema": SCHEMA,
        "board": BOARD,
        "channel": channel,
        "generated": generated,
        "releases": releases,
    }


def now_utc() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-test", action="store_true",
                        help="check the merge rules and stop")
    parser.add_argument("--version", help="the release's version, e.g. 0.2.0")
    parser.add_argument("--tag", help="the tag, e.g. v0.2.0 or firmware-dev")
    parser.add_argument("--date", help="when the release was made (ISO 8601, UTC)")
    parser.add_argument("--image", help="the asset's file name")
    parser.add_argument("--bytes", type=int, help="the asset's size")
    parser.add_argument("--sha256", help="the asset's hash")
    parser.add_argument("--url", help="where the asset is downloaded from")
    parser.add_argument("--slot-bytes", type=int,
                        help="the app slot the image was built for")
    parser.add_argument("--idf", default="", help="the ESP-IDF the image used")
    parser.add_argument("--rust", default="", help="the Rust the image used")
    parser.add_argument("--commit", default="", help="the commit it was built from")
    parser.add_argument("--notes", default="", help="the release page")
    parser.add_argument("--channel", default="stable", choices=CHANNELS)
    parser.add_argument("--previous", help="the index this release follows")
    parser.add_argument("--out", help="where to write the index")
    parser.add_argument("--generated", help="override the fetch time (for a test)")
    return parser.parse_args(argv)


def entry_from(arguments: argparse.Namespace) -> dict:
    missing = [name for name in ("version", "tag", "image", "bytes", "sha256",
                                 "url", "slot_bytes")
               if getattr(arguments, name) in (None, "")]
    refuse(not missing, f"missing {', '.join('--' + name.replace('_', '-') for name in missing)}")
    return check_entry({
        "version": arguments.version,
        "tag": arguments.tag,
        "date": arguments.date or now_utc(),
        "image": {
            "name": arguments.image,
            "bytes": arguments.bytes,
            "sha256": arguments.sha256,
            "url": arguments.url,
        },
        "slot_bytes": arguments.slot_bytes,
        "idf": arguments.idf,
        "rust": arguments.rust,
        "commit": arguments.commit,
        "notes": arguments.notes,
    })


def main(argv: list[str]) -> int:
    arguments = parse_arguments(argv)
    try:
        if arguments.self_test:
            return self_test()
        previous = None
        if arguments.previous and Path(arguments.previous).exists():
            previous = json.loads(Path(arguments.previous).read_text(encoding="utf-8"))
        index = merge(previous, entry_from(arguments), arguments.channel,
                      arguments.generated or now_utc())
    except Refused as refused:
        print(f"ERR {refused}", file=sys.stderr)
        return 2
    except (OSError, json.JSONDecodeError) as error:
        print(f"ERR {arguments.previous}: {error}", file=sys.stderr)
        return 2

    text = json.dumps(index, indent=2, ensure_ascii=False) + "\n"
    if arguments.out:
        Path(arguments.out).write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    print(f"firmware index {index['channel']}: {len(index['releases'])} release(s), "
          f"newest {index['releases'][0]['version']}", file=sys.stderr)
    return 0


def self_test() -> int:
    """The merge rules, exercised in a temporary directory."""
    def entry(version: str, date: str, sha: str = "a" * 64) -> dict:
        return check_entry({
            "version": version, "tag": f"v{version}", "date": date,
            "image": {"name": f"easellet-{version}.bin", "bytes": 1170416,
                      "sha256": sha, "url": f"https://example.org/{version}.bin"},
            "slot_bytes": 3145728, "idf": "v5.5.5", "rust": "1.97.0.0",
            "commit": "1a2b3c4", "notes": "",
        })

    def refused(action) -> bool:
        try:
            action()
        except Refused:
            return True
        return False

    generated = "2026-11-10T12:04:11Z"
    cases: list[tuple[str, bool]] = []

    first = merge(None, entry("0.1.0", "2026-11-03T09:00:00Z"), "stable", generated)
    cases.append(("the first index holds one release",
                  [release["version"] for release in first["releases"]] == ["0.1.0"]
                  and first["schema"] == SCHEMA and first["generated"] == generated))

    second = merge(first, entry("0.2.0", "2026-11-10T12:00:00Z"), "stable", generated)
    cases.append(("a merge puts the new release first",
                  [release["version"] for release in second["releases"]]
                  == ["0.2.0", "0.1.0"]))

    replaced = merge(second, entry("0.2.0", "2026-11-11T08:00:00Z"), "stable", generated)
    cases.append(("a repeated version replaces its entry",
                  [release["version"] for release in replaced["releases"]]
                  == ["0.2.0", "0.1.0"]
                  and replaced["releases"][0]["date"] == "2026-11-11T08:00:00Z"))

    with_dev = merge(merge(None, entry("0.1.0-dev+gabc1234",
                                      "2026-11-12T09:00:00Z"), "dev", generated),
                     entry("0.2.0", "2026-11-13T09:00:00Z"), "stable", generated)
    cases.append(("a dev build never reaches the stable list",
                  [release["version"] for release in with_dev["releases"]]
                  == ["0.2.0"]))

    dev = merge(second, entry("0.2.0-dev+gdef5678", "2026-11-14T09:00:00Z"),
                "dev", generated)
    cases.append(("the dev index keeps exactly one build",
                  [release["version"] for release in dev["releases"]]
                  == ["0.2.0-dev+gdef5678"]))

    cases.append(("a hash that is not a hash is refused",
                  refused(lambda: entry("0.2.0", "2026-11-10T12:00:00Z", sha="3F9C"))))
    cases.append(("a version that is not x.y.z is refused",
                  refused(lambda: entry("0.2", "2026-11-10T12:00:00Z"))))
    cases.append(("an index from a newer schema is refused",
                  refused(lambda: merge({"schema": SCHEMA + 1, "releases": []},
                                        entry("0.2.0", "2026-11-10T12:00:00Z"),
                                        "stable", generated))))

    with tempfile.TemporaryDirectory() as scratch:
        path = Path(scratch) / "firmware-index.json"
        path.write_text(json.dumps(second, indent=2) + "\n", encoding="utf-8")
        read_back = json.loads(path.read_text(encoding="utf-8"))
        cases.append(("the file survives a round trip",
                      read_back == second))

    for name, ok in cases:
        print(f"{'ok  ' if ok else 'FAIL'} {name}")
    failed = [name for name, ok in cases if not ok]
    if failed:
        print(f"ERR {len(failed)} of {len(cases)} cases failed", file=sys.stderr)
        return 1
    print(f"firmware-index: {len(cases)} cases ok")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
