# Writing a plugin for easellet-bridge

A plugin is a program that decides **what** a frame should show. The bridge decides
**how** to draw it (size, colours, orientation, dithering) for each frame, and when
to send it. Any language works. See `research/bridge-ui-design.md` section 5 for the
design.

## Layout

```
~/.config/easellet-bridge/plugins/<name>/   your plugins (checked first)
bridge/plugins/<name>/                        the ones shipped with the bridge
    plugin.toml    the manifest
    run…           the program, executable, path given in the manifest
```

`<name>` is `[a-z0-9-]`, at most 32 characters, and must equal `name` in the
manifest. A plugin in your directory replaces a shipped plugin of the same name.

The web UI names a plugin's directory relative to the plugins directory that holds
it, and names that directory by the one above it: a shipped plugin reads
`bridge/air`, one of your own `easellet-bridge/my-thing`.

## The manifest

```toml
name = "weather"
title = "Погода"                 # what the web UI shows
description = "Прогноз на 4 дня."
category = "weather"             # the group in the web UI, see below
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
i18n = { en = { title = "Place", note = "A city or «latitude,longitude»." } }
```

The web UI files a plugin under the group that `category` names. It knows five
groups: `weather` (weather and nature), `home`, `news` (news and work), `pictures`
and `examples`. It shows them in that order, each sorted by title, and it shows a
plugin that names no group as «Другое» (Other) last. A group that the UI does not
know appears under the name in the manifest, so a plugin may invent one and still
be found.

The bridge stores `secret` parameters and never sends them back to a browser.
`hidden = true` keeps a parameter out of the web form. Use it for a parameter that
the assistant fills (`text_param`).

### The same text in another language

The web UI opens in English or Russian, and the words that it shows for a plugin
come from the manifest. A translation therefore belongs in the manifest too. `i18n`
is a table keyed by language tag. The bridge reads `title` and `description` from it
on the plugin, and `title` and `note` on a parameter:

```toml
title = "Погода"
i18n = { en = { title = "Weather", description = "A forecast from Open-Meteo." } }

[[param]]
id = "place"
title = "Место"
i18n = { en = { title = "Place" } }
```

Anything missing falls back to the authored text. A plugin with no `i18n` therefore
stays in the language in which it was written. The interface then shows a Russian
title under an English page, not a blank title. The manifest keeps the authored
language first because that is also the language in which the assistant's `intent`
and `phrases` are matched. The UI does not translate those, and they must not be
translated.

The plugin's own output (the card's `title` and `text`, and the words that it says)
is content, not interface. A Russian plugin used by an English page still draws the
Russian card that it was asked for.

### Voice

```toml
[assistant]
intent = "показать погоду"                      # the main phrase
phrases = ["покажи погоду", "какая погода"]     # other ways people say it
text_param = "prompt"                           # optional: gets the whole question
description = "Show the weather forecast on the frame."
```

The bridge compares phrases by stem. «покажи погоду на завтра» therefore matches
«покажи погоду». When a frame's question contains every word of one of these
phrases, the bridge runs the plugin for that frame instead of asking the model. It
puts the result on the panel when the frame has finished speaking. The frame says
the plugin's `say` (below) if the run ends within 8 s. Otherwise the frame says
«Показываю: <title>». The plugin uses the parameters of a saved job for that plugin
and frame, or the defaults. Russian imperatives and infinitives differ after the
stem, so list the forms that people actually say.

## One run

The bridge starts the program in its directory with a cleared environment (`PATH`,
`HOME`, `TZ`, proxy and CA variables, `LANG=C.UTF-8`, `EASEL_PLUGIN_SCRATCH`). It
writes one JSON document to the program's stdin:

```json
{
  "api": 1,
  "params": {"place": "Helsinki"},
  "frame": {"id": "94a990caed46", "name": "ODCAED44", "width": 800, "height": 480,
            "orientation": "landscape",
            "palette": ["black", "white", "yellow", "red", "blue", "green"]},
  "locale": "ru-RU", "timezone": "Europe/Helsinki", "now": "2026-11-02T10:00:00Z",
  "scratch": "/tmp/easellet-bridge-…",
  "state": null
}
```

The program answers with one JSON document on stdout. Its stderr goes to the
bridge's log.

```json
{"api": 1,
 "output": {"type": "card", "layout": "list", "title": "CI",
            "items": [{"text": "main", "status": "ok"}]},
 "state": {"etag": "…"},
 "next_run_s": 900,
 "say": "Показываю погоду: Эспоо."}
```

`say` is optional. It is what the frame speaks when a voice question ran the
plugin. Use it to name what is shown, and to be honest when the plugin could not
meet the request as asked («Не нашёл место «ВСПУ». Показываю погоду: Хельсинки.»).

| `output.type` | Fields |
| --- | --- |
| `card` | `layout` and its fields, below. |
| `text` | `title`, `text`: shown as a `notice` card. |
| `image` | `path` (a PNG or JPEG inside `scratch`) or `base64`; `fit`: `cover` (default) or `contain`. |
| `none` | Nothing new: the frame keeps its picture and is not refreshed. |

Card layouts:

- `notice`: `title`, `body`, `accent` (`blue` `red` `green` `yellow` `black`).
- `list`: `title`, `items`
  (`[{"text": …, "status": "ok|fail|warn|info", "counts": {"fail": 6, "running": 2, "ok": 35, "other": 1}}]`),
  `accent`. `counts` is optional. The bridge draws it at the row's right end as
  coloured squares with numbers (red, yellow, green, black; zeros are left out).
  The text is cut to make room.
- `forecast`: `title`, `days`
  (`[{"label": "Пн", "icon": …, "high": 5, "low": -1, "precip_mm": 0.4}]`, up to 6),
  `note`, `accent`. Icons: `clear` `partly` `cloudy` `fog` `drizzle` `rain` `snow`
  `thunder`.

