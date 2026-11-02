# Writing a plugin for photoframe-bridge

A plugin is a program that decides **what** a frame should show; the bridge
decides **how** (size, colours, orientation, dithering) for each frame, and when
to send it. Any language works. Design: `research/bridge-ui-design.md` section 5.

## Layout

```
~/.config/photoframe-bridge/plugins/<name>/   your plugins (checked first)
bridge/plugins/<name>/                        the ones shipped with the bridge
    plugin.toml    the manifest
    run…           the program, executable, path given in the manifest
```

`<name>` is `[a-z0-9-]`, at most 32 characters, and must equal `name` in the
manifest. A plugin in your directory replaces a shipped one of the same name.

## The manifest

```toml
name = "weather"
title = "Погода"                 # what the web UI shows
description = "Прогноз на 4 дня."
run = "run.py"                   # relative to the plugin directory
timeout_s = 20                   # 1..120; the run is killed after it
outputs = ["card", "none"]       # what it may return (empty = anything)
default_every = "3h"             # a suggestion for schedules

[[param]]                        # one table per parameter; the UI builds a form
id = "place"
type = "string"                  # string text number bool choice secret location url
title = "Место"
required = true
default = "Helsinki"             # optional
choices = []                     # for type = "choice"
note = "Город или «широта,долгота»."
```

`secret` parameters are stored by the bridge and never sent back to a browser.
`hidden = true` keeps a parameter out of the web form; it is for one the
assistant fills (`text_param`).

### Voice

```toml
[assistant]
intent = "показать погоду"                      # the main phrase
phrases = ["покажи погоду", "какая погода"]     # other ways people say it
text_param = "prompt"                           # optional: gets the whole question
description = "Show the weather forecast on the frame."
```

When a frame's question contains every word of one of these phrases (compared
by stem, so «покажи погоду на завтра» matches «покажи погоду»), the bridge runs
the plugin for that frame instead of asking the model and puts the result on the
panel when the frame has finished speaking. The frame says the plugin's `say`
(below) if the run ends within 8 s, else «Показываю: <title>». It uses
the parameters of a saved job for that plugin and frame, else the defaults.
Russian imperatives and infinitives differ past the stem, so list the forms
people actually say.

## One run

The bridge starts the program in its directory with a cleared environment
(`PATH`, `HOME`, `TZ`, proxy and CA variables, `LANG=C.UTF-8`,
`PF_PLUGIN_SCRATCH`) and writes one JSON document to its stdin:

```json
{
  "api": 1,
  "params": {"place": "Helsinki"},
  "frame": {"id": "94a990caed46", "name": "ODCAED44", "width": 800, "height": 480,
            "orientation": "landscape",
            "palette": ["black", "white", "yellow", "red", "blue", "green"]},
  "locale": "ru-RU", "timezone": "Europe/Helsinki", "now": "2026-11-02T10:00:00Z",
  "scratch": "/tmp/photoframe-bridge-…",
  "state": null
}
```

It answers with one JSON document on stdout; stderr goes to the bridge's log.

```json
{"api": 1,
 "output": {"type": "card", "layout": "list", "title": "CI",
            "items": [{"text": "main", "status": "ok"}]},
 "state": {"etag": "…"},
 "next_run_s": 900,
 "say": "Показываю погоду: Эспоо."}
```

`say` is optional: what the frame speaks when a voice question ran the plugin.
Use it to name what is shown, and to be honest when the request could not be
met as asked («Не нашёл место «ВСПУ». Показываю погоду: Хельсинки.»).

| `output.type` | fields |
| --- | --- |
| `card` | `layout` and its fields, below |
| `text` | `title`, `text`: shown as a `notice` card |
| `image` | `path` (a PNG or JPEG inside `scratch`) or `base64`; `fit`: `cover` (default) or `contain` |
| `none` | nothing new: the frame keeps its picture and is not refreshed |

Card layouts:

- `notice`: `title`, `body`, `accent` (`blue` `red` `green` `yellow` `black`).
- `list`: `title`, `items` (`[{"text": …, "status": "ok|fail|warn|info",
  "counts": {"fail": 6, "running": 2, "ok": 35, "other": 1}}]`), `accent`.
  `counts` is optional and drawn at the row's right end as coloured squares
  with numbers (red, yellow, green, black; zeros are left out); the text is cut
  to make room.
- `forecast`: `title`, `days` (`[{"label": "Пн", "icon": …, "high": 5, "low": -1,
  "precip_mm": 0.4}]`, up to 6), `note`, `accent`. Icons: `clear` `partly`
  `cloudy` `fog` `drizzle` `rain` `snow` `thunder`.

## Shipped plugins

| Plugin | Shows | Needs |
| --- | --- | --- |
| `weather` | forecast for a place; by voice, for the place named («…погоду в Эспоо», "…weather in Espoo") | nothing (Open-Meteo) |
| `news` | headlines of an RSS/Atom feed | a feed URL |
| `ci` | the latest open PRs of a GitHub repository, each with how many checks failed, run and passed (Actions and external CI) | a read-only token (see below) |
| `note` | a title and text | — |
| `photo-folder` | photos from a folder on this host, in turn | a folder |
| `ai-image` | a generated picture | an OpenAI-compatible images endpoint |
| `hello`, `system` | examples | — |

`ci` counts both kinds of CI reports on a PR's head commit: check runs (GitHub
Actions and other apps) and commit statuses (external CI). Its token: a GitHub
**fine-grained personal access token** (Settings → Developer settings →
Personal access tokens → Fine-grained tokens), repository access "Public
repositories (read-only)" for public repositories, or only the repositories in
question with read-only *Pull requests*, *Checks* and *Commit statuses* for
private ones. Organisations may cap a token's lifetime (freeipa refuses tokens
valid for more than 366 days, with a 403 that the preview shows); pick an
expiration within the cap, and when it nears, GitHub's "Regenerate token" gives
a new value to paste into the job. From 14 days before expiry the card carries
a yellow line saying so. Without a token GitHub allows 60 requests an hour,
enough for three PRs per run. `api` points it at GitHub Enterprise.

`news` and `ci` answer `none` when nothing changed, so an unchanged feed or CI
board never refreshes the panel.

`state` is stored by the bridge and handed back as `state` next run, per plugin
and frame, so a plugin can remember an ETag or the last thing it showed without
files of its own. `next_run_s` asks for the next run sooner or later than the
schedule.

A non-zero exit, a timeout, or output that is not this JSON is a failed run: it
is logged and shown in the UI, and the frame keeps what it shows.

## Python

`bridge/plugins/sdk/photoframe_plugin.py` reads the input and writes the answer:

```python
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from photoframe_plugin import Plugin

plugin = Plugin.read()
plugin.items("CI", [("main", "ok"), ("nightly", "fail")], state={"seen": 2})
```

`hello/` is a complete example in Python, `system/` one in plain shell.

## Trying it

```bash
photoframe-bridge plugin list
photoframe-bridge plugin run hello --param name=мир --preview hello.png
```

(`./bridge/build.sh run plugin …` from the repository.) The web UI's Plugins tab
has the same preview, and can show the result on a frame.

## What a plugin may not do

The web UI cannot install, upload or edit plugins: a plugin is a program on this
host, and only someone with access to the host adds one. A plugin runs as the
bridge's user, so treat it like any other program you install.
