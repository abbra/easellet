// Check the portal's drop resolution against the shapes a browser really hands
// over, using the function from `web/index.html` itself rather than a copy.
//
// The bug this exists for: dragging a picture from another tab highlights the
// drop zone, and then nothing happens, because a cross-document drag carries a
// URL and `dataTransfer.files` is empty. That case is invisible to a person
// clicking a file picker, so it needs a test rather than a screenshot.
//
//   node tools/check-portal-js.mjs [path/to/index.html]

import { readFileSync, writeFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";

const page = process.argv[2] ?? new URL("../web/index.html", import.meta.url).pathname;
const html = readFileSync(page, "utf8");

/// The function as written, brace-balanced, so the page stays the only source.
function source(name) {
  const start = html.indexOf(`function ${name}(`);
  if (start < 0) throw new Error(`${name} not found in ${page}`);
  let depth = 0;
  for (let index = html.indexOf("{", start); index < html.length; index += 1) {
    if (html[index] === "{") depth += 1;
    if (html[index] === "}") depth -= 1;
    if (depth === 0) return html.slice(start, index + 1);
  }
  throw new Error(`${name} is not brace-balanced`);
}

const droppedPicture = new Function(`${source("droppedPicture")}; return droppedPicture;`)();

const picture = () => ({ type: "image/png", name: "from-disk.png" });
const itemPicture = () => ({ type: "image/jpeg", name: "from-clipboard.jpg" });

/// What Firefox builds for a drag from another tab: no files, just URLs.
const drag = ({ files = [], items = [], data = {} } = {}) => ({
  files,
  items,
  getData: (type) => data[type] ?? "",
});

const onDisk = picture();
const inClipboard = itemPicture();

const cases = [
  ["a file dragged from the desktop", drag({ files: [onDisk] }), onDisk],
  ["a picture listed only as a clipboard item", drag({ items: [{ kind: "file", type: "image/jpeg", getAsFile: () => inClipboard }] }), inClipboard],
  ["a drag from another tab (text/uri-list)", drag({ data: { "text/uri-list": "https://example.org/a.jpg\r\n" } }), "https://example.org/a.jpg"],
  ["a uri-list with a comment line first", drag({ data: { "text/uri-list": "# comment\r\nhttps://example.org/b.png" } }), "https://example.org/b.png"],
  ["a drag from another tab (text/html only)", drag({ data: { "text/html": '<img src="https://example.org/c.png" alt="">' } }), "https://example.org/c.png"],
  ["a plain-text URL", drag({ data: { "text/plain": "  https://example.org/d.webp " } }), "https://example.org/d.webp"],
  ["a data: URL", drag({ data: { "text/plain": "data:image/png;base64,AAAA" } }), "data:image/png;base64,AAAA"],
  ["text that is not a picture", drag({ data: { "text/plain": "привет" } }), null],
  ["an empty drag", drag(), null],
];

// The module must at least parse: a syntax error in the page is a blank portal,
// and no test below would ever get to run.
const module = html.match(/<script type="module">([\s\S]*?)<\/script>/);
if (!module) {
  console.log("FAIL the page has no module script");
  process.exit(1);
}
const scratch = join(tmpdir(), `portal-check-${process.pid}.mjs`);
writeFileSync(scratch, module[1]);
try {
  execFileSync(process.execPath, ["--check", scratch], { stdio: "pipe" });
  console.log("ok   the module parses");
} catch (error) {
  console.log(`FAIL the module does not parse: ${error.stderr?.toString().split("\n")[0] ?? error}`);
  process.exit(1);
}

let failures = 0;
for (const [name, data, expected] of cases) {
  const got = droppedPicture(data);
  const ok = got === expected;
  if (!ok) failures += 1;
  console.log(`${ok ? "ok  " : "FAIL"} ${name} -> ${got === null ? "null" : typeof got === "string" ? got : got.name}`);
  if (!ok) console.log(`     expected ${expected === null ? "null" : typeof expected === "string" ? expected : expected.name}`);
}

if (failures > 0) {
  console.log(`portal-js: ${failures} of ${cases.length} failed`);
  process.exit(1);
}
console.log(`portal-js: drop sources ok (${cases.length} cases)`);
