#!/usr/bin/env python3
"""A stand-in for the frame's HTTP portal, for working on the page without the board.

The page is served from `web/index.html` and the converter from `tools/el6.mjs`,
so the browser runs exactly the files the firmware would embed; only the API is
faked. Every route answers with the same shape and the same status codes as
`src/web.rs`, including the refusal to delete anything outside `EASELLET`, so a
page that works here works on the frame.

    tools/portal-mock.py --count 60 --port 8130
    firefox http://127.0.0.1:8130/

Pictures are generated, not read from a card: enough of them to page through and
to check that the gallery does not try to decode the whole collection at once.
"""

from __future__ import annotations

import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, unquote, urlparse

import sys

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))

from convert import HEIGHT, PALETTE, WIDTH, frame_from_rgb  # noqa: E402

FRAME_BYTES = WIDTH * HEIGHT // 2
WRITABLE = "EASELLET"


def synthetic(index: int, total: int) -> bytes:
    """A frame that is obvious at a glance: two palette blocks and a band whose
    position follows the index, so paging is visibly working."""
    colours = [entry[1] for entry in PALETTE]
    left = colours[index % len(colours)]
    right = colours[(index + 2) % len(colours)]
    band = colours[(index + 4) % len(colours)]
    row = bytes(left) * (WIDTH // 2) + bytes(right) * (WIDTH // 2)
    stripe = bytes(band) * WIDTH
    top = int(HEIGHT * index / max(1, total))
    rows = [stripe if top <= y < top + 24 else row for y in range(HEIGHT)]
    return frame_from_rgb(b"".join(rows))


class State:
    """Names are the state; a frame is ground on first request and kept, so a
    sixty-picture gallery opens instantly instead of dithering at boot."""

    def __init__(self, count: int) -> None:
        self.directory = f"{WRITABLE}/UPLOADS"
        self.total = count
        self.uploaded: dict[str, bytes] = {}
        self.ground: dict[str, bytes] = {}
        self.names = [f"{self.directory}/GEN{index:04}.EL6" for index in range(count)]
        # Two pictures from the vendor's own directories: readable, and the page
        # must not offer to delete them, because the firmware would refuse.
        self.vendor = ["01_SYS~1/00_INIT.BMP", "06_USE~1/800X480.BMP"]
        self.next_upload = count + 1
        # The same document `/ota` serves from the firmware: two 3 MB slots, the
        # frame booted from the first, the second empty.
        self.firmware = {
            "version": "0.1.0",
            "running": "ota_0",
            "state": "valid",
            "other": "ota_1",
            "other_version": None,
            "max_bytes": 0x300000,
        }
        # What a POST /ota received, so the harness can see the page sent it.
        self.installed: bytes | None = None
        # The same document `/settings` serves from the firmware.
        self.settings = {
            "wifi": {"ssid": "home", "password_set": True, "auto": True},
            "audio": {"volume_db": "-6.0", "gain_db": "30.0"},
            "ai": {
                "url": "http://192.168.50.93:8095",
                "key_set": True,
                "lang": "ru-RU",
                "idle": "voice",
                "threshold": 150,
            },
            "gallery": {"dir": self.directory, "secs": 300, "auto": True},
            "web": {"password_set": False},
        }

    def frame(self, name: str) -> bytes | None:
        if name in self.uploaded:
            return self.uploaded[name]
        if name in self.names:
            seed = self.names.index(name)
        elif name in self.vendor:
            seed = 1000 + self.vendor.index(name)
        else:
            return None
        if name not in self.ground:
            self.ground[name] = synthetic(seed, self.total)
        return self.ground[name]

    def exists(self, name: str) -> bool:
        """Whether the card holds this picture, without grinding its frame.

        `frame()` dithers on first use, which costs about two seconds in Python;
        asking a delete or a show whether a name is there must not pay that, or
        ten deletions take twenty seconds and look like a hung page.
        """
        return name in self.uploaded or name in self.names or name in self.vendor

    def count(self) -> int:
        return len(self.names) + len(self.uploaded) + len(self.vendor)

    def remove(self, name: str) -> None:
        self.uploaded.pop(name, None)
        self.ground.pop(name, None)
        if name in self.names:
            self.names.remove(name)

    def apply(self, fields: dict[str, str]) -> None:
        """Store what the form sent, the way `Settings::parse` would.

        The shape of the document is kept: a value keeps the type of the value it
        replaces, so `ai.threshold=200` stays a number and `ai.lang=de-DE` stays a
        string. Without this the page could not be tested against a setting it has
        just saved.
        """
        for key, value in fields.items():
            section, _, name = key.partition(".")
            bucket = self.settings.get(section)
            if not isinstance(bucket, dict) or not name:
                continue
            if key == "ai.key":
                bucket["key_set"] = bool(value)
                continue
            old = bucket.get(name)
            if isinstance(old, bool):
                bucket[name] = value not in ("", "0", "no", "off")
            elif isinstance(old, int):
                try:
                    bucket[name] = int(float(value))
                except ValueError:
                    continue
            else:
                bucket[name] = value



class Handler(BaseHTTPRequestHandler):
    state: State
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args) -> None:  # noqa: A002
        pass

    # --- helpers ---------------------------------------------------------

    def body(self, code: int, text: str, kind: str = "text/plain; charset=utf-8") -> None:
        payload = text.encode()
        self.send_response(code)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    def binary(self, code: int, payload: bytes, kind: str) -> None:
        self.send_response(code)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    def query(self) -> dict[str, str]:
        parsed = urlparse(self.path)
        return {key: values[0] for key, values in parse_qs(parsed.query).items()}

    def route(self) -> str:
        return urlparse(self.path).path

    def list_images(self) -> list[dict[str, object]]:
        names = sorted(self.state.names + list(self.state.uploaded) + self.state.vendor)
        # Every frame is the same size; asking for the bytes here would grind the
        # whole collection just to draw the list.
        return [
            {"name": name, "bytes": len(self.state.uploaded.get(name, b"")) or FRAME_BYTES}
            for name in names
        ]

    # --- verbs -----------------------------------------------------------

    def do_GET(self) -> None:  # noqa: N802
        route = self.route()
        if route in ("/", "/index.html"):
            return self.binary(200, (ROOT / "web/index.html").read_bytes(), "text/html; charset=utf-8")
        if route == "/harness.html":
            # The interaction harness lives beside this mock, never in `web/`:
            # it is a development tool, not something the frame serves.
            return self.binary(
                200, (ROOT / "tools/portal-harness.html").read_bytes(), "text/html; charset=utf-8"
            )
        if route == "/el6.mjs":
            return self.binary(200, (ROOT / "tools/el6.mjs").read_bytes(), "text/javascript; charset=utf-8")
        if route == "/status":
            return self.body(
                200,
                f"OK easellet-upload frame_bytes={FRAME_BYTES} "
                "routes=/ /el6.mjs /gallery /image /upload /show /delete /settings /sensors /ota /status\n",
            )
        if route == "/ota":
            return self.body(200, json.dumps(self.state.firmware), "application/json")
        if route == "/gallery":
            return self.body(
                200,
                json.dumps(
                    {
                        "dir": self.state.directory,
                        "frame_bytes": FRAME_BYTES,
                        "images": self.list_images(),
                    }
                ),
                "application/json",
            )
        if route == "/image":
            frame = self.state.frame(self.query().get("path", ""))
            if frame is None:
                return self.body(404, "ERR no such picture\n")
            return self.binary(200, frame, "application/octet-stream")
        if route == "/settings":
            return self.body(200, json.dumps(self.state.settings), "application/json")
        if route == "/sensors":
            return self.body(
                200,
                json.dumps(
                    {
                        "battery": {"present": True, "usb": False, "charging": False,
                                    "mv": 3912, "percent": 64, "low": False},
                        "ambient": {"temp_c": "22.40", "rh_pct": 41.5},
                        "chip_c": 31.0,
                        "age_s": 4,
                    }
                ),
                "application/json",
            )
        return self.body(404, "ERR not here\n")

    def do_POST(self) -> None:  # noqa: N802
        route = self.route()
        length = int(self.headers.get("Content-Length") or 0)
        payload = self.rfile.read(length) if length else b""
        # Same rule as the firmware: a POST without the portal's header may come
        # from another site's page, and is refused.
        if self.headers.get("X-Easellet") is None:
            return self.body(403, "ERR POST needs the X-Easellet header the portal page sends\n")

        if route == "/upload":
            if length != FRAME_BYTES:
                return self.body(413, f"ERR need exactly {FRAME_BYTES} bytes, got {length}\n")
            name = f"{self.state.directory}/UP{self.state.next_upload:04}.EL6"
            self.state.next_upload += 1
            self.state.uploaded[name] = payload
            return self.body(
                200,
                f"OK saved={name} bytes={len(payload)} paint=started\n",
            )
        if route == "/ota":
            # The same three checks the firmware makes before writing flash: a
            # body was announced, it fits the slot, and it starts like an image.
            if self.headers.get("Content-Length") is None:
                return self.body(411, "ERR send Content-Length: the size of the .bin\n")
            if len(payload) > self.state.firmware["max_bytes"]:
                return self.body(
                    413, f"ERR firmware: the app slot holds {self.state.firmware['max_bytes']} bytes\n"
                )
            if not payload.startswith(b"\xe9"):
                return self.body(
                    400, "ERR firmware: that is not an app image (it does not start with 0xE9)\n"
                )
            self.state.installed = payload
            self.state.firmware["other_version"] = "uploaded"
            return self.body(200, "OK firmware %d bytes written to ota_1; rebooting\n" % len(payload))
        if route == "/delete":
            path = self.query().get("path", "")
            return self.delete(path)
        if route == "/show":
            path = self.query().get("path", "")
            if not self.state.exists(path):
                return self.body(403, f"ERR {path}: no such picture\n")
            return self.body(200, f"OK show={path} paint=started (about 30 s)\n")
        if route == "/settings":
            fields = {
                key: values[0]
                for key, values in parse_qs(payload.decode(), keep_blank_values=True).items()
            }
            self.state.apply(fields)
            applied = ", ".join(sorted(fields)) or "nothing"
            return self.body(200, f"OK saved: {applied}\n")
        return self.body(404, "ERR unknown upload target\n")

    def delete(self, path: str) -> None:
        # The same two rules as the firmware: only the two picture formats, and
        # only inside the writable directory.
        if not path.lower().endswith((".el6", ".bmp")):
            return self.body(403, f"ERR {path}: only .el6 and .bmp pictures can be removed\n")
        if not path.lower().startswith(f"{WRITABLE.lower()}/"):
            return self.body(
                403,
                f"ERR {path}: only files inside {WRITABLE} can be removed; the rest of the card is the vendor's\n",
            )
        if not self.state.exists(path):
            return self.body(403, f"ERR remove {path}: no such file\n")
        self.state.remove(path)
        return self.body(200, f"OK deleted={path} images={self.state.count()}\n")


def main() -> None:
    parser = argparse.ArgumentParser(description="Serve the portal against generated pictures.")
    parser.add_argument("--port", type=int, default=8130)
    parser.add_argument("--count", type=int, default=60, help="how many pictures to invent")
    parser.add_argument("--host", default="127.0.0.1")
    arguments = parser.parse_args()

    Handler.state = State(arguments.count)

    class Server(ThreadingHTTPServer):
        # The page opens a handful of requests at once while it fills a gallery
        # page; the default backlog of five drops some of them, which looks like
        # a page bug and is not one.
        request_queue_size = 64
        daemon_threads = True

    server = Server((arguments.host, arguments.port), Handler)
    # The bound address, not the requested one: `--port 0` picks a free port, and
    # tools/run-portal-harness.mjs reads this line to find it.
    host, port = server.server_address[0], server.server_address[1]
    print(f"portal mock: http://{host}:{port}/ with {arguments.count} pictures", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
