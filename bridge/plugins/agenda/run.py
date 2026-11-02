#!/usr/bin/env python3
"""The next events of an iCalendar (.ics) calendar, read-only.

Accepts an http(s) address, a `webcal://` address, a `file://` address or a path
on this host, so a calendar that is only exported by hand works too. Recurring
events are expanded for the days that the card shows; only what a person needs at
a glance is kept: a day, a time and a title.
"""

import hashlib
import json
import re
import sys
import urllib.parse
import urllib.request
from datetime import date, datetime, time, timedelta, timezone
from pathlib import Path
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from easellet_plugin import Plugin  # noqa: E402

MAX_BYTES = 4 * 1024 * 1024
MAX_SUMMARY = 120
PROPERTY = re.compile(r"^([A-Za-z0-9-]+)((?:;[^:]*)*):(.*)$")
WEEKDAYS = {
    "ru": ["Пн", "Вт", "Ср", "Чт", "Пт", "Сб", "Вс"],
    "en": ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"],
}
DAY_CODES = {"MO": 0, "TU": 1, "WE": 2, "TH": 3, "FR": 4, "SA": 5, "SU": 6}
TODAY_WORDS = {"сегодня", "today", "сейчас"}
TOMORROW_WORDS = {"завтра", "tomorrow"}
WEEK_WORDS = {"неделю", "неделе", "неделя", "недели", "week", "7", "seven"}


# --- reading ---------------------------------------------------------------


def read_source(spec, timeout=10):
    """The bytes of a calendar: an address or a file on this host."""
    spec = spec.strip()
    if spec.startswith("webcal://"):
        spec = "https://" + spec[len("webcal://") :]
    if spec.startswith(("http://", "https://")):
        request = urllib.request.Request(spec, headers={"User-Agent": "easellet-bridge"})
        with urllib.request.urlopen(request, timeout=timeout) as answer:
            return answer.read(MAX_BYTES)
    if spec.startswith("file://"):
        spec = urllib.parse.urlparse(spec).path
    return Path(spec).expanduser().read_bytes()[:MAX_BYTES]


def unfold(text):
    """Lines, with the folded continuations joined. A long line in iCalendar
    continues on the next line, which starts with a space or a tab."""
    lines = []
    for line in text.splitlines():
        if line[:1] in (" ", "\t") and lines:
            lines[-1] += line[1:]
        elif line.strip():
            lines.append(line)
    return lines


def split_property(line):
    """`NAME;PARAM=value:content` as (NAME, {PARAM: value}, content)."""
    found = PROPERTY.match(line)
    if not found:
        return None
    params = {}
    for part in filter(None, found.group(2).split(";")):
        key, _, value = part.partition("=")
        params[key.upper()] = value.strip('"')
    return found.group(1).upper(), params, found.group(3)


def unescape(value):
    return re.sub(r"\\(.)", lambda found: " " if found.group(1) in "nN" else found.group(1), value)


# --- values ----------------------------------------------------------------


def moment(value, params, zone):
    """A date for a whole-day value, an aware datetime otherwise."""
    value = value.strip()
    if params.get("VALUE", "").upper() == "DATE" or ("T" not in value and len(value) == 8):
        return date.fromisoformat(f"{value[0:4]}-{value[4:6]}-{value[6:8]}")
    utc = value.endswith("Z")
    stamp = value[:-1] if utc else value
    for shape in ("%Y%m%dT%H%M%S", "%Y%m%dT%H%M"):
        try:
            parsed = datetime.strptime(stamp, shape)
            break
        except ValueError:
            continue
    else:
        raise ValueError(f"unreadable time {value!r}")
    if utc:
        return parsed.replace(tzinfo=timezone.utc)
    name = params.get("TZID")
    if name:
        try:
            return parsed.replace(tzinfo=ZoneInfo(name))
        except (ZoneInfoNotFoundError, ValueError):
            pass
    return parsed.replace(tzinfo=zone)


def duration(text, default):
    """An ISO 8601 duration such as PT1H30M, or `default` when it is not one."""
    found = re.fullmatch(r"[+-]?P(?:(\d+)W)?(?:(\d+)D)?(?:T(?:(\d+)H)?(?:(\d+)M)?)?", text.strip())
    if not found or not any(found.groups()):
        return default
    weeks, days, hours, minutes = (int(part or 0) for part in found.groups())
    return timedelta(weeks=weeks, days=days, hours=hours, minutes=minutes)


def rule_of(value):
    rule = {}
    for part in value.split(";"):
        key, _, setting = part.partition("=")
        if key:
            rule[key.upper()] = setting
    return rule


# --- events ----------------------------------------------------------------


