#!/usr/bin/env python3
"""A note: the title and text exactly as given."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from photoframe_plugin import Plugin  # noqa: E402

plugin = Plugin.read()
plugin.notice(plugin.params["title"], plugin.params.get("text", ""), accent=plugin.params.get("accent", "blue"))
