// Run tools/portal-harness.html in a real browser and report what it found.
//
// The harness drives the portal in a same-origin iframe and writes a list of
// checks into #log, one line each; it sets document.title to "done" when the
// last check has run. The 22 checks it makes are the ones a person has been
// running by hand: the default language, the gallery pages, a drop of a URL from
// another tab, the confirmation window, the settings that come back from the
// frame. Nothing but a browser exercises them.
//
//   npm --prefix tools ci            # once; installs puppeteer-core
//   node tools/run-portal-harness.mjs
//
// EASEL_CHROMIUM names the browser binary; otherwise the usual system names are
// tried, which is how this runs in the Fedora CI image without a bundled
// browser download.
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import puppeteer from "puppeteer-core";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = dirname(HERE);
const CANDIDATES = [
  process.env.EASEL_CHROMIUM,
  "/usr/bin/chromium",
  "/usr/bin/chromium-browser",
  "/usr/bin/google-chrome",
  "/usr/bin/google-chrome-stable",
].filter(Boolean);

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function browserPath() {
  const found = CANDIDATES.find((path) => existsSync(path));
  if (!found) {
    console.error(`ERR no browser; tried ${CANDIDATES.join(", ")} (set EASEL_CHROMIUM)`);
    process.exit(2);
  }
  return found;
}

// The mock prints the address it actually bound, which matters when the port is
// 0: CI must not collide with anything on the machine.
function startMock() {
  const child = spawn("python3", [join(HERE, "portal-mock.py"), "--port", "0"], {
    cwd: ROOT,
    stdio: ["ignore", "pipe", "inherit"],
  });
  return new Promise((resolve, reject) => {
    let output = "";
    const timer = setTimeout(() => reject(new Error(`portal mock said nothing: ${output}`)), 20000);
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      output += chunk;
      const match = output.match(/portal mock: (http:\/\/\S+?)\/ with/);
      if (match) {
        clearTimeout(timer);
        resolve({ child, url: match[1] });
      }
    });
    child.on("exit", (code) => {
      clearTimeout(timer);
      reject(new Error(`portal mock exited with ${code}`));
    });
  });
}

async function main() {
  const { child, url } = await startMock();
  const browser = await puppeteer.launch({
    executablePath: browserPath(),
    // The container runs as root and has a small /dev/shm, both of which stop
    // Chromium before the page loads.
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
    defaultViewport: { width: 1280, height: 1024 },
  });
  let failed = 0;
  let checks = 0;
  try {
    const page = await browser.newPage();
    page.on("pageerror", (error) => console.error(`page error: ${error.message}`));
    // Every /harness.html navigation is the same page; a second load happens
    // inside the iframe, not here, so this waits for the title instead of an
    // event.
    await page.goto(`${url}/harness.html`, { waitUntil: "domcontentloaded", timeout: 60000 });
    const deadline = Date.now() + 120000;
    while (Date.now() < deadline) {
      if ((await page.title()) === "done") break;
      await sleep(250);
    }
    const lines = await page.$$eval("#log li", (items) =>
      items.map((item) => ({ ok: !item.classList.contains("bad"), text: item.textContent }))
    );
    checks = lines.length;
    for (const line of lines) {
      console.log(line.text);
      if (!line.ok) failed += 1;
    }
    if ((await page.title()) !== "done") {
      console.error("ERR the harness did not finish; the last line above is where it stopped");
      failed += 1;
    }
    if (lines.length === 0) {
      console.error("ERR the harness reported nothing");
      failed += 1;
    }
  } finally {
    await browser.close().catch(() => {});
    child.kill();
  }
  if (failed) {
    console.error(`ERR ${failed} portal check(s) failed`);
    process.exit(1);
  }
  console.log(`portal harness: ${checks} checks passed`);
}

main().catch((error) => {
  console.error(`ERR ${error.message}`);
  process.exit(1);
});
