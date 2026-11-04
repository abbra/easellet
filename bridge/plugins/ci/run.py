#!/usr/bin/env python3
"""The latest open pull requests of a GitHub or Forgejo repository and their checks.

A PR's state is that of its head commit. GitHub gets CI reports in two places:
check runs (GitHub Actions and other GitHub Apps) and commit statuses (external
CI such as Jenkins or Buildkite). Forgejo has one: its Actions write a commit
status for every job, the way GitHub's external CI does. A failure anywhere wins
(red), then anything still running (yellow), then success (green); a PR nobody
reported on is blue, and one not checked yet has no square.
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

# What each forge is called in a message to the person who reads the run log.
FORGES = {"github": "GitHub", "forgejo": "Forgejo"}
# Conclusions of a GitHub check run that are not a pass, and its status states
# together with Forgejo's two more; a warning or a skip is not a verdict.
CHECK_FAILED = {"failure", "timed_out", "action_required", "startup_failure"}
STATUS = {
    "success": "ok",
    "pending": "running",
    "failure": "fail",
    "error": "fail",
    "warning": "other",
    "skipped": "other",
}
# Check runs come in pages of 100; a PR with hundreds of jobs is still counted
# whole, up to this many pages. The combined status answers in pages of its own.
MAX_PAGES = 5
STATUS_PAGE = 50

plugin = Plugin.read()
repo = plugin.params["repo"].strip()
if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
    plugin.fail(f"ci: {repo!r} is not owner/name")
forge = plugin.params.get("forge", "github").strip()
if forge not in FORGES:
    plugin.fail(f"ci: {forge!r} is not a forge; use one of {', '.join(FORGES)}")
count = int(plugin.params.get("count", "6"))
token = plugin.params.get("token")
api = plugin.params.get("api", "").rstrip("/") or "https://api.github.com"
# A Forgejo instance has its own address, and no default can stand for it.
if forge == "forgejo" and api == "https://api.github.com":
    plugin.fail("ci: a Forgejo repository needs api, for example https://codeberg.org/api/v1")
if forge == "forgejo":
    # Forgejo and Gitea take the token as `token`, and know nothing of GitHub's
    # media type; both answer with the same `message` when they refuse.
    headers = {"Accept": "application/json", "User-Agent": "easellet-bridge"}
    if token:
        headers["Authorization"] = f"token {token}"
else:
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "easellet-bridge"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
# Two requests per PR whose result is not known yet; without a token GitHub
# allows 60 an hour, so only a few PRs are checked per run and the rest wait.
# A Forgejo instance has no such limit, so every PR on its card is checked.
budget = count if token or forge == "forgejo" else 3
# head sha -> counts; only finished commits (nothing running) are kept.
known = dict(plugin.state.get("checks", {}))


class Refused(Exception):
    """The forge refused a request; carries its own explanation."""


# When the token stops working, from GitHub's own header on every answer
# ("2026-12-01 10:00:00 +0100"); organisations such as freeipa cap token
# lifetimes, so expiry is routine and worth a warning before it happens.
# Forgejo reports no expiry, so there the warning is skipped rather than guessed.
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
        # personal access token ...", "Resource not accessible ...", rate limit),
        # and so does Forgejo ("token does not have at least one of required
        # scopes ..."); without it a 403 is a guess.
        try:
            reason = json.load(error).get("message", "")
        except Exception:  # noqa: BLE001
            reason = ""
        where = path.split("?")[0]
        raise Refused(f"{FORGES[forge]} {error.code} on {where}: {reason or error.reason}") from None


def statuses(sha):
    """The latest state of each context that reported on the commit.

    Both forges answer `commits/<sha>/status` with one entry per context, so a
    context that passed and then failed is one row and not two. Forgejo pages
    that answer and names the page size `limit`; GitHub ignores the name and
    pages by 30, which the loop still walks because `total_count` is given.
    """
    found = []
    page = 1
    while True:
        answer = get(f"commits/{sha}/status?limit={STATUS_PAGE}&page={page}")
        entries = answer.get("statuses") or []
        found.extend(entries)
        if not entries or len(found) >= (answer.get("total_count") or 0):
            return found
        page += 1


def state_of(status):
    """A commit status's state: GitHub spells it `state`, Forgejo `status`."""
    return status.get("state") or status.get("status") or ""


def tally(sha):
    """How the commit's checks ended: {"fail", "running", "ok", "other"}.

    On GitHub check runs (Actions and other apps) and commit statuses (external
    CI) both count. On Forgejo every job of its own Actions is a commit status,
    so there are no check runs to read.
    """
    counts = {"fail": 0, "running": 0, "ok": 0, "other": 0}
    if forge == "github":
        for page in range(1, MAX_PAGES + 1):
            answer = get(f"commits/{sha}/check-runs?per_page=100&page={page}")
            runs = answer.get("check_runs", [])
            for run in runs:
                if run["status"] != "completed":
                    counts["running"] += 1
                elif run["conclusion"] in CHECK_FAILED:
                    counts["fail"] += 1
                elif run["conclusion"] in {"success"}:
                    counts["ok"] += 1
                else:
                    counts["other"] += 1
            if page * 100 >= answer.get("total_count", 0) or not runs:
                break
    for status in statuses(sha):
        counts[STATUS.get(state_of(status), "other")] += 1
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


if forge == "forgejo":
    query = {"state": "open", "sort": "recentupdate", "limit": count}
else:
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
if forge == "github" and expires:
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
