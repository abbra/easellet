#!/usr/bin/env python3
"""Easellet bridge LLM backend: a local llama.cpp server, answering in the language asked.

The bridge runs `cmd:` backends as a program reading the prompt on stdin and writing
the answer on stdout, so this is a plain filter in front of `llama-server`'s
OpenAI-compatible chat endpoint. Nothing leaves the machine: no provider key, no
account, no round trip past the LAN.

Why a server and not `llama-cli`: a 2.3 GB model takes seconds to map and warm up, and
`cmd:llama-cli` would pay that for every sentence. `llama-server` keeps the weights
resident, so a short answer costs ~1.3 s instead of ~15 s.

Start the server once (the model is already on this host):

    ~/local/19cce691/llama-server \\
        -m /home/abokovoy/tmp/small-llms/gguf/Phi-4-mini-instruct-Q4_K_M.gguf \\
        --host 127.0.0.1 --port 8096 -c 4096 -t 24 --jinja

then point the bridge at it:

    ./bridge/build.sh serve -- --stt whisper:small \\
        --llm cmd:bridge/scripts/llm-local.py \\
        --tts cmd:bridge/scripts/tts-piper.sh

Crane is the alternative, and it does use this host's GPU (Radeon 890M, ROCm):

    ~/.local/bin/crane-serve \
        -m /home/abokovoy/tmp/small-llms/gguf/MiniCPM5-2B-Q4_K_M.gguf \
        --host 127.0.0.1 --port 8097 --context 4096

    EASEL_LLM_URL=http://127.0.0.1:8097/v1/chat/completions \
        ./bridge/build.sh serve -- --stt whisper:small \
        --llm cmd:bridge/scripts/llm-local.py

Crane loads only GGUF architectures it knows. Phi-4-mini is `phi3`, which it rejects with
`Unrecognized GGUF architecture 'phi3'` and then a demand for a sibling `tokenizer.json`;
`minicpm5` and `gemma4` load, and those are what the GPU path starts with. Crane also
insists on a `model` field that llama-server ignores, so the name is discovered from
`/v1/models` — see `model_name`.

Model choice, measured on this host (24 cores, no GPU) with the Russian prompt
"Почему небо голубое?":

    Phi-4-mini-instruct Q4_K_M   2.3 GB   1.3 s   "Это связано с длиной волн солнечного света."
    granite-4.2-3b Q4_K_M        2.1 GB   9.6 s   80 tokens of English reasoning, no answer
    LFM2.5-8B-A1B Q4_K_M         4.8 GB   5.7 s   same: reasoning_content only
    T-lite-instruct 8B Q4_K_M    4.6 GB   ~20 s   2.8 t/s, unusable for speech
    Qwen3.8-27B Q4_K_M          15.9 GB   —       no GPU, far too slow

The two instruct models that "thought" before answering are exactly wrong here: the
frame wants one sentence, and a reasoning trace spends the whole budget on words it
then reads aloud. Hence a non-reasoning model, and a system prompt that forbids lists
and markdown — the reply is spoken and drawn on a Spectre-6 panel, both of which
punish structure that a chat window rewards.

Environment: EASEL_LLM_URL (default http://127.0.0.1:8096/v1/chat/completions),
EASEL_LLM_MODEL (discovered from `/v1/models`, then "local"), EASEL_LLM_SYSTEM,
EASEL_LLM_MAX_TOKENS (96), EASEL_LLM_TEMPERATURE (0.4), EASEL_LLM_TIMEOUT (60).
"""

from __future__ import annotations

import json
import os
import re
import sys
import urllib.error
import urllib.request

try:
    from langdetect import DetectorFactory, detect
except ImportError:  # detection is a hint; the prompt still names every language
    detect = None
else:
    DetectorFactory.seed = 0

URL = os.environ.get("EASEL_LLM_URL", "http://127.0.0.1:8096/v1/chat/completions")
SYSTEM = os.environ.get(
    "EASEL_LLM_SYSTEM",
    "Ты — голосовой помощник в фоторамке. Отвечай на языке вопроса — русском, "
    "финском, английском или французском — кратко: одно или два предложения. "
    "Если просят перевести или объяснить слово, можешь ответить на другом языке. "
    "Без списков, без markdown, без формул и без пояснений о себе.",
)
MODEL = os.environ.get("EASEL_LLM_MODEL", "")
MAX_TOKENS = int(os.environ.get("EASEL_LLM_MAX_TOKENS", "96"))
TEMPERATURE = float(os.environ.get("EASEL_LLM_TEMPERATURE", "0.4"))
TIMEOUT = float(os.environ.get("EASEL_LLM_TIMEOUT", "60"))

