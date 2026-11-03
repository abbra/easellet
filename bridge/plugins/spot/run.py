#!/usr/bin/env python3
"""Nord Pool day-ahead electricity prices for one bidding zone (Elering, no key)."""

import hashlib
import json
import sys
import urllib.parse
from datetime import datetime, timedelta, timezone
from pathlib import Path
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from easellet_place import fetch  # noqa: E402
from easellet_plugin import Plugin  # noqa: E402

# Elering's dashboard serves the Nord Pool day-ahead prices of the four zones
# around the Baltic Sea, in EUR per MWh and without tax, and needs no key.
API = "https://dashboard.elering.ee/api/nps/price"
ZONES = {
    "fi": {"vat": 25.5, "ru": "Финляндии", "en": "Finland"},
    "ee": {"vat": 24.0, "ru": "Эстонии", "en": "Estonia"},
    "lt": {"vat": 21.0, "ru": "Литве", "en": "Lithuania"},
    "lv": {"vat": 21.0, "ru": "Латвии", "en": "Latvia"},
}
# A consumer pays cents per kilowatt-hour, so the bands are for that unit.
BANDS = [
    (5.0, "cheap", "ok"),
    (10.0, "normal", "info"),
    (20.0, "dear", "warn"),
    (1e9, "very_dear", "fail"),
]
WORDS = {
    "ru": {
        "now": "Сейчас",
        "unit": "c/кВт·ч",
        "vat": "НДС",
        "cheapest": "Дешевле всего",
        "dearest": "Дороже всего",
        "tomorrow": "Завтра",
        "no_prices": "цен пока нет",
        "incomplete": "данные неполные",
        "same": "как сегодня",
    },
    "en": {
        "now": "Now",
        "unit": "c/kWh",
        "vat": "VAT",
        "cheapest": "Cheapest",
        "dearest": "Dearest",
        "tomorrow": "Tomorrow",
        "no_prices": "no prices yet",
        "incomplete": "incomplete data",
        "same": "about today's",
    },
}
BAND_WORDS = {
    "ru": {
        "negative": "цена отрицательная",
        "cheap": "это дёшево",
        "normal": "обычная цена",
        "dear": "это дорого",
        "very_dear": "это очень дорого",
    },
    "en": {
        "negative": "a negative price",
        "cheap": "cheap",
        "normal": "a normal price",
        "dear": "dear",
        "very_dear": "very dear",
    },
}


def band(price):
    """The band of one price in cents per kilowatt-hour, and its row status."""
    if price < 0:
        return "negative", "ok"
    for edge, name, status in BANDS:
        if price < edge:
            return name, status
    return "very_dear", "fail"


def number(value, language):
    """A price with two decimal places and the decimal comma of Russian."""
    text = f"{value:.2f}"
    return text.replace(".", ",") if language == "ru" else text


def hourly(points, tz):
    """The mean exchange price of each local hour, from the day-ahead intervals.

    Nord Pool quotes 15-minute periods, and a wall frame should change its
    picture once an hour, not every quarter: the intervals of an hour are
    averaged.
    """
    buckets = {}
    for point in points:
        price = point.get("price")
        at = point.get("timestamp")
        if not isinstance(price, (int, float)) or not isinstance(at, (int, float)):
            continue
        hour = datetime.fromtimestamp(at, tz).replace(
            minute=0, second=0, microsecond=0
        )
        buckets.setdefault(hour, []).append(float(price))
    return {hour: sum(values) / len(values) for hour, values in buckets.items()}


def window(prices, start, hours=2):
    """The mean price of `hours` local hours from `start`, or None."""
    values = [prices.get(start + timedelta(hours=offset)) for offset in range(hours)]
    if any(value is None for value in values):
        return None
    return sum(values) / hours


def rate(value, language):
    """A percentage without a needless decimal zero: 25,5 % and 24 %."""
    text = f"{value:g}"
    return text.replace(".", ",") if language == "ru" else text


def span(start, hours=2):
    """«02:00–04:00» for a window of `hours` hours from `start`."""
    end = start + timedelta(hours=hours)
    return f"{start:%H:%M}–{end:%H:%M}"