def events_of(text, zone):
    """The VEVENT blocks as dicts, cancelled ones left out."""
    events = []
    inside = None
    for line in unfold(text):
        found = split_property(line)
        if not found:
            continue
        name, params, value = found
        if name == "BEGIN" and value.strip().upper() == "VEVENT":
            inside = []
        elif name == "END" and value.strip().upper() == "VEVENT":
            if inside is not None:
                event = event_of(inside, zone)
                if event:
                    events.append(event)
            inside = None
        elif inside is not None:
            inside.append((name, params, value))
    return events


def event_of(lines, zone):
    fields = {}
    for name, params, value in lines:
        fields.setdefault(name, (params, value))
    if "DTSTART" not in fields:
        return None
    if fields.get("STATUS", ({}, ""))[1].strip().upper() == "CANCELLED":
        return None
    params, value = fields["DTSTART"]
    start = moment(value, params, zone)
    end = start
    if "DTEND" in fields:
        end = moment(fields["DTEND"][1], fields["DTEND"][0], zone)
    elif "DURATION" in fields:
        end = start + duration(fields["DURATION"][1], timedelta(0))
    elif isinstance(start, datetime):
        end = start
    else:
        end = start + timedelta(days=1)
    all_day = isinstance(start, date) and not isinstance(start, datetime)
    summary = unescape(fields.get("SUMMARY", ({}, ""))[1])
    summary = " ".join(summary.split())[:MAX_SUMMARY] or "(без названия)"
    exdates = set()
    for name, params, value in lines:
        if name != "EXDATE":
            continue
        for piece in value.split(","):
            try:
                exdates.add(moment(piece, params, zone))
            except ValueError:
                pass
    rule = rule_of(fields["RRULE"][1]) if "RRULE" in fields else None
    return {"start": start, "end": end, "all_day": all_day, "summary": summary, "rule": rule, "exdates": exdates}


def as_datetime(value, zone):
    return value if isinstance(value, datetime) else datetime.combine(value, time(), tzinfo=zone)


def expand(event, window_start, window_end, zone):
    """The starts of `event` inside the window, the first one included."""
    start = as_datetime(event["start"], zone)
    rule = event["rule"]
    if not rule:
        return [start] if start < window_end and as_datetime(event["end"], zone) > window_start else []

    freq = rule.get("FREQ", "").upper()
    interval = max(1, int(rule.get("INTERVAL", "1") or 1))
    count = int(rule["COUNT"]) if rule.get("COUNT", "").isdigit() else None
    until = None
    if "UNTIL" in rule:
        try:
            until = moment(rule["UNTIL"], {}, zone)
            if not isinstance(until, datetime):
                until = datetime.combine(until, time(23, 59, 59), tzinfo=zone)
        except ValueError:
            until = None

    found = []
    for index, occurrence in enumerate(starts_of(start, rule, freq, interval, window_end)):
        if count is not None and index >= count:
            break
        if until is not None and occurrence > until:
            break
        if occurrence in event["exdates"] or occurrence.date() in event["exdates"]:
            continue
        if window_start <= occurrence < window_end:
            found.append(occurrence)
    return found


