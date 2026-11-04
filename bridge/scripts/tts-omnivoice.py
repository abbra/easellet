#!/usr/bin/env python3
"""Synthesize stdin through a local OmniVoice server as 16 kHz mono PCM."""

import json
import os
from pathlib import Path
import subprocess
import sys
import urllib.error
import urllib.request


def main():
    text = sys.stdin.read().strip()
    if not text:
        raise ValueError("tts-omnivoice: empty input")
    detected = subprocess.run(
        [sys.executable, str(Path(__file__).with_name("detect-lang.py"))],
        input=text, text=True, stdout=subprocess.PIPE, check=True,
    ).stdout.strip()
    hint = os.environ.get("LANGS", "").split("-")[0].lower()
    supported = {"ru", "en", "fi", "fr"}
    lang = detected if detected in supported else hint
    if lang not in supported:
        lang = "ru"
    url = os.environ.get("EASEL_OMNIVOICE_URL", "http://127.0.0.1:8099/v1/audio/speech")
    request = urllib.request.Request(
        url,
        data=json.dumps({"input": text, "language": lang, "response_format": "wav"}).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=55) as response:
            wav = response.read()
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"tts-omnivoice: server returned {error.code}: {error.read(512)!r}") from error
    print(f"tts-omnivoice: lang={lang}", file=sys.stderr)
    subprocess.run(
        ["ffmpeg", "-nostdin", "-loglevel", "error", "-f", "wav", "-i", "pipe:0",
         "-af", "volume=0.85", "-ar", "16000", "-ac", "1", "-f", "s16le", "pipe:1"],
        input=wav, stdout=sys.stdout.buffer, check=True,
    )


if __name__ == "__main__":
    main()
