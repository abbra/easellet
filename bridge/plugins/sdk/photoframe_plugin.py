"""Helpers for photoframe-bridge plugins written in Python.

A plugin reads one JSON document on stdin and writes one on stdout. This module
does both and gives the output types names, so a plugin is a few lines:

    from photoframe_plugin import Plugin

    plugin = Plugin.read()
    plugin.notice("Привет", f"Привет, {plugin.params['name']}!")

Standard library only: plugins run with a cleared environment and no virtualenv.
See bridge/plugins/README.md for the whole contract.
"""

import json
import sys

API = 1


class Plugin:
    """The input of one run, and the ways to answer it."""

    def __init__(self, document):
        self.document = document
        self.params = document.get("params", {})
        self.frame = document.get("frame", {})
        self.locale = document.get("locale", "ru-RU")
        self.timezone = document.get("timezone", "UTC")
        self.now = document.get("now")
        self.scratch = document.get("scratch")
        self.state = document.get("state") or {}

    @classmethod
    def read(cls):
        return cls(json.load(sys.stdin))

    def emit(self, output, state=None, next_run_s=None, say=None):
        """Write the answer and stop. `state` comes back as `self.state` next run;
        `say` is what the voice assistant speaks when a question ran the plugin."""
        answer = {"api": API, "output": output}
        if say:
            answer["say"] = say
        if state is not None:
            answer["state"] = state
        if next_run_s is not None:
            answer["next_run_s"] = int(next_run_s)
        json.dump(answer, sys.stdout, ensure_ascii=False)
        sys.stdout.flush()
        sys.exit(0)

    def notice(self, title, body="", accent="blue", **kwargs):
        """A title and a paragraph. Accents: blue, red, green, yellow, black."""
        self.emit({"type": "card", "layout": "notice", "title": title, "body": body, "accent": accent}, **kwargs)

    def items(self, title, items, accent="blue", **kwargs):
        """A title and a list. Each item is text, (text, status) with status one
        of ok, fail, warn, info, or a dict with "text" and optional "status" and
        "counts" ({"fail", "running", "ok", "other"}, drawn at the row's end)."""
        rows = []
        for item in items:
            if isinstance(item, str):
                rows.append({"text": item})
            elif isinstance(item, dict):
                rows.append(item)
            else:
                text, status = item
                rows.append({"text": text, "status": status})
        self.emit({"type": "card", "layout": "list", "title": title, "items": rows, "accent": accent}, **kwargs)

    def image(self, path, fit="cover", **kwargs):
        """A PNG or JPEG the plugin wrote into `self.scratch`."""
        self.emit({"type": "image", "path": path, "fit": fit}, **kwargs)

    def nothing(self, **kwargs):
        """Nothing new: the frame keeps what it shows and is not refreshed."""
        self.emit({"type": "none"}, **kwargs)

    def fail(self, message):
        """Give up with a message for the run log; the frame keeps its picture."""
        print(message, file=sys.stderr)
        sys.exit(1)