LANGUAGES = {
    "ru": "Russian",
    "fi": "Finnish",
    "en": "English",
    "fr": "French",
}


def question_language(prompt: str) -> str:
    """One of `ru`, `fi`, `en`, `fr`.

    The model is instruction-following but has a strong Russian bias from the
    system prompt, and a short English question was answered in Russian until the
    language was named explicitly. Cyrillic is unambiguous; the rest goes through
    `langdetect`, with English as the fallback for an unsupported language.
    """
    if re.search(r"[А-Яа-яЁё]", prompt):
        return "ru"
    if detect is None:
        return "ru"
    try:
        code = detect(prompt)
    except Exception:
        return "en"
    return code if code in LANGUAGES else "en"


def fail(message: str) -> int:
    print(f"llm-local: {message}", file=sys.stderr)
    return 1


def tidy(text: str) -> str:
    """One paragraph, no markdown, no wrapping quotes.

    The answer is spoken by piper and painted on the panel, so a `**` or a bullet
    would be read out literally or blitted as punctuation. Whisper also tends to
    transcribe question marks into the prompt, which models like to mirror.
    """
    text = " ".join(text.split())
    for marker in ("**", "__", "`"):
        text = text.replace(marker, "")
    while text.startswith(("- ", "* ", "# ")):
        text = text[2:]
    if len(text) > 1 and text[0] in "«\"'" and text[-1] in "»\"'":
        text = text[1:-1].strip()
    return text


def model_name() -> str:
    """What the server calls itself.

    Crane rejects a payload without `model`; llama-server ignores the field. Both answer
    `/v1/models` but in different shapes (`data[].id` versus `models[].name`), so ask the
    server instead of making the operator keep two spellings in sync.
    """
    if MODEL:
        return MODEL
    catalog = URL.rsplit("/", 1)[0] + "/models"
    try:
        with urllib.request.urlopen(catalog, timeout=5) as response:
            body = json.load(response)
    except (OSError, ValueError):
        return "local"
    if not isinstance(body, dict):
        return "local"
    for key in ("data", "models"):
        entries = body.get(key)
        if isinstance(entries, list) and entries and isinstance(entries[0], dict):
            for field in ("id", "model", "name"):
                value = entries[0].get(field)
                if isinstance(value, str) and value:
                    return value
    return "local"


def main() -> int:
    prompt = sys.stdin.read().strip()
    if not prompt:
        return fail("empty prompt")

    lang = question_language(prompt)
    system = (
        f"{SYSTEM} The question is in {LANGUAGES[lang]}; answer in "
        f"{LANGUAGES[lang]} unless the question explicitly asks for a translation."
    )
    payload = {
        "model": model_name(),
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": prompt},
        ],
        "max_tokens": MAX_TOKENS,
        "temperature": TEMPERATURE,
        "stream": False,
    }
    request = urllib.request.Request(
        URL,
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
            body = json.load(response)
    except urllib.error.HTTPError as error:
        return fail(f"{URL} answered {error.code}: {error.read()[:200]!r}")
    except OSError as error:
        return fail(
            f"cannot reach {URL} ({error}); start it with "
            "llama-server -m <model.gguf> --host 127.0.0.1 --port 8096 -c 4096 --jinja"
        )

    try:
        message = body["choices"][0]["message"]
    except (KeyError, IndexError, TypeError):
        return fail(f"unexpected reply shape: {json.dumps(body)[:200]}")

    text = tidy(message.get("content") or "")
    if not text:
        # Reasoning models answer into `reasoning_content` and leave `content` empty;
        # saying so is more useful than a bare "empty reply", because the fix is the
        # model, not the wiring.
        thinking = message.get("reasoning_content") or ""
        if thinking:
            return fail(
                "model spent its budget reasoning and produced no answer; "
                "use a non-reasoning instruct model"
            )
        return fail("model produced an empty answer")

    # The frame's `ai.model` is a hint the bridge cannot act on here — the model is
    # whatever `llama-server` has loaded — so name the real one on stderr and let the
    # access log carry both.
    timings = body.get("timings") or {}
    name = os.path.basename(str(body.get("model", "?")))
    tokens = (body.get("usage") or {}).get("completion_tokens", "?")
    speed = timings.get("predicted_per_second")
    detail = f" speed={speed:.1f}t/s" if isinstance(speed, (int, float)) else ""
    print(
        f"llm-local: model={name} lang={lang} tokens={tokens}{detail}",
        file=sys.stderr,
    )

    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
