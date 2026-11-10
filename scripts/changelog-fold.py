#!/usr/bin/env python3
"""Fold changelog/unreleased/*.md into CHANGELOG.md under ``## [Unreleased]``.

Usage: scripts/changelog-fold.py [--check]

- ``--check`` is a dry-run: print ``would fold <file>`` per fragment, exit 0,
  mutate nothing (but fail on malformed fragments).
- Without flags, fold newest-first by first-commit time, then delete fragments.
- Empty fragments: print ``no fragments``, exit 0.

Order is each fragment's first-commit time (git log --diff-filter=A),
else the date in its filename.
"""
from __future__ import annotations

import calendar
import glob
import os
import re
import subprocess
import sys
import time
from pathlib import Path

UNRELEASED_HEADING: str = "## [Unreleased]"
UNRELEASED_DIR: str = "changelog/unreleased"
CHANGELOG: str = "CHANGELOG.md"
USAGE: str = "Usage: python3 scripts/changelog-fold.py [--check]"

# Fragment well-formedness: one top-level bullet, no absolute paths, no secrets.
BULLET_RE: re.Pattern[str] = re.compile(r"^-\s+")
SECRET_RES: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("absolute-path", re.compile(r"/Users/|/private/var/")),  # ncui-allow-hygiene: the pattern itself, not a path
    ("windows-path", re.compile(r"[A-Za-z]:\\Users\\|[A-Za-z]:\\nc-")),
    ("session", re.compile(r"ses_[A-Za-z0-9]{20}")),
    ("token", re.compile(r"ghp_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{22,}")),
    ("api-key", re.compile(r"sk-[A-Za-z0-9]{20,}")),
)


def repo_root() -> Path:
    try:
        out = subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True, encoding="utf-8",
            stderr=subprocess.DEVNULL,
        ).strip()
        if out:
            return Path(out)
    except (subprocess.CalledProcessError, FileNotFoundError, OSError):
        pass
    return Path.cwd()


def stamp(path: str) -> int:
    """First-commit unix time, else filename YYYY-MM-DD, else now."""
    try:
        out = subprocess.run(
            ["git", "log", "--diff-filter=A", "--format=%ct", "-1", "--", path],
            capture_output=True, text=True, encoding="utf-8", check=True,
        ).stdout.strip()
        if out:
            return int(out)
    except (subprocess.CalledProcessError, FileNotFoundError, ValueError, OSError):
        pass
    base = os.path.basename(path)
    m = re.match(r"(\d{4})-(\d{2})-(\d{2})", base)
    if m:
        return calendar.timegm(
            (int(m[1]), int(m[2]), int(m[3]), 0, 0, 0)
        )
    return int(time.time())


def list_fragments() -> list[str]:
    return sorted(
        f
        for f in glob.glob(f"{UNRELEASED_DIR}/*.md")
        if os.path.basename(f) != "README.md"
    )


def validate_fragment(path: str) -> str | None:
    """Return an error string, or None when well-formed."""
    if not re.match(r"\d{4}-\d{2}-\d{2}-.+\.md", os.path.basename(path)):
        return "filename must be <YYYY-MM-DD>-<slug>.md"
    try:
        text = Path(path).read_text(encoding="utf-8", errors="ignore").lstrip("\ufeff")
    except OSError as exc:
        return f"unreadable: {exc}"
    has_bullet = any(
        BULLET_RE.match(line.lstrip("\ufeff")) for line in text.splitlines()
    )
    if not has_bullet:
        return "missing top-level bullet (line starting with '- ')"
    for name, pattern in SECRET_RES:
        if pattern.search(text):
            return f"contains banned {name} pattern"
    return None


def main(argv: list[str]) -> int:
    if "--help" in argv or "-h" in argv:
        print(USAGE)
        return 0
    check = "--check" in argv
    for arg in argv:
        if arg != "--check":
            print(f"unknown option: {arg}", file=sys.stderr)
            print(USAGE, file=sys.stderr)
            return 2

    root = repo_root()
    try:
        os.chdir(root)
    except OSError:
        pass

    frags = list_fragments()
    if not frags:
        print("no fragments")
        return 0

    if not Path(CHANGELOG).is_file():
        print(f"error: {CHANGELOG} not found (create it first)", file=sys.stderr)
        return 1

    frags.sort(key=lambda f: (-stamp(f), os.path.basename(f)))

    if check:
        failed = False
        for f in frags:
            err = validate_fragment(f)
            if err:
                print(f"error: {f}: {err}", file=sys.stderr)
                failed = True
            else:
                print(f"would fold {f}")
        if failed:
            return 1
        # Heading must exist even on dry-run.
        text = Path(CHANGELOG).read_text(encoding="utf-8", errors="ignore")
        if UNRELEASED_HEADING not in text.splitlines():
            print(
                f"error: {UNRELEASED_HEADING!r} heading not found in {CHANGELOG}",
                file=sys.stderr,
            )
            return 1
        return 0

    entries: list[str] = []
    for f in frags:
        entries.append(Path(f).read_text(encoding="utf-8", errors="ignore").strip("\n"))
    text = Path(CHANGELOG).read_text(encoding="utf-8", errors="ignore")
    lines = text.split("\n")
    if UNRELEASED_HEADING not in lines:
        print(
            f"error: {UNRELEASED_HEADING!r} heading not found in {CHANGELOG}",
            file=sys.stderr,
        )
        return 1
    i = lines.index(UNRELEASED_HEADING)
    j = i + 1
    while j < len(lines) and lines[j].strip() == "":
        j += 1
    # Keep the note line (and any other leading prose) above the entries.
    while j < len(lines) and lines[j] and not lines[j].startswith(("- ", "## ")):
        j += 1
    while j < len(lines) and lines[j].strip() == "":
        j += 1
    block = "\n\n".join(entries).split("\n") + [""]
    new = lines[:j] + block + lines[j:]
    Path(CHANGELOG).write_text("\n".join(new), encoding="utf-8")
    for f in frags:
        Path(f).unlink()
        print(f"folded {f}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
