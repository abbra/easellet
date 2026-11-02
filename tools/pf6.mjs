// PhotoPainter's native frame format, for the browser and for Node.
//
// The frame's own decoder is deliberately strict: a `.pf6` is exactly 192 000
// bytes of 4bpp indices and nothing else. That strictness is what makes the
// upload path cheap - the panel never has to trust a phone's JPEG, its EXIF
// orientation, or a 12-megapixel allocation it cannot afford. The browser does
// the conversion instead, in a canvas it already has, and posts a frame the
// firmware can hand straight to `display raw`.
//
// This module is the browser-side half of `tools/convert.py`, and the two must
// agree byte for byte: same resize, same Floyd-Steinberg diffusion, same
// 180-degree native layout. `tools/check-pf6-js.py` proves that by feeding both
// the same 800x480 RGB and comparing hashes.
//
// Pure: no canvas, no DOM, no Node APIs. `pack` takes plain RGBA bytes, so the
// page passes `ctx.getImageData(...).data` and the test harness passes a buffer
// it read from disk.

export const WIDTH = 800;
export const HEIGHT = 480;

/** Bytes in one raw frame: two 4bpp pixels per byte. */
export const FRAME_BYTES = (WIDTH * HEIGHT) / 2;

// The panel's exact six colours, in the order ties resolve in. Index 4 is gap
// in the controller's 4-bit encoding and must never be emitted.
export const PALETTE = [
  { index: 0, rgb: [0, 0, 0] },
  { index: 1, rgb: [255, 255, 255] },
  { index: 2, rgb: [255, 255, 0] },
  { index: 3, rgb: [255, 0, 0] },
  { index: 5, rgb: [0, 0, 255] },
  { index: 6, rgb: [0, 255, 0] },
];

function nearest(red, green, blue) {
  let best = PALETTE[0];
  let bestDistance = Infinity;
  for (const entry of PALETTE) {
    const [r, g, b] = entry.rgb;
    const distance = (red - r) ** 2 + (green - g) ** 2 + (blue - b) ** 2;
    if (distance < bestDistance) {
      bestDistance = distance;
      best = entry;
    }
  }
  return best;
}

// Python's `round` is half-to-even, `Math.round` is half-up. The two disagree
// exactly at .5, which error diffusion does produce, so the test would fail on
// a handful of pixels without this.
function roundHalfEven(value) {
  const floor = Math.floor(value);
  const fraction = value - floor;
  if (fraction > 0.5) return floor + 1;
  if (fraction < 0.5) return floor;
  return floor % 2 === 0 ? floor : floor + 1;
}

/**
 * How to map a source rectangle onto the panel.
 *
 * Mirrors `ImageOps.fit(..., centering=(0.5, 0.5))` in `convert.py`, which is a
 * *cover* fit: fill the panel, crop the overflow from the centre, never letterbox.
 * `contain` is offered for uploads where the whole picture matters more than a
 * full panel; the frame letterboxes the remainder in white.
 *
 * The returned rectangle is in source pixels, ready for
 * `ctx.drawImage(source, sx, sy, sw, sh, 0, 0, WIDTH, HEIGHT)`.
 */
export function fitRect(sourceWidth, sourceHeight, mode = "cover") {
  if (!(sourceWidth > 0 && sourceHeight > 0)) {
    throw new RangeError(`bad source size ${sourceWidth}x${sourceHeight}`);
  }
  const scale =
    mode === "contain"
      ? Math.min(WIDTH / sourceWidth, HEIGHT / sourceHeight)
      : Math.max(WIDTH / sourceWidth, HEIGHT / sourceHeight);
  const sw = Math.min(sourceWidth, WIDTH / scale);
  const sh = Math.min(sourceHeight, HEIGHT / scale);
  return {
    sx: (sourceWidth - sw) / 2,
    sy: (sourceHeight - sh) / 2,
    sw,
    sh,
  };
}

/**
 * Convert RGBA pixels at exactly WIDTH x HEIGHT into a raw frame.
 *
 * @param {Uint8ClampedArray|Uint8Array} rgba `WIDTH * HEIGHT * 4` bytes, row-major.
 * @param {{dither?: boolean}} [options] `dither: false` snaps each pixel to the
 *   nearest palette entry, which keeps flat graphics and text crisp; photographs
 *   want the default, because six colours without diffusion posterise badly.
 * @returns {Uint8Array} `FRAME_BYTES` bytes, 4bpp, 180-degree native order.
 */
