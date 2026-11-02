#!/usr/bin/env python3
"""Prove `tools/el6.mjs` and `tools/convert.py` produce identical frames.

The upload page will convert pictures in the browser and post a raw frame; the
SD-card path converts with `tools/convert.py` on the host. If those two ever
disagree, the same picture would look different depending on how it arrived,
and nothing on the frame would say why. So both are fed the same 800x480 RGB
bytes - the resize is necessarily engine-specific, everything after it is not -
and the resulting frames are compared byte for byte.

usage:
    tools/check-el6-js.py                 # generated images
    tools/check-el6-js.py photo.jpg ...   # real ones, cover-fitted first
"""

import hashlib
import subprocess
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageOps

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from convert import HEIGHT, WIDTH, frame_from_rgb  # noqa: E402

SCRATCH = Path.home() / "tmp" / "frame" / "el6-check"


def generated() -> list[tuple[str, Image.Image]]:
    """Images that exercise the parts most likely to drift.

    A smooth gradient hits every diffusion carry; seeded noise hits the tie
    cases in `nearest` and the half-to-even rounding; exact palette colours must
    survive untouched; a tiny source exercises upscaling.
    """
    gradient = Image.new("RGB", (WIDTH, HEIGHT))
    draw = ImageDraw.Draw(gradient)
    for x in range(WIDTH):
        draw.line([(x, 0), (x, HEIGHT)], fill=(x * 255 // WIDTH, 128, 255 - x * 255 // WIDTH))

    # Deterministic, so a failure is reproducible: LCG rather than `random`.
    noise = Image.new("RGB", (WIDTH, HEIGHT))
    pixels = noise.load()
    state = 12345
    for y in range(HEIGHT):
        for x in range(WIDTH):
            state = (1103515245 * state + 12345) & 0x7FFFFFFF
            pixels[x, y] = ((state >> 16) & 0xFF, (state >> 8) & 0xFF, state & 0xFF)

    blocks = Image.new("RGB", (WIDTH, HEIGHT))
    draw = ImageDraw.Draw(blocks)
    palette = [(0, 0, 0), (255, 255, 255), (255, 255, 0), (255, 0, 0), (0, 0, 255), (0, 255, 0)]
    for index, colour in enumerate(palette):
        x = index * WIDTH // len(palette)
        draw.rectangle([x, 0, x + WIDTH // len(palette) - 1, HEIGHT - 1], fill=colour)

    tiny = Image.new("RGB", (64, 64))
    draw = ImageDraw.Draw(tiny)
    draw.ellipse([0, 0, 63, 63], fill=(200, 40, 40))
    draw.rectangle([16, 16, 47, 47], fill=(20, 40, 200))

    return [
        ("gradient", gradient),
        ("noise", noise),
        ("palette", blocks),
        ("tiny-upscale", tiny),
    ]


def fitted(image: Image.Image) -> bytes:
    return ImageOps.fit(
        image.convert("RGB"),
        (WIDTH, HEIGHT),
        method=Image.Resampling.LANCZOS,
        centering=(0.5, 0.5),
    ).tobytes()


def check(name: str, image: Image.Image) -> bool:
    rgb = fitted(image)
    rgba = bytearray()
    for index in range(0, len(rgb), 3):
        rgba += rgb[index : index + 3] + b"\xff"

    source = SCRATCH / f"{name}.rgba"
    source.write_bytes(rgba)
    ok = True
    for dither in (True, False):
        node_output = SCRATCH / f"{name}-{'dither' if dither else 'plain'}.el6"
        command = ["node", str(HERE / "el6-check.mjs"), str(source), str(node_output)]
        if not dither:
            command.append("--no-dither")
        completed = subprocess.run(command, check=True, capture_output=True, text=True)
        if "roundtrip=ok" not in completed.stdout:
            ok = False
            print(f"  FAIL {label:24} unpack/pack does not round-trip")
            continue
        from_js = node_output.read_bytes()
        from_py = bytes(frame_from_rgb(rgb, dither=dither))
        label = f"{name} {'dither' if dither else 'nearest'}"
        if from_js == from_py:
            digest = hashlib.sha256(from_js).hexdigest()[:16]
            print(f"  ok   {label:24} {len(from_js)} bytes sha256:{digest}")
            continue
        ok = False
        differing = sum(1 for a, b in zip(from_js, from_py) if a != b)
        first = next(
            (index for index, (a, b) in enumerate(zip(from_js, from_py)) if a != b),
            None,
        )
        print(f"  FAIL {label:24} {differing} differing bytes, first at {first}")
        print(f"       js sha256:{hashlib.sha256(from_js).hexdigest()[:16]}")
        print(f"       py sha256:{hashlib.sha256(from_py).hexdigest()[:16]}")
    return ok


def main() -> int:
    SCRATCH.mkdir(parents=True, exist_ok=True)
    subjects = generated()
    for argument in sys.argv[1:]:
        path = Path(argument)
        with Image.open(path) as opened:
            subjects.append((path.stem.lower().replace(" ", "-")[:24], opened.copy()))

    failures = 0
    for name, image in subjects:
        print(f"{name} ({image.width}x{image.height})")
        if not check(name, image):
            failures += 1
    if failures:
        print(f"FAIL {failures} of {len(subjects)} images disagree")
        return 1
    print(f"OK {len(subjects)} images, both modes, byte-identical")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
