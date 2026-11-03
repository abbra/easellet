#!/usr/bin/env python3
"""Print the app slot size and the number of app slots in `partitions.csv`.

The size ceiling comes from the table the frame is flashed with, not from
`sdkconfig`: the app is built as a raw image and the table is written at 0x8000
by esptool, so a build that fits sdkconfig's single-app layout can still be too
large for a real slot. Two equal app slots are also what OTA needs, so a table
that lost one is worth failing on.

    tools/partitions.py            # "3145728 2"
    tools/partitions.py --sizes    # every app slot, one per line
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
_SCALE = {"": 1, "k": 1024, "m": 1024 * 1024}
_SIZE = re.compile(r"(0x[0-9a-fA-F]+|[0-9]+)([KkMm]?)")


def app_slots() -> list[int]:
    sizes = []
    for line in (ROOT / "partitions.csv").read_text(encoding="utf-8").splitlines():
        if line.lstrip().startswith("#"):
            continue
        fields = [field.strip() for field in line.split(",")]
        if len(fields) < 5 or fields[1] != "app":
            continue
        match = _SIZE.fullmatch(fields[4])
        if not match:
            sys.exit(f"ERR partitions.csv: cannot read size {fields[4]!r}")
        sizes.append(int(match.group(1), 0) * _SCALE[match.group(2).lower()])
    return sizes


def main() -> int:
    sizes = app_slots()
    if "--sizes" in sys.argv[1:]:
        print("\n".join(str(size) for size in sizes))
        return 0
    if not sizes:
        print("ERR partitions.csv: no app slot", file=sys.stderr)
        return 1
    print(f"{max(sizes)} {len(sizes)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
