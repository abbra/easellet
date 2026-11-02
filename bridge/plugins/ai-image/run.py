#!/usr/bin/env python3
"""A generated picture from an OpenAI-compatible images endpoint."""

import base64
import json
import sys
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from easellet_plugin import Plugin  # noqa: E402

MAX_IMAGE = 20 * 1024 * 1024

plugin = Plugin.read()
request = {"prompt": plugin.params["prompt"], "n": 1, "size": plugin.params.get("size", "1280x768"),
           "response_format": "b64_json"}
if plugin.params.get("model"):
    request["model"] = plugin.params["model"]
headers = {"Content-Type": "application/json", "User-Agent": "easellet-bridge"}
if plugin.params.get("api_key"):
    headers["Authorization"] = f"Bearer {plugin.params['api_key']}"
try:
    call = urllib.request.Request(plugin.params["endpoint"], data=json.dumps(request).encode(), headers=headers)
    with urllib.request.urlopen(call, timeout=100) as answer:
        data = json.load(answer)["data"][0]
    if data.get("b64_json"):
        picture = base64.b64decode(data["b64_json"])
    else:
        with urllib.request.urlopen(data["url"], timeout=30) as answer:
            picture = answer.read(MAX_IMAGE)
except Exception as error:  # noqa: BLE001
    plugin.fail(f"ai-image: {error}")
path = Path(plugin.scratch) / "generated.png"
path.write_bytes(picture)
plugin.image(str(path), fit=plugin.params.get("fit", "cover"))
