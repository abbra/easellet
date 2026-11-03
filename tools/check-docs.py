#!/usr/bin/env python3
"""Check the committed Markdown: fences balance, no tabs, every link resolves.

A broken relative link in a README is invisible until somebody follows it, and
an unclosed code fence silently swallows the rest of the page on GitHub. Both
are cheap to catch and neither is caught by a build.

    tools/check-docs.py          # every tracked *.md
    tools/check-docs.py -v       # name each file it read

Only files git knows about are read, so the notes in `research/` (not committed)
are outside the check on purpose.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
_FENCE = re.compile(r"^\s{0,3}```")
# [text](target) and ![alt](target); the target may carry a title after a space.
_LINK = re.compile(r"!?\[[^\]]*\]\(\s*([^)\s]+)(?:\s+\"[^\"]*\")?\s*\)")
_SKIP_SCHEME = ("http://", "https://", "mailto:", "tel:", "#")


def git_ls_files(pattern: str) -> str:
    """List the tracked files matching `pattern`, tolerating a foreign checkout.

    A container job usually checks the tree out as one user and reads it as
    another, and git then refuses to touch the repository at all unless it is
    told the directory is safe. The directory is the one this script lives in,
    so the only thing being trusted is the caller's own checkout. The retry is
    narrow on purpose: every other failure is reported as it came.
    """
    def run(*options: str) -> subprocess.CompletedProcess[str]:
        try:
            return subprocess.run(
                ["git", "-C", str(ROOT), *options, "ls-files", "-z", pattern],
                capture_output=True, text=True,
            )
        except OSError as error:
            sys.exit(f"ERR git ls-files: {error}")

    result = run()
    if result.returncode != 0 and "dubious ownership" in result.stderr:
        result = run("-c", "safe.directory=*")
    if result.returncode != 0:
        reason = result.stderr.strip() or f"exit status {result.returncode}"
        sys.exit(f"ERR git ls-files: {reason}")
    return result.stdout


def tracked_markdown() -> list[Path]:
    listed = git_ls_files("*.md")
    return [ROOT / name for name in listed.split("\0") if name]


def check(path: Path) -> list[str]:
    problems: list[str] = []
    text = path.read_text(encoding="utf-8")
    where = path.relative_to(ROOT)

    fences = 0
    in_fence = False
    in_front_matter = False
    for number, line in enumerate(text.splitlines(), start=1):
        if number == 1 and line.strip() == "---":
            in_front_matter = True
            continue
        if in_front_matter:
            if line.strip() == "---":
                in_front_matter = False
            continue
        if "\t" in line:
            problems.append(f"{where}:{number}: tab character")
        if _FENCE.match(line):
            fences += 1
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        for target in _LINK.findall(line):
            if target.startswith(_SKIP_SCHEME):
                continue
            if target.startswith("/"):
                problems.append(f"{where}:{number}: absolute link {target}")
                continue
            resolved = (path.parent / target.split("#")[0]).resolve()
            if not resolved.exists():
                problems.append(f"{where}:{number}: link goes nowhere: {target}")

    if fences % 2:
        problems.append(f"{where}: {fences} fence markers, so one is unclosed")
    if not text.endswith("\n"):
        problems.append(f"{where}: no newline at end of file")
    return problems


def main() -> int:
    verbose = "-v" in sys.argv[1:]
    problems: list[str] = []
    files = tracked_markdown()
    for path in files:
        problems.extend(check(path))
        if verbose:
            print(f"read {path.relative_to(ROOT)}")
    if problems:
        for problem in problems:
            print(problem, file=sys.stderr)
        print(f"ERR {len(problems)} documentation problem(s)", file=sys.stderr)
        return 1
    print(f"docs: {len(files)} files, fences balanced, links resolve")
    return 0


if __name__ == "__main__":
    sys.exit(main())