def main():
    plugin = Plugin.read()
    english = plugin.locale.lower().startswith("en")
    language = "en" if english else "ru"
    words = WORDS[language]
    state = dict(plugin.state)
    zone = (plugin.params.get("zone") or "fi").lower()
    if zone not in ZONES:
        plugin.fail(f"spot: unknown zone «{zone}»")
    vat = ZONES[zone]["vat"]
    # The exchange price is EUR per MWh without tax; a tenth of it is cents per
    # kWh, and the consumer price adds the country's VAT.
    factor = (1 + vat / 100) / 10

    try:
        tz = ZoneInfo(plugin.timezone)
    except (ZoneInfoNotFoundError, ValueError):
        tz = timezone.utc
    try:
        now = datetime.fromisoformat(plugin.now.replace("Z", "+00:00")) if plugin.now else datetime.now(timezone.utc)
    except ValueError:
        now = datetime.now(timezone.utc)
    now_local = now.astimezone(tz)
    today = now_local.date()
    hour = now_local.replace(minute=0, second=0, microsecond=0)
    # The next hour starts a new price, so ask for a run then. The digest also
    # keeps a frame that runs more often from drawing the same rows again.
    next_run_s = int(((hour + timedelta(hours=1)) - now_local).total_seconds())
    next_run_s = max(60, min(3600, next_run_s))
    hour_key = hour.strftime("%Y-%m-%dT%H")
    # The price of an hour does not change within it, so another run in the same
    # hour needs no request. A voice question always reads the price again.
    if (
        not plugin.params.get("question")
        and state.get("hour") == hour_key
        and state.get("zone") == zone
    ):
        plugin.nothing(state=state, next_run_s=next_run_s)

    start = datetime.combine(today, datetime.min.time(), tzinfo=tz)
    query = urllib.parse.urlencode({
        "start": start.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.000Z"),
        "end": (start + timedelta(days=2)).astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.000Z"),
    })
    try:
        answer = fetch(f"{API}?{query}")
        points = (answer.get("data") or {}).get(zone) or []
    except Exception as error:  # noqa: BLE001 - any failure is a failed run
        plugin.fail(f"spot: {error}")
    if not points:
        plugin.fail(f"spot: no prices for zone {zone}")

    prices = {at: value * factor for at, value in hourly(points, tz).items()}
    current = prices.get(hour)
    if current is None:
        plugin.fail(f"spot: no price for {hour:%Y-%m-%d %H:%M}")

    title = f"{'Electricity' if english else 'Электричество'}, {zone.upper()}"
    name, status = band(current)
    rows = [{
        "text": f"{words['now']} {number(current, language)} {words['unit']} ({words['vat']} {rate(vat, language)} %)",
        "status": status,
    }]

    # Every two-hour window that starts in the current hour or later, and ends
    # within the next day: enough for prices that are known only for today.
    windows = [
        (at, window(prices, at))
        for at in (hour + timedelta(hours=offset) for offset in range(23))
    ]
    windows = [(at, value) for at, value in windows if value is not None]
    cheapest = min(windows, key=lambda pair: pair[1]) if windows else None
    dearest = max(windows, key=lambda pair: pair[1]) if windows else None
    if cheapest:
        rows.append({"text": f"{words['cheapest']} {span(cheapest[0])} · {number(cheapest[1], language)}"})
    if dearest:
        rows.append({"text": f"{words['dearest']} {span(dearest[0])} · {number(dearest[1], language)}"})

    # Tomorrow's trend, against today's average. The day-ahead auction publishes
    # a complete day, so a partial day is not an average that means anything.
    tomorrow = today + timedelta(days=1)
    today_prices = [value for at, value in prices.items() if at.date() == today]
    tomorrow_prices = [value for at, value in prices.items() if at.date() == tomorrow]
    if len(tomorrow_prices) >= 23 and len(today_prices) >= 23:
        average = sum(tomorrow_prices) / len(tomorrow_prices)
        today_average = sum(today_prices) / len(today_prices)
        if today_average >= 1:
            change = round((average - today_average) / today_average * 100)
            trend = f"{change:+d} %" if change else words["same"]
        else:
            # A percent of a near-zero price says nothing, so show the change.
            change = average - today_average
            trend = f"{change:+.2f}".replace(".", ",") if language == "ru" else f"{change:+.2f}"
            trend = words["same"] if abs(change) < 0.05 else trend
        rows.append({"text": f"{words['tomorrow']} {number(average, language)} ({trend})"})
    else:
        # A whole day arrives at once, so a partial day is not yet an answer.
        missing = words["incomplete"] if tomorrow_prices else words["no_prices"]
        rows.append({"text": f"{words['tomorrow']}: {missing}", "status": "info"})

    # The card stays as it is when the rows are the same as last time.
    digest = hashlib.sha256(json.dumps([title, rows], ensure_ascii=False).encode()).hexdigest()
    remembered = {"hour": hour_key, "zone": zone, "digest": digest}
    if state.get("digest") == digest:
        plugin.nothing(state=remembered, next_run_s=next_run_s)

    say = None
    if plugin.params.get("question"):
        if english:
            say = f"Electricity in {ZONES[zone]['en']}: now {number(current, language)} cents per kilowatt-hour, {BAND_WORDS['en'][name]}."
        else:
            say = f"Электричество в {ZONES[zone]['ru']}: сейчас {number(current, language)} цента за киловатт-час, {BAND_WORDS['ru'][name]}."
        if cheapest:
            first = cheapest[0]
            last = first + timedelta(hours=2)
            if english:
                say += f" Cheapest from {first.hour} to {last.hour}: {number(cheapest[1], language)}."
            else:
                say += f" Дешевле всего с {first.hour} до {last.hour}: {number(cheapest[1], language)}."

    plugin.items(
        title,
        rows,
        accent="yellow",
        state=remembered,
        next_run_s=next_run_s,
        say=say,
    )


if __name__ == "__main__":
    main()