export function pack(rgba, options = {}) {
  const dither = options.dither ?? true;
  const expected = WIDTH * HEIGHT * 4;
  if (rgba.length !== expected) {
    throw new RangeError(`need ${expected} RGBA bytes, got ${rgba.length}`);
  }

  // Diffusion needs fractional, signed channels, so the source is copied into
  // floats once. 800x480x3 of them is 4.6 MB - fine in a browser, and this is
  // why the conversion belongs there and not on the frame.
  const pixels = new Float32Array(WIDTH * HEIGHT * 3);
  for (let source = 0, target = 0; source < expected; source += 4, target += 3) {
    pixels[target] = rgba[source];
    pixels[target + 1] = rgba[source + 1];
    pixels[target + 2] = rgba[source + 2];
  }

  const frame = new Uint8Array(FRAME_BYTES);
  for (let y = 0; y < HEIGHT; y += 1) {
    for (let x = 0; x < WIDTH; x += 1) {
      const offset = (y * WIDTH + x) * 3;
      const red = pixels[offset];
      const green = pixels[offset + 1];
      const blue = pixels[offset + 2];
      // Clamp for the lookup only: the error is measured against the unclamped
      // value, which is what keeps a bright pixel from losing its overflow.
      const entry = nearest(
        Math.max(0, Math.min(255, roundHalfEven(red))),
        Math.max(0, Math.min(255, roundHalfEven(green))),
        Math.max(0, Math.min(255, roundHalfEven(blue))),
      );

      // Native order: the panel scans rotated 180 degrees.
      const nativeX = WIDTH - 1 - x;
      const nativeY = HEIGHT - 1 - y;
      const address = nativeY * (WIDTH / 2) + Math.floor(nativeX / 2);
      if (nativeX % 2 === 0) {
        frame[address] |= entry.index << 4;
      } else {
        frame[address] |= entry.index;
      }

      if (!dither) continue;
      const errorRed = red - entry.rgb[0];
      const errorGreen = green - entry.rgb[1];
      const errorBlue = blue - entry.rgb[2];
      const spread = (neighbour, factor) => {
        pixels[neighbour] += errorRed * factor;
        pixels[neighbour + 1] += errorGreen * factor;
        pixels[neighbour + 2] += errorBlue * factor;
      };
      // Floyd-Steinberg, same kernel and order as convert.py.
      if (x + 1 < WIDTH) spread(offset + 3, 7 / 16);
      if (y + 1 < HEIGHT) {
        const below = offset + WIDTH * 3;
        if (x > 0) spread(below - 3, 3 / 16);
        spread(below, 5 / 16);
        if (x + 1 < WIDTH) spread(below + 3, 1 / 16);
      }
    }
  }
  return frame;
}

/**
 * Draw `source`, fit it, and convert - the one call the upload page needs.
 *
 * @param {CanvasImageSource & {width: number, height: number}} source
 * @param {CanvasRenderingContext2D} context a canvas sized WIDTH x HEIGHT.
 */
export function packImage(source, context, options = {}) {
  const { sx, sy, sw, sh } = fitRect(
    source.width,
    source.height,
    options.mode ?? "cover",
  );
  context.save();
  context.fillStyle = "#ffffff";
  context.fillRect(0, 0, WIDTH, HEIGHT);
  if (options.mode === "contain") {
    const scale = Math.min(WIDTH / source.width, HEIGHT / source.height);
    const dw = source.width * scale;
    const dh = source.height * scale;
    context.drawImage(
      source,
      0,
      0,
      source.width,
      source.height,
      (WIDTH - dw) / 2,
      (HEIGHT - dh) / 2,
      dw,
      dh,
    );
  } else {
    context.drawImage(source, sx, sy, sw, sh, 0, 0, WIDTH, HEIGHT);
  }
  context.restore();
  const { data } = context.getImageData(0, 0, WIDTH, HEIGHT);
  return pack(data, options);
}

/** Palette entry for a 4-bit index, or `null` for the gap at 4. */
const BY_INDEX = new Map(PALETTE.map((entry) => [entry.index, entry.rgb]));

/**
 * Decode a raw frame back into RGBA at exactly WIDTH x HEIGHT.
 *
 * This is the inverse of `pack`, and the portal uses it to show the gallery as
 * the panel will draw it: same 180-degree rotation, same nibble order, same six
 * colours. A browser preview that re-interpreted the frame with its own idea of
 * the layout would look plausible and be wrong.
 *
 * @param {Uint8Array} frame `FRAME_BYTES` bytes.
 * @returns {Uint8ClampedArray} `WIDTH * HEIGHT * 4` bytes, ready for
 *   `new ImageData(rgba, WIDTH, HEIGHT)` or `putImageData`.
 */
export function unpack(frame) {
  if (frame.length !== FRAME_BYTES) {
    throw new RangeError(`need ${FRAME_BYTES} frame bytes, got ${frame.length}`);
  }
  const rgba = new Uint8ClampedArray(WIDTH * HEIGHT * 4);
  for (let y = 0; y < HEIGHT; y += 1) {
    for (let x = 0; x < WIDTH; x += 1) {
      const nativeX = WIDTH - 1 - x;
      const nativeY = HEIGHT - 1 - y;
      const byte = frame[nativeY * (WIDTH / 2) + Math.floor(nativeX / 2)];
      const index = nativeX % 2 === 0 ? byte >> 4 : byte & 0x0f;
      // Index 4 is unrepresentable; a corrupt byte shows as black rather than
      // throwing, because a gallery must render something for every file.
      const [r, g, b] = BY_INDEX.get(index) ?? [0, 0, 0];
      const offset = (y * WIDTH + x) * 4;
      rgba[offset] = r;
      rgba[offset + 1] = g;
      rgba[offset + 2] = b;
      rgba[offset + 3] = 255;
    }
  }
  return rgba;
}

/** Draw a raw frame onto a canvas context sized WIDTH x HEIGHT. */
export function drawFrame(context, frame) {
  context.putImageData(new ImageData(unpack(frame), WIDTH, HEIGHT), 0, 0);
}
