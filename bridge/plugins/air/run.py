#!/usr/bin/env python3
"""Air quality, particles and pollen for a place (Open-Meteo, no key)."""

import hashlib
import json
import sys
import urllib.parse
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from easellet_place import fetch, locate, place_from_question  # noqa: E402
from easellet_plugin import Plugin  # noqa: E402

# European AQI bands, as the agency that defines the index publishes them.
BANDS = [
    (20, "ok", "хорошее", "good"),
    (40, "ok", "приемлемое", "fair"),
    (60, "warn", "среднее", "moderate"),
    (80, "warn", "плохое", "poor"),
    (100, "fail", "очень плохое", "very poor"),
    (10**9, "fail", "крайне плохое", "extremely poor"),
]
POLLEN = [
    ("alder_pollen", "ольха", "alder"),
    ("birch_pollen", "берёза", "birch"),
    ("grass_pollen", "злаки", "grasses"),
    ("mugwort_pollen", "полынь", "mugwort"),
    ("olive_pollen", "олива", "olive"),
    ("ragweed_pollen", "амброзия", "ragweed"),
]


def band_of(aqi):
    for limit, status, ru, en in BANDS:
        if aqi <= limit:
            return status, ru, en
    return BANDS[-1][1:]


def pollen_status(value):
    """Low, moderate or high: the squares say it without another word."""
    if value < 10:
        return "ok"
    return "warn" if value < 50 else "fail"


def number(value):
    return f"{value:.1f}".rstrip("0").rstrip(".") if value is not None else "—"


def main():
    plugin = Plugin.read()
    english = plugin.locale.lower().startswith("en")
    state = dict(plugin.state)
    words = {
        "air": "Air" if english else "Воздух",
        "index": "Index" if english else "Индекс",
        "particles": "Particles" if english else "Частицы",
        "pollen": "Pollen" if english else "Пыльца",
        "uv": "UV index" if english else "УФ-индекс",
        "unit": "µg/m³" if english else "мкг/м³",
        "none": "No data for this place." if english else "Нет данных для этого места.",
    }
    question = plugin.params.get("question", "")

    try:
        asked = place_from_question(question, state)
        if isinstance(asked, dict):
            latitude, longitude, name = asked["latitude"], asked["longitude"], asked["name"]
        else:
            latitude, longitude, name = locate(plugin.params["place"], state)
        query = urllib.parse.urlencode({
            "latitude": latitude,
            "longitude": longitude,
            "current": "european_aqi,pm2_5,pm10,uv_index,"
            + ",".join(key for key, _, _ in POLLEN),
            "timezone": "auto",
        })
        current = fetch(f"https://air-quality-api.open-meteo.com/v1/air-quality?{query}")["current"]
    except Exception as error:  # noqa: BLE001 - any failure is a failed run
        plugin.fail(f"air: {error}")

    # The question named a place that the geocoder does not know: say so
    # instead of passing off the default place as the one asked for.
    missing = asked if isinstance(asked, str) else None
    title = words["air"] if not isinstance(asked, dict) else f"{words['air']}: {name}"

    aqi = current.get("european_aqi")
    if aqi is None:
        plugin.notice(title, words["none"], accent="yellow", state={"digest": None},
                      say=f"{words['none']}" if question else None)

    status, ru, en = band_of(aqi)
    rows = [{"text": f"{words['index']} {round(aqi)} — {en if english else ru}", "status": status}]
    for key, label in (("pm2_5", "PM2.5"), ("pm10", "PM10")):
        if current.get(key) is not None:
            rows.append({"text": f"{label} {number(current[key])} {words['unit']}"})
    pollens = []
    for key, label_ru, label_en in POLLEN:
        value = current.get(key) or 0
        if value > 0:
            pollens.append((value, label_en if english else label_ru))
    pollens.sort(reverse=True)
    for value, label in pollens[:3]:
        rows.append({"text": f"{words['pollen']}: {label} {number(value)}", "status": pollen_status(value)})
    uv = current.get("uv_index")
    if uv is not None and uv >= 1:
        rows.append({"text": f"{words['uv']} {number(uv)}", "status": "ok" if uv < 3 else ("warn" if uv < 6 else "fail")})

    # Same card as last time: nothing new to draw.
    digest = hashlib.sha256(json.dumps([title, rows], ensure_ascii=False).encode()).hexdigest()
    if plugin.state.get("digest") == digest:
        plugin.nothing(state=plugin.state)

    say = None
    if question:
        if english:
            say = f"Air in {name}: {round(aqi)}, {en}."
        else:
            say = f"Воздух в {name}: {round(aqi)}, {ru}."
        if missing:
            # The question named a place that the geocoder does not know.
            say = f"Не нашёл место «{missing}». " + say if not english else f"Place «{missing}» not found. " + say
    plugin.items(title, rows, accent="blue", state={"digest": digest}, next_run_s=3600, say=say)


if __name__ == "__main__":
    main()