## Shipped plugins

| Plugin | Shows | Needs |
| --- | --- | --- |
| `weather` | A forecast for a place. By voice, for the place named («…погоду в Эспоо», "…weather in Espoo"). | Nothing (Open-Meteo). |
| `sun` | Sunrise, sunset, the length of the day and the moon phase for a place. | Nothing (Open-Meteo). |
| `air` | Air quality (European AQI), particles and pollen for a place. | Nothing (Open-Meteo). |
| `spot` | The Nord Pool electricity price for Finland, Estonia, Lithuania or Latvia: the current hour, the cheapest and dearest two hours ahead, and tomorrow against today. | Nothing (Elering). |
| `agenda` | The next events of an iCalendar calendar. By voice, for the day named («что у меня завтра», "what is on tomorrow"). | A calendar address, a file, or a path. No key. |
| `news` | Headlines of an RSS/Atom feed. | A feed URL. |
| `ci` | The latest open PRs of a GitHub repository, each with how many checks failed, ran and passed (Actions and external CI). | A read-only token (see below). |
| `note` | A title and text. | — |
| `photo-folder` | Photos from a folder on this host, in turn. | A folder. |
| `ai-image` | A generated picture. | An OpenAI-compatible images endpoint. |
| `hello`, `system` | Examples. | — |

`agenda` reads an `.ics` calendar from an `http`, `https`, `webcal` or `file`
address, or from a path on this host. A calendar's private address is a key that
shows the whole calendar, so the parameter has type `secret`: the bridge stores it
and never sends it back to a browser. The plugin expands recurring events for the
days that the card shows. It reads `FREQ` with `INTERVAL`, `COUNT`, `UNTIL`,
`BYDAY` and `BYMONTHDAY`, and it honours `EXDATE` and `STATUS:CANCELLED`. Other
properties are ignored. All-day events have no time. A timed event shows its
start. The card answers `none` when its rows do not change, and it asks for the
next run when the first event starts, so a finished row does not stay on the
panel.

`sun` shows sunrise, sunset and the length of the day, and how much the day grew or
shrank against the previous one. It names the moon phase and the lit part, which it
computes from the mean length of a lunation, so the moon needs no request of its
own. In the polar night and in the polar day it says so instead of showing an empty
time. It takes the place from the question the way `weather` does («во сколько закат
в Тампере»), and it takes the day from the question: «завтра» shows tomorrow.

`air` shows the European AQI with its band, the PM2.5 and PM10 particles, the pollen
types that are above zero, and the UV index from 1. It takes the place from the
question the way `weather` does («какой воздух в Эспоо»), and it names the place that
it shows when it speaks.

`spot` reads the Nord Pool day-ahead prices from Elering's dashboard, in EUR per MWh
and without tax, and adds the VAT of the zone. Nord Pool quotes 15-minute periods.
The card averages the periods of each local hour, so the panel changes at the start
of an hour and not every quarter. The cheapest and dearest rows are two-hour
windows that start in the current hour or later and end within the next day. The
last row compares the mean of tomorrow with the mean of today. A day-ahead auction
publishes a whole day, so a partial day has no mean on the card. The card answers
`none` when its rows do not change, and a run in the same hour asks for no prices
again. The default interval is 15 minutes, so the card follows the hour.

`ci` counts both kinds of CI reports on a PR's head commit: check runs (GitHub
Actions and other apps) and commit statuses (external CI). Its token is a GitHub
**fine-grained personal access token** (Settings → Developer settings → Personal
access tokens → Fine-grained tokens). For public repositories, use repository
access "Public repositories (read-only)". For private repositories, use only the
repositories in question with read-only *Pull requests*, *Checks* and *Commit
statuses*. Organisations can limit a token's lifetime. FreeIPA refuses tokens that
are valid for more than 366 days, with a 403 that the preview shows. Pick an
expiration within the limit. When the token nears its expiry, GitHub's "Regenerate
token" gives a new value to paste into the job. From 14 days before expiry, the card
carries a yellow line that says so. Without a token, GitHub allows 60 requests an
hour, which is enough for three PRs per run. `api` points the plugin at GitHub
Enterprise.

`news`, `ci`, `sun`, `air` and `spot` answer `none` when nothing changed, so an
unchanged feed, CI board, sky or price never refreshes the panel.

The bridge stores `state` and hands it back as `state` on the next run, per plugin
and frame. A plugin can therefore remember an ETag or the last thing that it showed
without files of its own. `next_run_s` asks for the next run sooner or later than
the schedule.

A non-zero exit, a timeout, or output that is not this JSON is a failed run. The
bridge logs it and shows it in the UI, and the frame keeps what it shows.

## Python

`bridge/plugins/sdk/easellet_plugin.py` reads the input and writes the answer:

```python
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from easellet_plugin import Plugin

plugin = Plugin.read()
plugin.items("CI", [("main", "ok"), ("nightly", "fail")], state={"seen": 2})
```

`hello/` is a complete example in Python. `system/` is one in plain shell.

`bridge/plugins/sdk/easellet_place.py` holds the place lookup that `weather`,
`sun` and `air` share: the Open-Meteo geocoder, the coordinates cache, and the
words of a spoken question that name a place.

## Trying it

```bash
easellet-bridge plugin list
easellet-bridge plugin run hello --param name=мир --preview hello.png
```

(`./bridge/build.sh run plugin …` from the repository.) The web UI's Plugins tab
has the same preview, and can show the result on a frame.

## What a plugin may not do

The web UI cannot install, upload or edit plugins. A plugin is a program on this
host, and only someone with access to the host adds one. A plugin runs as the
bridge's user, so treat it like any other program that you install.
