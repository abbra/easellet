#!/usr/bin/env python3
"""Weather forecast card from Open-Meteo (https://open-meteo.com), no key needed."""

import json
import sys
import urllib.parse
import urllib.request
from datetime import date, datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from photoframe_plugin import Plugin  # noqa: E402

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


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": "photoframe-bridge"})
    with urllib.request.urlopen(request, timeout=10) as answer:
        return json.load(answer)


PREPOSITIONS = {"в", "во", "на", "для", "у", "in", "at", "for"}
# A place may also follow the word itself, without a preposition, when the
# recogniser swallowed it («покажи погоду ВСПУ» for «в Эспоо»).
WEATHER_WORDS = {"погоду", "погода", "погоды", "weather"}
TIME_WORDS = {
    "завтра", "сегодня", "послезавтра", "неделю", "неделе", "выходные", "выходных",
    "утро", "утром", "вечер", "вечером", "ближайшие", "дни", "дней",
    "tomorrow", "today", "week", "weekend", "the", "next",
}


def fold(text):
    return text.lower().replace("ё", "е")


def place_words(question):
    """The words naming a place in a spoken question, or [].

    «покажи погоду в Эспоо на завтра» -> ["Эспоо"]; «погода на выходные в
    Нижнем Новгороде» -> ["Нижнем", "Новгороде"]; a preposition followed by a
    time word («на завтра») is not a place.
    """
    words = [word.strip(".,!?;:«»\"'()") for word in question.split()]
    words = [word for word in words if word]
    for index, word in enumerate(words):
        if fold(word) in WEATHER_WORDS:
            # Only a capitalised name right after it («погоду Эспоо»), so
            # «погода будет завтра» names no place.
            following = words[index + 1 : index + 2]
            if not following or not following[0][:1].isupper():
                continue
        elif fold(word) not in PREPOSITIONS:
            continue
        place = []
        for following in words[index + 1 : index + 4]:
            if fold(following) in PREPOSITIONS or fold(following) in TIME_WORDS:
                break
            place.append(following)
        if place:
            return place
    return []


def forms(word):
    """The word and the base forms it may be an inflection of (prepositional
    case after «в»: Москве -> Москва, Лондоне -> Лондон, Твери -> Тверь,
    Нижнем -> Нижний). The geocoder decides which one is real."""
    out = [word]
    lower = fold(word)
    if lower.endswith("ем"):
        out.append(word[:-2] + "ий")
    if lower.endswith("ой"):
        out.append(word[:-2] + "ая")
    if lower.endswith("е"):
        out += [word[:-1] + "а", word[:-1], word[:-1] + "ь", word[:-1] + "я"]
    if lower.endswith("и"):
        out += [word[:-1] + "ь", word[:-1] + "и", word[:-1] + "а"]
    return list(dict.fromkeys(out))


def candidates(words, limit=12):
    """Whole-phrase variants, as-is first."""
    phrases = [""]
    for word in words:
        phrases = [f"{phrase} {form}".strip() for phrase in phrases for form in forms(word)]
    return phrases[:limit]


def geocode_exact(name):
    """A place whose name is exactly `name`. The geocoder matches prefixes
    («Москве» finds Москверо, USA), so anything else is refused."""
    # A Latin-script name ("Espoo") is matched against English names, a
    # Cyrillic one against Russian names.
    language = "en" if any("a" <= letter <= "z" for letter in fold(name)) else "ru"
    query = urllib.parse.urlencode({"name": name, "count": 10, "language": language, "format": "json"})
    found = fetch(f"https://geocoding-api.open-meteo.com/v1/search?{query}").get("results") or []
    exact = [hit for hit in found if fold(hit["name"]) == fold(name)]
    if not exact:
        return None
    return max(exact, key=lambda hit: hit.get("population") or 0)


def place_from_question(question, state):
    """The place a spoken question names: a dict if the geocoder knows it, the
    words as text if it does not, None if the question names no place."""
    words = place_words(question)
    if not words:
        return None
    key = " ".join(words)
    cached = state.setdefault("asked", {})
    if key in cached:
        return cached[key]
    for name in candidates(words):
        hit = geocode_exact(name)
        if hit:
            cached[key] = {"latitude": hit["latitude"], "longitude": hit["longitude"], "name": hit["name"]}
            # Keep the cache small: the last few places asked for.
            while len(cached) > 20:
                cached.pop(next(iter(cached)))
            return cached[key]
    return " ".join(words)


def locate(place, state):
    """Coordinates and a display name, from 'lat,lon' or by name (cached in state)."""
    try:
        latitude, longitude = (float(part) for part in place.split(","))
        return latitude, longitude, place
    except ValueError:
        pass
    cached = state.get("place")
    if cached and cached.get("query") == place:
        return cached["latitude"], cached["longitude"], cached["name"]
    query = urllib.parse.urlencode({"name": place, "count": 1, "language": "ru", "format": "json"})
    found = fetch(f"https://geocoding-api.open-meteo.com/v1/search?{query}").get("results")
    if not found:
        raise LookupError(f"место «{place}» не найдено")
    hit = found[0]
    state["place"] = {"query": place, "latitude": hit["latitude"], "longitude": hit["longitude"], "name": hit["name"]}
    return hit["latitude"], hit["longitude"], hit["name"]


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
