#!/usr/bin/env python3
"""Folder-shape guard (Tier 1).

Keeps folders shaped by capability, not by type or accident:

    prefix family  x.rs or x/ plus 2+ x_*.rs siblings, or 3+ x_*.rs alone: error
    folder breadth > 40 non-test source files in one folder: warn; > 60: error

Files directly in examples/, benches/ and bin/ are skipped (each is its own
program). Test files are not counted. Prefix is the stem up to the first
_ or -. Extensions: .rs only in Tier 1 (nc-ui is Rust-first).

Usage: python3 scripts/folder-shape.py [--warn-only] [--update-exceptions [--allow-raise]]
"""
from __future__ import annotations

import os
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

WARN: int = 40
HARD: int = 60
FAM: int = 3
EXC: str = "scripts/folder-shape-exceptions.txt"
SRC: set[str] = {".rs"}
PROGRAMS: set[str] = {"examples", "benches", "bin"}
SCOPES: tuple[str, ...] = ("crates", "examples", "tools/ncui-tools/src", "packages")
TEST_STEM_RE: re.Pattern[str] = re.compile(r"(^|[_.-])tests?$")
PREFIX_RE: re.Pattern[str] = re.compile(r"([^_-]+)[_-]")
USAGE: str = (
    "Usage: python3 scripts/folder-shape.py [--warn-only] "
    "[--update-exceptions [--allow-raise]]"
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


def list_files() -> list[str]:
    """Tracked + untracked files in scope, repo-relative."""
    names: set[str] = set()
    try:
        proc = subprocess.run(
            ["git", "ls-files", "-z", "--cached", "--others",
             "--exclude-standard", "--", *SCOPES],
            capture_output=True, text=True, encoding="utf-8", check=True,
        )
        for entry in proc.stdout.split("\x00"):
            entry = entry.strip()
            if entry:
                names.add(entry.replace("\\", "/"))
    except (subprocess.CalledProcessError, FileNotFoundError, OSError):
        names = set()
    if names:
        return sorted(names)
    found: list[str] = []
    for scope in SCOPES:
        base = Path(scope)
        if base.is_dir():
            for path in base.rglob("*"):
                if path.is_file():
                    found.append(path.as_posix())
    return sorted(set(found))


def load_exceptions() -> dict[str, int]:
    out: dict[str, int] = {}
    exc = Path(EXC)
    if not exc.is_file():
        return out
    for raw in exc.read_text(encoding="utf-8", errors="ignore").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        out[parts[0]] = int(parts[1]) if len(parts) > 1 else 10**9
    return out


def collect() -> dict[str, tuple[list[str], set[str]]]:
    """Map dir -> (source stems, subdirs)."""
    files = list_files()
    dirs: dict[str, tuple[list[str], set[str]]] = defaultdict(
        lambda: ([], set())  # type: ignore[return-value]
    )
    for f in files:
        if not f:
            continue
        slash = f.rfind("/")
        if slash == -1:
            d = ""
            name = f
        else:
            d = f[:slash]
            name = f[slash + 1 :]
        parts = d.split("/") if d else []
        for i in range(2, len(parts)):
            parent = "/".join(parts[:i])
            dirs[parent][1].add(parts[i])
        stem, dot, ext_suf = name.rpartition(".")
        if not dot:
            continue
        ext = "." + ext_suf
        if parts and parts[-1] in PROGRAMS:
            continue
        if ext not in SRC:
            continue
        if TEST_STEM_RE.search(stem):
            continue
        if "/tests/" in "/" + d + "/":
            continue
        dirs[d][0].append(stem)
    return dirs


def find_violations() -> dict[str, tuple[int, str]]:
    """Map violation key -> (count, hint)."""
    dirs = collect()
    found: dict[str, tuple[int, str]] = {}
    for d, (stems, subs) in sorted(dirs.items()):
        by: dict[str, list[str]] = defaultdict(list)
        for s in stems:
            m = PREFIX_RE.match(s)
            if m:
                by[m.group(1)].append(s)
        for prefix, group in sorted(by.items()):
            base = prefix in stems or prefix in subs
            if len(group) >= (2 if base else FAM):
                found[f"{d}/{prefix}_*"] = (len(group), f"move into {prefix}/")
        if len(stems) > WARN:
            found[d + "/"] = (len(stems), "group into capability subfolders")
    return found


def main(argv: list[str]) -> int:
    if "--help" in argv or "-h" in argv:
        print(USAGE)
        return 0
    warn_only = "--warn-only" in argv
    update = "--update-exceptions" in argv
    raise_ok = "--allow-raise" in argv
    for arg in argv:
        if arg not in ("--warn-only", "--update-exceptions", "--allow-raise"):
            print(f"unknown option: {arg}", file=sys.stderr)
            print(USAGE, file=sys.stderr)
            return 2

    root = repo_root()
    try:
        os.chdir(root)
    except OSError:
        pass

    found = find_violations()
    exc = load_exceptions()

    if update:
        new: dict[str, int] = {}
        for key, (n, _hint) in sorted(found.items()):
            if not (key.endswith("_*") or n > HARD):
                continue
            if key not in exc:
                if raise_ok:
                    new[key] = n
                else:
                    print(
                        f"not listing new violation {key} ({n}); use --allow-raise",
                        file=sys.stderr,
                    )
            else:
                new[key] = n if (n <= exc[key] or raise_ok) else exc[key]
        lines: list[str] = [
            "# <key> <ceiling>: existing folder-shape violations "
            "(see scripts/folder-shape.sh). Ceilings only go down.",
            "# Refresh with scripts/folder-shape.sh --update-exceptions.",
        ]
        for key in sorted(new):
            lines.append(f"{key} {new[key]}")
        Path(EXC).write_text("\n".join(lines) + "\n", encoding="utf-8")
        print(f"wrote {EXC} ({len(new)} entries)")
        return 0

    errors = 0
    warns = 0
    families = 0
    gh = os.environ.get("GITHUB_ACTIONS") == "true"
    rows: list[str] = []
    over_warn = 0
    for key, (n, hint) in sorted(found.items()):
        fam = key.endswith("_*")
        if fam:
            families += 1
        if not fam:
            over_warn += 1
        is_err = fam or n > HARD
        if is_err:
            limit = exc.get(key)
            if limit is not None and n <= limit:
                warns += 1
                rows.append(
                    f"{n:6d}  {'warn':<6} {key} ({hint}; exception, ceiling {limit})"
                )
            else:
                errors += 1
                suffix = f"; limit {limit}" if limit is not None else ""
                rows.append(f"{n:6d}  {'ERROR':<6} {key} ({hint}{suffix})")
                if gh:
                    print(f"::error file={key}::{n} files: {hint}")
        else:
            warns += 1
            rows.append(
                f"{n:6d}  {'warn':<6} {key} ({hint}, soft limit {WARN})"
            )
    for row in rows:
        print(row)
    print(
        f"folder shape: families: {families}, folders over {WARN}: {over_warn}, "
        f"errors: {errors}, warnings: {warns}"
    )
    if warn_only or errors == 0:
        return 0
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
