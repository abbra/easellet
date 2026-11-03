#!/usr/bin/env python3
"""Weather forecast card from Open-Meteo (https://open-meteo.com), no key needed."""

import sys
import urllib.parse
from datetime import date, datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from easellet_place import fetch, locate, place_from_question  # noqa: E402
from easellet_plugin import Plugin  # noqa: E402

WEEKDAYS = ["Пн", "Вт", "Ср", "Чт", "Пт", "Сб", "Вс"]


def icon(code):
    """WMO weather code -> one of the bridge's icons."""
    if code == 0:
        return "clear"
    if code in (1, 2):
        return "partly"
    if code == 3:
        return "cloudy"
    if code in (45, 48):
        return "fog"
    if 51 <= code <= 57:
        return "drizzle"
    if 61 <= code <= 67 or 80 <= code <= 82:
        return "rain"
    if 71 <= code <= 77 or code in (85, 86):
        return "snow"
    if code >= 95:
        return "thunder"
    return "cloudy"


def main():
    plugin = Plugin.read()
    state = dict(plugin.state)
    days = int(plugin.params.get("days", "4"))
    try:
        asked = place_from_question(plugin.params.get("question", ""), state)
        say = None
        if isinstance(asked, dict):
            latitude, longitude, name = asked["latitude"], asked["longitude"], asked["name"]
            say = f"Показываю погоду: {name}."
        else:
            latitude, longitude, name = locate(plugin.params["place"], state)
            if isinstance(asked, str):
                # Never pass off the default place as the one asked for.
                say = f"Не нашёл место «{asked}». Показываю погоду: {name}."
        query = urllib.parse.urlencode({
            "latitude": latitude,
            "longitude": longitude,
            "daily": "weather_code,temperature_2m_max,temperature_2m_min,precipitation_sum",
            "timezone": "auto",
            "forecast_days": days,
        })
        daily = fetch(f"https://api.open-meteo.com/v1/forecast?{query}")["daily"]
    except Exception as error:  # noqa: BLE001 - any failure is a failed run
        plugin.fail(f"weather: {error}")
    today = date.today()
    cards = []
    for index, day in enumerate(daily["time"]):
        when = date.fromisoformat(day)
        label = "Сегодня" if when == today else WEEKDAYS[when.weekday()]
        cards.append({
            "label": label,
            "icon": icon(daily["weather_code"][index]),
            "high": daily["temperature_2m_max"][index],
            "low": daily["temperature_2m_min"][index],
            "precip_mm": daily["precipitation_sum"][index],
        })
    plugin.emit(
        {
            "type": "card",
            "layout": "forecast",
            "title": name,
            "days": cards,
            "note": f"Open-Meteo, обновлено {datetime.now().strftime('%H:%M')}",
        },
        state=state,
        say=say,
    )


main()
