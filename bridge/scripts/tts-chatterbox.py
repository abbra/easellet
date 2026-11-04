#!/usr/bin/env python3
"""Speak stdin with Chatterbox, writing 16 kHz mono signed PCM to stdout."""

import contextlib
import os
from pathlib import Path
import subprocess
import sys


def language(detected, hint):
    supported = {"ru", "en", "fi", "fr"}
    hint = hint.split("-")[0].lower()
    if detected in supported:
        return detected
    return hint if hint in supported else "ru"


def main():
    text = sys.stdin.read().strip()
    if not text:
        raise ValueError("tts-chatterbox: empty input")

    detected = subprocess.run(
        [sys.executable, str(Path(__file__).with_name("detect-lang.py"))],
        input=text, text=True, stdout=subprocess.PIPE, check=True,
    ).stdout.strip()
    lang = language(detected, os.environ.get("LANGS", ""))

    # Model libraries may print to stdout; stdout is reserved for raw audio.
    with contextlib.redirect_stdout(sys.stderr):
        import torch
        import torchaudio.functional
        from chatterbox.mtl_tts import ChatterboxMultilingualTTS

        device = "cuda" if torch.cuda.is_available() else "cpu"
        print(f"tts-chatterbox: lang={lang} device={device}", file=sys.stderr)
        model = ChatterboxMultilingualTTS.from_pretrained(device=device, t3_model="v3")
        with torch.inference_mode():
            samples = model.generate(text, language_id=lang)
            samples = torchaudio.functional.resample(samples, model.sr, 16000)
            pcm = samples.reshape(-1).clamp(-1, 1).mul(0.85 * 32767).to(torch.int16)
            audio = pcm.cpu().numpy().astype("<i2", copy=False).tobytes()
    sys.stdout.buffer.write(audio)


if __name__ == "__main__":
    main()
