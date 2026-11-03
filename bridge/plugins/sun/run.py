#!/usr/bin/env python3
"""Sunrise, sunset, day length and moon phase for a place (Open-Meteo, no key)."""

import hashlib
import json
import math
import sys
import urllib.parse
from datetime import date, datetime, timedelta, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from easellet_place import fetch, locate, place_from_question  # noqa: E402
from easellet_plugin import Plugin  # noqa: E402

# A known new moon and the mean length of a lunation: enough to name the phase
# and the illuminated part, which is what a card can show.
NEW_MOON = datetime(2000, 1, 6, 18, 14, tzinfo=timezone.utc)
SYNODIC_DAYS = 29.530588853
MOON = {
    "ru": [
        "новолуние",
        "растущий серп",
        "первая четверть",
        "растущая луна",
        "полнолуние",
        "убывающая луна",
        "последняя четверть",
        "убывающий серп",
    ],
    "en": [
        "new moon",
        "waxing crescent",
        "first quarter",
        "waxing gibbous",
        "full moon",
        "waning gibbous",
        "last quarter",
        "waning crescent",
    ],
}
TOMORROW_WORDS = {"завтра", "tomorrow"}


def moon_of(now):
    """The phase index (0 = new moon, 4 = full moon) and the lit part, in %."""
    days = (now - NEW_MOON).total_seconds() / 86400
    fraction = (days / SYNODIC_DAYS) % 1
    return int(fraction * 8 + 0.5) % 8, round((1 - math.cos(2 * math.pi * fraction)) / 2 * 100)


def span(seconds, language):
    """A length of day as hours and minutes."""
    minutes = int(round(seconds / 60))
    if language == "en":
        return f"{minutes // 60} h {minutes % 60:02d} min"
    return f"{minutes // 60} ч {minutes % 60:02d} мин"


def asked_tomorrow(question):
    words = {word.strip(".,!?;:«»\"'()").lower().replace("ё", "е") for word in (question or "").split()}
    return bool(words & TOMORROW_WORDS)


def main():
    plugin = Plugin.read()
    english = plugin.locale.lower().startswith("en")
    language = "en" if english else "ru"
    state = dict(plugin.state)
    words = {
        "sun": "Sun" if english else "Солнце",
        "tomorrow": "tomorrow" if english else "завтра",
        "sunrise": "Sunrise" if english else "Восход",
        "sunset": "Sunset" if english else "Закат",
        "day": "Day" if english else "День",
        "moon": "Moon" if english else "Луна",
        "night": "Polar night" if english else "Полярная ночь",
        "day_long": "Polar day" if english else "Полярный день",
        "no_sunrise": "The sun does not rise." if english else "Солнце не восходит.",
        "no_sunset": "The sun does not set." if english else "Солнце не заходит.",
    }
    try:
        now = datetime.fromisoformat(plugin.now.replace("Z", "+00:00")) if plugin.now else datetime.now(timezone.utc)
    except ValueError:
        now = datetime.now(timezone.utc)
    tomorrow = asked_tomorrow(plugin.params.get("question", ""))

    try:
        asked = place_from_question(plugin.params.get("question", ""), state)
        if isinstance(asked, dict):
            latitude, longitude, name = asked["latitude"], asked["longitude"], asked["name"]
        else:
            latitude, longitude, name = locate(plugin.params["place"], state)
        query = urllib.parse.urlencode({
            "latitude": latitude,
            "longitude": longitude,
            "daily": "sunrise,sunset,daylight_duration",
            "timezone": "auto",
            "past_days": 1,
            "forecast_days": 2,
        })
        daily = fetch(f"https://api.open-meteo.com/v1/forecast?{query}")["daily"]
    except Exception as error:  # noqa: BLE001 - any failure is a failed run
        plugin.fail(f"sun: {error}")

    days = [date.fromisoformat(day) for day in daily["time"]]
    today = now.astimezone(timezone.utc).date()
    # The place's date is at most one day away from the UTC date, so a
    # question about tomorrow means the next row of the answer.
    wanted = today + timedelta(days=1) if tomorrow else today
    title = words["sun"] if not tomorrow else f"{words['sun']} — {words['tomorrow']}"
    if wanted not in days:
        # The place is a day ahead of UTC or behind it, so its date differs from
        # the UTC date by one day at most. Anything further means the answer is
        # about other days, and showing another day would be a wrong answer.
        nearest = min(days, key=lambda day: abs((day - wanted).days))
        if abs((nearest - wanted).days) > 1:
            plugin.fail(f"sun: the answer has no day {wanted}")
        wanted = nearest
    index = days.index(wanted)
    sunrise = daily["sunrise"][index]
    sunset = daily["sunset"][index]
    daylight = daily["daylight_duration"][index]
    say = None

    if not sunrise or not sunset:
        polar_day = (daylight or 0) >= 23.9 * 3600
        heading = words["day_long"] if polar_day else words["night"]
        body = words["no_sunset"] if polar_day else words["no_sunrise"]
        plugin.notice(heading, body, accent="yellow", state={"digest": None},
                      say=f"{heading}." if plugin.params.get("question") else None)

    phase, lit = moon_of(now.astimezone(timezone.utc))
    rows = [
        {"text": f"{words['sunrise']} {sunrise[11:16]}"},
        {"text": f"{words['sunset']} {sunset[11:16]}"},
    ]
    line = f"{words['day']} {span(daylight, language)}"
    if index > 0 and daily["daylight_duration"][index - 1]:
        change = int(round((daylight - daily["daylight_duration"][index - 1]) / 60))
        if change:
            line += f" ({change:+d} {'min' if english else 'мин'})"
    rows.append({"text": line})
    rows.append({"text": f"{words['moon']}: {MOON[language][phase]}, {lit}%"})

    # Same card as last time: nothing new to draw.
    digest = hashlib.sha256(json.dumps([title, rows], ensure_ascii=False).encode()).hexdigest()
    if plugin.state.get("digest") == digest:
        plugin.nothing(state=plugin.state)

    if plugin.params.get("question"):
        day = f", {words['tomorrow']}" if tomorrow else ""
        if english:
            say = f"Sun in {name}{day}: sunrise {sunrise[11:16]}, sunset {sunset[11:16]}."
        else:
            say = f"Солнце в {name}{day}: восход {sunrise[11:16]}, закат {sunset[11:16]}."
    plugin.items(title, rows, accent="yellow", state={"digest": digest}, next_run_s=6 * 3600, say=say)


if __name__ == "__main__":
    main()