def starts_of(start, rule, freq, interval, window_end):
    """Every start of a series, oldest first, in order, up to the window."""
    if freq == "DAILY":
        step = timedelta(days=interval)
        while start < window_end:
            yield start
            start += step
        return
    if freq == "WEEKLY":
        byday = sorted(DAY_CODES[code[-2:]] for code in rule.get("BYDAY", "").split(",") if code[-2:] in DAY_CODES)
        if not byday:
            step = timedelta(weeks=interval)
            while start < window_end:
                yield start
                start += step
            return
        week = start - timedelta(days=start.weekday())
        while week < window_end:
            for weekday in byday:
                occurrence = week + timedelta(days=weekday)
                if occurrence >= start:
                    yield occurrence
            week += timedelta(weeks=interval)
        return
    if freq in ("MONTHLY", "YEARLY"):
        step = interval if freq == "MONTHLY" else 12 * interval
        months = int(rule.get("BYMONTHDAY", "") or 0)
        days = [months] if months else [start.day]
        if freq == "YEARLY" and "BYMONTH" not in rule:
            days = [start.day]
        month = start.month - 1
        year = start.year
        while True:
            for day in days:
                try:
                    occurrence = start.replace(year=year, month=month % 12 + 1, day=day)
                except ValueError:
                    continue  # 31 February and the like do not exist
                if occurrence >= start:
                    yield occurrence
            year += (month + step) // 12
            month = (month + step) % 12
            first = start.replace(year=year + month // 12, month=month % 12 + 1, day=1)
            if first >= window_end:
                return
        return
    # An unknown or absent rule: the single start is all there is.
    if start < window_end:
        yield start


# --- the card --------------------------------------------------------------


def plural_ru(number, forms):
    if number % 10 == 1 and number % 100 != 11:
        return forms[0]
    if 2 <= number % 10 <= 4 and not 12 <= number % 100 <= 14:
        return forms[1]
    return forms[2]


def scope_of(question, days, today):
    """The window the question asks for: (first day, number of days)."""
    words = {word.strip(".,!?;:«»\"'()").lower().replace("ё", "е") for word in question.split()}
    if words & WEEK_WORDS:
        return today, 7, "week"
    if words & TOMORROW_WORDS:
        return today + timedelta(days=1), 1, "tomorrow"
    if words & TODAY_WORDS:
        return today, 1, "today"
    return today, days, None


def main():
    plugin = Plugin.read()
    zone = ZoneInfo(plugin.timezone) if plugin.timezone else timezone.utc
    english = plugin.locale.lower().startswith("en")
    language = "en" if english else "ru"
    words = {
        "today": "Today" if english else "Сегодня",
        "tomorrow": "Tomorrow" if english else "Завтра",
        "schedule": "Schedule" if english else "Расписание",
        "empty": "No events." if english else "Событий нет.",
        "more": "and {n} more" if english else "и ещё {n}",
        "anon": "(no title)" if english else "(без названия)",
    }

    try:
        now = datetime.fromisoformat(plugin.now.replace("Z", "+00:00")) if plugin.now else datetime.now(timezone.utc)
    except ValueError:
        now = datetime.now(timezone.utc)
    now = now.astimezone(zone)

    today = now.date()
    question = plugin.params.get("question", "")
    days = int(plugin.params.get("days", "3"))
    first_day, days, scope = scope_of(question, days, now.date())
    window_start = datetime.combine(first_day, time(), tzinfo=zone)
    window_end = window_start + timedelta(days=days)

    try:
        text = read_source(plugin.params["url"]).decode("utf-8", "replace")
    except Exception as error:  # noqa: BLE001 - any failure is a failed run
        plugin.fail(f"agenda: {error}")

    starts = []
    for event in events_of(text, zone):
        for occurrence in expand(event, window_start, window_end, zone):
            starts.append((occurrence, event))
    starts.sort(key=lambda pair: (pair[0], pair[1]["summary"]))

    def day_label(day):
        """The weekday that names the row's day, or nothing for a single-day
        card, whose heading already names it. A weekday is short and is
        unambiguous inside a week, so it fits every row of a multi-day card."""
        if days == 1:
            return ""
        return WEEKDAYS[language][day.weekday()]

    rows = []
    for occurrence, event in starts:
        all_day = isinstance(event["start"], date) and not isinstance(event["start"], datetime)
        day = occurrence.date() if all_day else occurrence.astimezone(zone).date()
        label = day_label(day)
        lead = f"{label} " if label else ""
        if all_day:
            rows.append({"text": f"{lead}— {event['summary']}"})
        else:
            rows.append({"text": f"{lead}{occurrence.astimezone(zone):%H:%M} {event['summary']}"})

    limit = int(plugin.params.get("max", "6"))
    extra = max(0, len(rows) - limit)
    rows = rows[:limit]

    if not rows:
        title = plugin.params.get("title", "").strip() or {
            "tomorrow": words["tomorrow"],
            "today": words["today"],
        }.get(scope, words["schedule"])
        body = words["empty"]
        if not english:
            body = "Событий нет." if scope != "week" else "Событий нет на неделю."
        say = None
        if question:
            say = f"{title}: {words['empty']}" if english else f"{title}: {body}"
        plugin.notice(title, body, accent="yellow", state={"digest": None}, say=say)
    title = plugin.params.get("title", "").strip()
    if not title:
        if scope == "tomorrow":
            title = words["tomorrow"]
        elif scope == "today" or days == 1:
            title = words["today"]
        elif english:
            title = f"Schedule, {days} days"
        else:
            title = f"Расписание на {days} {plural_ru(days, ['день', 'дня', 'дней'])}"
    if extra:
        rows.append({"text": "…" + words["more"].format(n=extra)})

    # Same card as last time: nothing new to draw.
    digest = hashlib.sha256(json.dumps([title, rows], ensure_ascii=False).encode()).hexdigest()
    if plugin.state.get("digest") == digest:
        plugin.nothing(state=plugin.state)

    # Ask for a run when the first event has started, so that a row that is
    # over does not stay on the panel until the next scheduled run.
    next_run = 1800
    for occurrence, _ in starts:
        if occurrence > now:
            next_run = min(3600, max(300, int((occurrence - now).total_seconds())))
            break

    say = None
    if question:
        mention = {"tomorrow": words["tomorrow"], "today": words["today"], "week": title}.get(scope, title)
        if english:
            say = f"Showing {mention.lower()}."
        else:
            say = f"Показываю: {mention.lower()}."
    plugin.items(title, rows, accent="blue", state={"digest": digest}, next_run_s=next_run, say=say)


if __name__ == "__main__":
    main()
