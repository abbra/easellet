#!/usr/bin/env node
// Node CLI half of `tools/el6.mjs`, used only by `tools/check-el6-js.py`.
//
//   node tools/el6-check.mjs <rgba-in> <el6-out> [--no-dither]
//
// Reads WIDTH*HEIGHT*4 raw RGBA bytes and writes a 192000-byte frame. It exists
// so the browser module can be tested without a browser: the page and this
// script call the same `pack`.

import { readFileSync, writeFileSync } from "node:fs";
import { FRAME_BYTES, HEIGHT, WIDTH, pack, unpack } from "./el6.mjs";

const [input, output, ...flags] = process.argv.slice(2);
if (!input || !output) {
  console.error("usage: node tools/el6-check.mjs <rgba-in> <el6-out> [--no-dither]");
  process.exit(2);
}

const rgba = readFileSync(input);
if (rgba.length !== WIDTH * HEIGHT * 4) {
  console.error(
    `need ${WIDTH * HEIGHT * 4} RGBA bytes, ${input} has ${rgba.length}`,
  );
  process.exit(2);
}

const frame = pack(rgba, { dither: !flags.includes("--no-dither") });
if (frame.length !== FRAME_BYTES) {
  console.error(`internal error: frame is ${frame.length} bytes`);
  process.exit(1);
}
// Round trip: decoding the frame and re-packing it with diffusion off must
// reproduce it exactly, because every decoded colour *is* a palette entry. A
// wrong rotation or nibble order cannot survive that, which is what makes this
// a test of `unpack` rather than a restatement of it.
const roundTrip = Buffer.compare(Buffer.from(pack(unpack(frame), { dither: false })), Buffer.from(frame)) === 0;

writeFileSync(output, frame);
console.log(`roundtrip=${roundTrip ? "ok" : "FAIL"}`);
console.log(`${WIDTH}x${HEIGHT} ${frame.length} bytes -> ${output}`);
