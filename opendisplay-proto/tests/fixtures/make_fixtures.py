#!/usr/bin/env python3
"""Regenerate the zlib fixture for tests/server.rs.

Standard library only. Compresses a synthetic 800x480 Spectra 6 frame exactly as
py-opendisplay does for a device that advertises streaming decompression
(encoding/compression.py: zlib level 6, wbits 9, i.e. a 512-byte window). The
Rust test rebuilds the same frame with `spectra_frame()` and compares.
"""

import pathlib
import zlib

WIDTH, HEIGHT = 800, 480
CODES = (0, 1, 2, 3, 5, 6)  # black, white, yellow, red, blue, green


def nibble(x: int, y: int) -> int:
    v = (x // 40 + y // 30) % 6
    if (x * 31 + y * 17) % 7 == 0:
        v = (v + 3) % 6
    return CODES[v]


def frame() -> bytes:
    return bytes(
        (nibble(x, y) << 4) | nibble(x + 1, y)
        for y in range(HEIGHT)
        for x in range(0, WIDTH, 2)
    )


def main() -> None:
    compressor = zlib.compressobj(level=6, method=zlib.DEFLATED, wbits=9)
    stream = compressor.compress(frame()) + compressor.flush()
    out = pathlib.Path(__file__).with_name("spectra-800x480-wbits9.zlib")
    out.write_bytes(stream)
    print(f"{out.name}: {len(stream)} bytes, header {stream[:2].hex()}")


if __name__ == "__main__":
    main()
