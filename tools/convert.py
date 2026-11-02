#!/usr/bin/env python3
"""Convert any Pillow-readable image to PhotoPainter's 192000-byte raw frame.

The output is 4 bits per pixel, two pixels per byte, high nibble for even native
columns. The panel scans rotated 180 degrees, so viewer (0,0) is written to native
(799,479).
Floyd-Steinberg error diffusion targets exactly these six RGB colours:
black (0,0,0), white (255,255,255), yellow (255,255,0), red (255,0,0),
blue (0,0,255), green (0,255,0).
"""

import argparse
from pathlib import Path

try:
    from PIL import Image, ImageOps
except ImportError as error:
    raise SystemExit("error: Pillow is required (python -m pip install Pillow)") from error

WIDTH, HEIGHT = 800, 480
PALETTE = [
    (0, (0, 0, 0)),
    (1, (255, 255, 255)),
    (2, (255, 255, 0)),
    (3, (255, 0, 0)),
    (5, (0, 0, 255)),
    (6, (0, 255, 0)),
]


def nearest(rgb):
    r, g, b = rgb
    return min(
        PALETTE,
        key=lambda item: (r - item[1][0]) ** 2
        + (g - item[1][1]) ** 2
        + (b - item[1][2]) ** 2,
    )


def convert(source, dither=True):
    with Image.open(source) as opened:
        image = ImageOps.fit(
            opened.convert("RGB"),
            (WIDTH, HEIGHT),
            method=Image.Resampling.LANCZOS,
            centering=(0.5, 0.5),
        )
    return frame_from_rgb(image.tobytes(), dither=dither)


def frame_from_rgb(data, dither=True):
    """Pack 800x480 RGB bytes into a raw frame.

    Split out from `convert` so `tools/check-el6-js.py` can hand this and the
    browser-side `tools/el6.mjs` the *same* pixels: the resize is Pillow's and
    the browser's, but everything after it must be identical, byte for byte.
    """
    if len(data) != WIDTH * HEIGHT * 3:
        raise SystemExit(f"internal error: need {WIDTH * HEIGHT * 3} RGB bytes")
    pixels = [list(data[index : index + 3]) for index in range(0, len(data), 3)]

    indices = bytearray(WIDTH * HEIGHT)
    for y in range(HEIGHT):
        for x in range(WIDTH):
            offset = y * WIDTH + x
            old = pixels[offset]
            current = tuple(max(0, min(255, round(channel))) for channel in old)
            index, target = nearest(current)
            indices[offset] = index
            error = [old[channel] - target[channel] for channel in range(3)]
            if not dither:
                continue
            if x + 1 < WIDTH:
                spread(pixels, offset + 1, error, 7 / 16)
            if y + 1 < HEIGHT:
                row = offset + WIDTH
                if x:
                    spread(pixels, row - 1, error, 3 / 16)
                spread(pixels, row, error, 5 / 16)
                if x + 1 < WIDTH:
                    spread(pixels, row + 1, error, 1 / 16)

    frame = bytearray(WIDTH * HEIGHT // 2)
    for y in range(HEIGHT):
        for x in range(WIDTH):
            native_x = WIDTH - 1 - x
            native_y = HEIGHT - 1 - y
            address = native_y * (WIDTH // 2) + native_x // 2
            index = indices[y * WIDTH + x]
            if native_x % 2 == 0:
                frame[address] |= index << 4
            else:
                frame[address] |= index
    return frame


def spread(pixels, offset, error, factor):
    pixel = pixels[offset]
    for channel in range(3):
        pixel[channel] += error[channel] * factor


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path, help="raw .el6 output path")
    parser.add_argument(
        "--no-dither",
        action="store_true",
        help="snap each pixel to the nearest of the six colours instead of diffusing",
    )
    args = parser.parse_args()
    frame = convert(args.source, dither=not args.no_dither)
    if len(frame) != WIDTH * HEIGHT // 2:
        raise SystemExit("internal error: wrong output size")
    args.output.write_bytes(frame)
    print(f"wrote {args.output} ({len(frame)} bytes, {WIDTH}x{HEIGHT}, 6-colour 4bpp)")


if __name__ == "__main__":
    main()
