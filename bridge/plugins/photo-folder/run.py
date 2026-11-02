#!/usr/bin/env python3
"""The next photograph from a folder on the bridge host."""

import random
import shutil
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from easellet_plugin import Plugin  # noqa: E402

SUFFIXES = {".jpg", ".jpeg", ".png"}

plugin = Plugin.read()
folder = Path(plugin.params["folder"]).expanduser()
if not folder.is_absolute() or not folder.is_dir():
    plugin.fail(f"photo-folder: {folder} is not a folder (give a full path)")
pictures = sorted(path for path in folder.iterdir() if path.suffix.lower() in SUFFIXES and path.is_file())
if not pictures:
    plugin.fail(f"photo-folder: no JPEG or PNG in {folder}")
last = plugin.state.get("last")
if plugin.params.get("order") == "random" and len(pictures) > 1:
    choice = random.choice([path for path in pictures if path.name != last])
else:
    following = [path for path in pictures if last is None or path.name > last]
    choice = following[0] if following else pictures[0]
# The bridge reads pictures only from the run's scratch directory.
copy = Path(plugin.scratch) / f"picture{choice.suffix.lower()}"
shutil.copyfile(choice, copy)
plugin.image(str(copy), fit=plugin.params.get("fit", "cover"), state={"last": choice.name})
