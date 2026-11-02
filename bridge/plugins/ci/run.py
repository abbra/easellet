#!/usr/bin/env python3
"""The latest open pull requests of a GitHub repository and the state of their checks.

A PR's state is that of its head commit, from both places CI reports to:
check runs (GitHub Actions and other GitHub Apps) and commit statuses
(external CI such as Jenkins or Buildkite). A failure anywhere wins (red), then
anything still running (yellow), then success (green); a PR nobody reported on
is blue, and one not checked yet has no square.
"""

import hashlib
from datetime import datetime, timezone
import json
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "sdk"))
from easellet_plugin import Plugin  # noqa: E402

FAILED = {"failure", "timed_out", "action_required", "startup_failure"}
PASSED = {"success"}
# Check runs come in pages of 100; a PR with hundreds of jobs is still counted
# whole, up to this many pages.
MAX_PAGES = 5

plugin = Plugin.read()
repo = plugin.params["repo"].strip()
if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
    plugin.fail(f"ci: {repo!r} is not owner/name")
count = int(plugin.params.get("count", "6"))
token = plugin.params.get("token")
headers = {"Accept": "application/vnd.github+json", "User-Agent": "easellet-bridge"}
if token:
    headers["Authorization"] = f"Bearer {token}"
# Two requests per PR whose result is not known yet; without a token GitHub
# allows 60 an hour, so only a few PRs are checked per run and the rest wait.
budget = count if token else 3
api = plugin.params.get("api", "https://api.github.com").rstrip("/")
# head sha -> counts; only finished commits (nothing running) are kept.
known = dict(plugin.state.get("checks", {}))


class GitHubError(Exception):
    """GitHub refused a request; carries its own explanation."""


# When the token stops working, from GitHub's own header on every answer
# ("2026-12-01 10:00:00 +0100"); organisations such as freeipa cap token
# lifetimes, so expiry is routine and worth a warning before it happens.
expires = None
WARN_DAYS = 14


def get(path):
    global expires
    request = urllib.request.Request(f"{api}/repos/{repo}/{path}", headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=10) as answer:
            expires = answer.headers.get("github-authentication-token-expiration") or expires
            return json.load(answer)
    except urllib.error.HTTPError as error:
        # GitHub says why in the body ("... forbids access via a fine-grained
        # personal access token ...", "Resource not accessible ...", rate limit);
        # without it a 403 is a guess.
        try:
            reason = json.load(error).get("message", "")
        except Exception:  # noqa: BLE001
            reason = ""
        where = path.split("?")[0]
        raise GitHubError(f"GitHub {error.code} on {where}: {reason or error.reason}") from None


def tally(sha):
    """How the commit's checks ended: {"fail", "running", "ok", "other"}.

    Check runs (GitHub Actions and other apps) and commit statuses (external
    CI) both count; a status is one entry per context, its latest state.
    Cancelled, skipped and neutral runs are "other": not a verdict either way.
    """
    counts = {"fail": 0, "running": 0, "ok": 0, "other": 0}
    for page in range(1, MAX_PAGES + 1):
        answer = get(f"commits/{sha}/check-runs?per_page=100&page={page}")
        runs = answer.get("check_runs", [])
        for run in runs:
            if run["status"] != "completed":
                counts["running"] += 1
            elif run["conclusion"] in FAILED:
                counts["fail"] += 1
            elif run["conclusion"] in PASSED:
                counts["ok"] += 1
            else:
                counts["other"] += 1
        if page * 100 >= answer.get("total_count", 0) or not runs:
            break
    for status in get(f"commits/{sha}/status").get("statuses", []):
        key = {"success": "ok", "pending": "running"}.get(status["state"], "fail")
        counts[key] += 1
    return counts


def verdict(counts):
    """The row's square: failed beats running beats passed."""
    if counts["fail"]:
        return "fail"
    if counts["running"]:
        return "warn"
    if counts["ok"] or counts["other"]:
        return "ok"
    return "info"


query = {"state": "open", "sort": "updated", "direction": "desc", "per_page": count}
if plugin.params.get("base"):
    query["base"] = plugin.params["base"]
try:
    pulls = get(f"pulls?{urllib.parse.urlencode(query)}")
except Exception as error:  # noqa: BLE001
    plugin.fail(f"ci: {error}")

items = []
for pull in pulls:
    sha = pull["head"]["sha"]
    counts = known.get(sha)
    if counts is None and budget > 0:
        budget -= 1
        try:
            counts = tally(sha)
        except Exception as error:  # noqa: BLE001 - one PR's error is not the board's
            print(f"ci: #{pull['number']}: {error}", file=sys.stderr)
            counts = None
        if counts is not None and not counts["running"]:
            known[sha] = counts
    draft = "черновик · " if pull.get("draft") else ""
    text = f"#{pull['number']} {draft}{pull['title']}"
    # Not checked yet (the request budget ran out): no square and no numbers,
    # rather than a colour that would claim something about its checks.
    if counts is None:
        items.append(text)
    else:
        items.append({"text": text, "status": verdict(counts), "counts": counts})
if not items:
    items = [("открытых PR нет", "info")]
if expires:
    try:
        left = (datetime.strptime(expires, "%Y-%m-%d %H:%M:%S %z") - datetime.now(timezone.utc)).days
    except ValueError:
        left = None
    if left is not None and left < WARN_DAYS:
        items.append((f"Токен GitHub истекает через {max(left, 0)} дн.", "warn"))

# Keep the cache to the commits still on the board.
heads = {pull["head"]["sha"] for pull in pulls}
known = {sha: state for sha, state in known.items() if sha in heads}
digest = hashlib.sha256(json.dumps(items, ensure_ascii=False).encode()).hexdigest()
state = {"digest": digest, "checks": known}
if plugin.state.get("digest") == digest:
    plugin.nothing(state=state)
failing = any(isinstance(item, dict) and item["status"] == "fail" for item in items)
plugin.items(f"PR: {repo}", items, accent="red" if failing else "green", state=state)
