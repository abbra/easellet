"""Place lookup for plugins that ask Open-Meteo for data.

`locate` turns a configured place into coordinates, and `place_from_question`
finds the place that a spoken question names, undoing Russian case endings by
trying base forms against the geocoder. A place that the geocoder does not know
comes back as the words themselves, so that a plugin can say so instead of
showing another place without saying it.

Standard library only, like the rest of the SDK.
"""

import json
import urllib.parse
import urllib.request

AGENT = "easellet-bridge"
PREPOSITIONS = {"в", "во", "на", "для", "у", "in", "at", "for"}
# A place may also follow one of these words directly, without a preposition,
# when the recogniser swallowed the preposition («покажи погоду ВСПУ» for
# «в Эспоо»). Only a capitalised name counts after them, so «погода будет
# завтра» names no place.
TRIGGER_WORDS = {"погоду", "погода", "погоды", "weather"}
# Words that belong to the question itself, so they never name a place even
# when the geocoder knows a place with that name.
QUESTION_WORDS = {
    "сколько", "какой", "какая", "какое", "какие", "что", "чем", "где", "когда",
    "почему", "зачем", "как", "куда", "откуда", "кто", "который", "которая",
    "меня", "тебя", "нас", "вас", "него", "нее", "них", "себе",
    "what", "when", "where", "why", "how", "which", "who",
}
TIME_WORDS = {
    "завтра", "сегодня", "послезавтра", "неделю", "неделе", "выходные", "выходных",
    "утро", "утром", "вечер", "вечером", "ближайшие", "дни", "дней",
    "tomorrow", "today", "week", "weekend", "the", "next",
}


def fetch(url, timeout=10):
    """The JSON answer of one address, with the bridge's user agent."""
    request = urllib.request.Request(url, headers={"User-Agent": AGENT})
    with urllib.request.urlopen(request, timeout=timeout) as answer:
        return json.load(answer)


def fold(text):
    return text.lower().replace("ё", "е")


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


def place_groups(question):
    """Every group of words that may name a place, longest first.

    «во сколько закат в Тампере» gives [«сколько закат», «сколько», «Тампере»]:
    the words after «во» are not a place, and only the geocoder can say so, so
    each group is tried and the first one that is a place wins. «покажи погоду в
    Эспоо на завтра» gives [«Эспоо»]: a preposition followed by a time word
    («на завтра») introduces no place. «покажи погоду Эспоо» gives [«Эспоо»],
    because a capitalised name may follow the trigger word itself.
    """
    words = [word.strip(".,!?;:«»\"'()") for word in (question or "").split()]
    words = [word for word in words if word]
    groups = []
    for index, word in enumerate(words):
        if fold(word) in TRIGGER_WORDS:
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
        for length in range(len(place), 0, -1):
            groups.append(place[:length])
    return groups


def place_from_question(question, state):
    """The place a spoken question names: a dict if the geocoder knows it, the
    words as text if the question names a place that it does not know, None if
    the question names no place."""
    cached = state.setdefault("asked", {})
    unknown = None
    for words in place_groups(question):
        key = " ".join(words)
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
        # A group that is not part of the question itself may still name a
        # place that the geocoder does not know; the plugin then says so
        # instead of showing another place. A recogniser often writes no
        # capital letters, so the words decide, not the case.
        if unknown is None and not any(fold(word) in QUESTION_WORDS for word in words):
            unknown = key
    return unknown


def locate(place, state):
    """Coordinates and a display name, from 'lat,lon' or by name (cached in state).

    The geocoder answers in the script of the name that it is given: a Latin name
    comes back in English, a Cyrillic one in Russian, so «Helsinki» stays Latin in
    an English interface and «Хельсинки» stays Cyrillic in a Russian one.
    """
    try:
        latitude, longitude = (float(part) for part in place.split(","))
        return latitude, longitude, place
    except ValueError:
        pass
    cached = state.get("place")
    if cached and cached.get("query") == place:
        return cached["latitude"], cached["longitude"], cached["name"]
    language = "en" if any("a" <= letter <= "z" for letter in fold(place)) else "ru"
    query = urllib.parse.urlencode({"name": place, "count": 1, "language": language, "format": "json"})
    found = fetch(f"https://geocoding-api.open-meteo.com/v1/search?{query}").get("results")
    if not found:
        raise LookupError(f"место «{place}» не найдено")
    hit = found[0]
    state["place"] = {
        "query": place,
        "latitude": hit["latitude"],
        "longitude": hit["longitude"],
        "name": hit["name"],
    }
    return hit["latitude"], hit["longitude"], hit["name"]
