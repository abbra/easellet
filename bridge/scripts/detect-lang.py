#!/usr/bin/env python3
"""Print one of `ru`, `en`, `fi`, `fr` or `unknown` for the text on stdin.

Used by the Piper script when the bridge did not supply a language. `langdetect` is
good on a sentence and unreliable on a single word, so Cyrillic is handled directly
and unsupported or low-confidence guesses become `unknown`, so the caller can keep
the recogniser's language hint. The seed makes repeated runs deterministic, which
matters when a voice is chosen from the text.
"""

from __future__ import annotations

import re
import sys

from langdetect import DetectorFactory, detect_langs

DetectorFactory.seed = 0
SUPPORTED = {"ru", "en", "fi", "fr"}


def main() -> int:
    text = sys.stdin.read().strip()
    if re.search(r"[А-Яа-яЁё]", text):
        print("ru")
        return 0
    # `langdetect` will confidently call "Yes." French. A short, plain-ASCII reply
    # is exactly where the recogniser's language hint is more trustworthy, so keep
    # detection for sentences and for anything with a non-ASCII letter.
    if text.isascii() and (len(text.split()) < 3 or sum(c.isalpha() for c in text) < 12):
        print("unknown")
        return 0
    try:
        guesses = detect_langs(text)
    except Exception:
        print("unknown")
        return 0
    if guesses and guesses[0].prob >= 0.70 and guesses[0].lang in SUPPORTED:
        print(guesses[0].lang)
    else:
        print("unknown")
    return 0


if __name__ == "__main__":
    sys.exit(main())
