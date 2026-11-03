#!/usr/bin/env python3
"""Start the real bridge and ask it the questions a person would.

`bridge/build.sh test` exercises `handle()` and never binds a socket, so a route
that the docs describe and the table does not have was only ever noticed by a
person with a browser. This starts the built binary on `127.0.0.1:0` with a
throwaway settings file, checks the pages and the JSON the web interface reads,
and stops it.

    bridge/build.sh test                 # builds bridge/target/debug/easellet-bridge
    tools/bridge-smoke.py                # the default binary
    tools/bridge-smoke.py --bin path/to/easellet-bridge

Nothing here reaches the network beyond the loopback socket, and the settings
file is temporary, so a run cannot change the machine's bridge configuration.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BIN = ROOT / "bridge/target/debug/easellet-bridge"
LISTENING = re.compile(r"listening on http://([^ ]+)")


def die(message: str) -> None:
    print(f"ERR {message}", file=sys.stderr)
    sys.exit(1)


def get(base: str, path: str) -> tuple[int, str, dict[str, str]]:
    request = urllib.request.Request(f"{base}{path}")
    try:
        with urllib.request.urlopen(request, timeout=20) as answer:
            return answer.status, answer.read().decode("utf-8", "replace"), dict(answer.headers)
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode("utf-8", "replace"), dict(error.headers)


def post(
    base: str, path: str, body: bytes, kind: str = "application/json", portal: bool = True
) -> int:
    """POST as the web interface does; `portal=False` omits its header on purpose.

    The bridge refuses a POST without `X-Easellet` for the same reason the frame
    does: the header cannot be sent by a plain cross-site form.
    """
    headers = {"Content-Type": kind, **({"X-Easellet": "1"} if portal else {})}
    request = urllib.request.Request(f"{base}{path}", data=body, method="POST", headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=20) as answer:
            return answer.status
    except urllib.error.HTTPError as error:
        return error.code


def plugin_names(base: str) -> list[str]:
    status, text, _ = get(base, "/api/plugins")
    if status != 200:
        die(f"GET /api/plugins returned {status}: {text.strip()[:200]}")
    document = json.loads(text)
    if document.get("problems"):
        die(f"plugin discovery reported problems: {document['problems']}")
    return sorted(plugin["name"] for plugin in document["plugins"])


def expected_plugins() -> list[str]:
    names = []
    for manifest in sorted((ROOT / "bridge/plugins").glob("*/plugin.toml")):
        match = re.search(r'^name\s*=\s*"([^"]+)"', manifest.read_text(encoding="utf-8"), re.M)
        if match:
            names.append(match.group(1))
    return sorted(names)


def checks(base: str) -> list[str]:
    failures = []

    def check(condition: bool, what: str) -> None:
        print(f"{'ok  ' if condition else 'FAIL'} {what}")
        if not condition:
            failures.append(what)

    status, page, headers = get(base, "/")
    check(status == 200 and "<html" in page.lower(), f"GET / {status}")
    check(headers.get("Content-Type", "").startswith("text/html"), "GET / is HTML")

    status, script, _ = get(base, "/app.js")
    check(status == 200 and "function" in script, f"GET /app.js {status}")

    status, health, _ = get(base, "/health")
    check(status == 200 and health.strip() != "", f"GET /health {status}")

    for path, key in (("/api/status", None), ("/api/frames", "frames"),
                      ("/api/screens", "screens"), ("/api/jobs", "jobs"),
                      ("/api/settings", None), ("/api/opendisplay", None),
                      ("/api/firmware", "store")):
        status, text, _ = get(base, path)
        parsed = None
        try:
            parsed = json.loads(text)
        except json.JSONDecodeError:
            pass
        wanted = key is None or (isinstance(parsed, dict) and key in parsed)
        check(status == 200 and wanted, f"GET {path} {status}")

    # Plugin discovery is the one route whose answer depends on the filesystem
    # layout next to the binary, which is exactly what a packaging change breaks.
    found = plugin_names(base)
    wanted = expected_plugins()
    missing = sorted(set(wanted) - set(found))
    check(found == wanted, f"GET /api/plugins {len(found)} of {len(wanted)} {missing or ''}".strip())

    check(get(base, "/api/nope")[0] == 404, "GET /api/nope is 404")
    check(post(base, "/api/jobs/save", b"not json", portal=False) == 403,
          "a POST without X-Easellet is refused")
    check(post(base, "/api/jobs/save", b"not json") == 400, "POST /api/jobs/save rejects junk")
    check(post(base, "/api/frames/remove", b"{}") == 400, "POST /api/frames/remove needs a frame")
    check(post(base, "/api/screens/delete", b"{}") == 400, "POST /api/screens/delete needs a frame")
    # A bridge with no `firmware_repo` has nothing to fetch, and says so rather
    # than reaching for GitHub.
    check(post(base, "/api/firmware/refresh", b"{}") == 409,
          "POST /api/firmware/refresh without a repo is 409")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bin", default=str(DEFAULT_BIN), help="the bridge binary")
    arguments = parser.parse_args()
    binary = Path(arguments.bin)
    if not binary.is_file():
        die(f"{binary} is not built; run bridge/build.sh test first")

    with tempfile.TemporaryDirectory(prefix="easellet-smoke-") as work:
        child = subprocess.Popen(
            [str(binary), "--addr", "127.0.0.1:0", "--config", str(Path(work) / "bridge.toml")],
            cwd=ROOT,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            base = ""
            for line in child.stderr:
                match = LISTENING.search(line)
                if match:
                    base = f"http://{match.group(1)}"
                    break
                print(f"bridge: {line.rstrip()}")
            if not base:
                die("the bridge never said where it listens")
            print(f"bridge listening on {base}")
            failures = checks(base)
        finally:
            child.terminate()
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                child.kill()

    if failures:
        print(f"ERR {len(failures)} smoke check(s) failed", file=sys.stderr)
        return 1
    print("bridge smoke: passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
