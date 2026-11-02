#!/usr/bin/env python3
"""Headlines from an RSS 2.0 or Atom feed."""

import hashlib
import sys
import urllib.request
import xml.etree.ElementTree as ElementTree
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from photoframe_plugin import Plugin  # noqa: E402

ATOM = "{http://www.w3.org/2005/Atom}"
MAX_FEED = 2 * 1024 * 1024

plugin = Plugin.read()
count = int(plugin.params.get("count", "6"))
try:
    request = urllib.request.Request(plugin.params["url"], headers={"User-Agent": "photoframe-bridge"})
    with urllib.request.urlopen(request, timeout=10) as answer:
        root = ElementTree.fromstring(answer.read(MAX_FEED))
except Exception as error:  # noqa: BLE001
    plugin.fail(f"news: {error}")

if root.tag == f"{ATOM}feed":
    feed_title = root.findtext(f"{ATOM}title", "")
    titles = [entry.findtext(f"{ATOM}title", "") for entry in root.findall(f"{ATOM}entry")]
else:
    channel = root.find("channel")
    if channel is None:
        plugin.fail("news: neither RSS nor Atom")
    feed_title = channel.findtext("title", "")
    titles = [item.findtext("title", "") for item in channel.findall("item")]
titles = [" ".join(title.split()) for title in titles if title and title.strip()][:count]
if not titles:
    plugin.fail("news: the feed has no headlines")

# Same headlines as last time: no new picture, no refresh.
digest = hashlib.sha256("\n".join(titles).encode()).hexdigest()
if plugin.state.get("digest") == digest:
    plugin.nothing(state=plugin.state)
plugin.items(plugin.params.get("title") or feed_title or "Новости", titles, accent="red", state={"digest": digest})
