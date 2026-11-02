#!/usr/bin/env python3
"""Greeting card: the hello-world of photoframe-bridge plugins."""

import sys
from datetime import datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from photoframe_plugin import Plugin  # noqa: E402

plugin = Plugin.read()
now = datetime.now().strftime("%H:%M")
runs = plugin.state.get("runs", 0) + 1
plugin.notice(
    f"Привет, {plugin.params.get('name', 'мир')}!",
    f"Сейчас {now}. Эта карточка пришла от плагина «hello» (показ № {runs}).",
    accent=plugin.params.get("accent", "green"),
    state={"runs": runs},
)
