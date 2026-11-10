#!/usr/bin/env python3
"""File-size guard (Tier 1).

Counts non-test lines per .rs file under crates/, tools/ncui-tools/src/,
examples/ (and packages/ if present). Inline #[cfg(test)] items are excluded;
test-only files are measured separately.

    warn  > 800 non-test lines
    error > 1500 non-test lines unless listed in scripts/file-size-exceptions.txt
    warn  > 1000 lines in a test file or inline test module

Also runs folder-shape.py; its errors and warnings are added to the summary
line, and --update-exceptions refreshes both exception lists.

Usage: python3 scripts/file-size.py [--warn-only] [--update-exceptions [--allow-raise]]
"""
from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

SOFT: int = 800
HARD: int = 1500
TESTCAP: int = 1000
EXC: str = "scripts/file-size-exceptions.txt"
SCOPES: tuple[str, ...] = ("crates", "examples", "tools/ncui-tools/src", "packages")
# rune-dev awk regex is (\/|^)tests?\.rs$ | _tests\.rs$ | \/tests\/; Tier 1
# extends it with _test\.rs to cover plan §1.1 `*_test.rs`.
TESTFILE_RE: re.Pattern[str] = re.compile(
    r"(^|/)tests?\.rs$|_tests?\.rs$|_test\.rs$|/tests/"
)
CFG_TEST_RE: re.Pattern[str] = re.compile(r"^[ \t]*#\[cfg\(test\)\]")
ATTR_RE: re.Pattern[str] = re.compile(r"^[ \t]*#\[")
USAGE: str = (
    "Usage: python3 scripts/file-size.py [--warn-only] "
    "[--update-exceptions [--allow-raise]]"
)


def repo_root() -> Path:
    """Return the git top-level directory, falling back to cwd."""
    try:
        out = subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True, encoding="utf-8", stderr=subprocess.DEVNULL
        ).strip()
        if out:
            return Path(out)
    except (subprocess.CalledProcessError, FileNotFoundError, OSError):
        pass
    return Path.cwd()


def list_rs_files() -> list[str]:
    """Tracked + untracked .rs files in scope, repo-relative, sorted."""
    names: set[str] = set()
    try:
        proc = subprocess.run(
            ["git", "ls-files", "-z", "--cached", "--others",
             "--exclude-standard", "--", *SCOPES],
            capture_output=True, text=True, encoding="utf-8", check=True,
        )
        for entry in proc.stdout.split("\x00"):
            entry = entry.strip()
            if entry and entry.endswith(".rs"):
                names.add(entry.replace("\\", "/"))
    except (subprocess.CalledProcessError, FileNotFoundError, OSError):
        names = set()
    if names:
        return sorted(names)
    # Fallback (tarball / non-git): rglob.
    found: list[str] = []
    for scope in SCOPES:
        base = Path(scope)
        if base.is_dir():
            for path in base.rglob("*.rs"):
                found.append(path.as_posix())
    return sorted(set(found))


def braces(line: str) -> int:
    """Net brace depth delta (naive port of the rune-dev awk braces())."""
    s = re.sub(r"\\.", "", line)
    s = re.sub(r'"[^"]*"', "", s)
    s = re.sub(r"'.'", "", s)
    s = re.sub(r"//.*", "", s)
    return s.count("{") - s.count("}")


def measure_file(path: str) -> tuple[int, int, int]:
    """Return (prod, test, maxtest) for one file (rune-dev awk faithful)."""
    try:
        text = Path(path).read_text(encoding="utf-8", errors="ignore")
    except OSError:
        return (0, 0, 0)
    lines = text.splitlines()
    testfile: bool = bool(TESTFILE_RE.search(path.replace("\\", "/")))
    prod: int = 0
    test: int = 0
    st: int = 0  # 0 prod, 1 attr seen, 2 inside test item
    depth: int = 0
    maxtest: int = 0
    cur: int = 0
    for line in lines:
        if testfile:
            test += 1
            cur += 1
            continue
        if st == 0:
            if CFG_TEST_RE.match(line):
                st = 1
                cur = 1
                test += 1
                continue
            prod += 1
            continue
        test += 1
        cur += 1
        if st == 1:
            if ATTR_RE.match(line):
                continue
            delta = braces(line)
            if delta > 0 or "{" in line:
                depth = delta
                st = 2 if depth > 0 else 0
            elif re.search(r";[ \t]*$", line):
                st = 0
            if st == 0:
                if cur > maxtest:
                    maxtest = cur
                cur = 0
            continue
        depth += braces(line)
        if depth <= 0:
            st = 0
            if cur > maxtest:
                maxtest = cur
            cur = 0
    if cur > maxtest:
        maxtest = cur
    return (prod, test, maxtest)


def load_ceilings() -> dict[str, int]:
    """Map path -> ceiling from the exceptions file."""
    out: dict[str, int] = {}
    exc = Path(EXC)
    if not exc.is_file():
        return out
    for raw in exc.read_text(encoding="utf-8", errors="ignore").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if len(parts) >= 2:
            try:
                out[parts[0]] = int(parts[1])
            except ValueError:
                continue
    return out


def run_folder_shape(
    warn_only: bool, update: bool, raise_ok: bool
) -> tuple[int, int]:
    """Delegate to folder-shape.py; return (shape_errors, shape_warns)."""
    if not Path("scripts/folder-shape.py").is_file():
        return (0, 0)
    args: list[str] = [sys.executable, "scripts/folder-shape.py"]
    if update:
        args.append("--update-exceptions")
    if raise_ok:
        args.append("--allow-raise")
    if not update:
        args.append("--warn-only")
    proc = subprocess.run(args, capture_output=True, text=True, encoding="utf-8")
    out_text = proc.stdout.strip()
    if out_text:
        print(out_text)
    if proc.stderr.strip() and update:
        # Update-mode notices go to stderr in folder-shape; keep them visible.
        print(proc.stderr.strip(), file=sys.stderr)
    errors = 0
    warns = 0
    if not update:
        last = ""
        for ln in out_text.splitlines():
            if ln.strip():
                last = ln.strip()
        m_err = re.search(r"errors:\s*(\d+)", last)
        m_warn = re.search(r"warnings:\s*(\d+)", last)
        if m_err:
            errors = int(m_err.group(1))
        if m_warn:
            warns = int(m_warn.group(1))
    return (errors, warns)


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

    files = list_rs_files()
    ceilings = load_ceilings()
    gh = os.environ.get("GITHUB_ACTIONS") == "true"

    errors = 0
    warns = 0
    oversoft = 0
    overhard = 0
    out_lines: list[str] = []
    rows: list[tuple[str, int, int, int]] = []

    for path in files:
        prod, test, maxtest = measure_file(path)
        rows.append((path, prod, test, maxtest))
        is_test_only: bool = (
            prod == 0 and test > 0 and maxtest == test and test > TESTCAP
        )
        if is_test_only:
            warns += 1
            out_lines.append(f"{test:6d}  {'tests':<6} {path}")
            if gh:
                print(f"::warning file={path}::test file has {test} lines (limit {TESTCAP})")
            continue
        if maxtest > TESTCAP:
            warns += 1
            out_lines.append(f"{maxtest:6d}  {'tests':<6} {path}")
            if gh:
                print(f"::warning file={path}::inline test module has {maxtest} lines (limit {TESTCAP})")
        if prod <= SOFT:
            continue
        oversoft += 1
        if prod > HARD:
            overhard += 1
            limit = ceilings.get(path, HARD)
            if prod > limit:
                errors += 1
                out_lines.append(f"{prod:6d}  {'ERROR':<6} {path} (limit {limit})")
                if gh:
                    print(
                        f"::error file={path}::{prod} non-test lines, limit {limit}; "
                        "split a module out before adding more"
                    )
            else:
                warns += 1
                out_lines.append(
                    f"{prod:6d}  {'warn':<6} {path} (exception, ceiling {limit})"
                )
                if gh:
                    print(
                        f"::warning file={path}::{prod} non-test lines "
                        f"(exception ceiling {limit}, soft limit {SOFT})"
                    )
        else:
            warns += 1
            out_lines.append(f"{prod:6d}  {'warn':<6} {path}")
            if gh:
                print(f"::warning file={path}::{prod} non-test lines (soft limit {SOFT})")

    # Sorted by descending count, then path (rune-dev `sort -rn`).
    def sort_key(line: str) -> tuple[int, str]:
        parts = line.split(None, 1)
        try:
            n = int(parts[0])
        except (ValueError, IndexError):
            n = 0
        rest = parts[1] if len(parts) > 1 else ""
        return (-n, rest)

    for line in sorted(out_lines, key=sort_key):
        print(line)

    shape_errors, shape_warns = run_folder_shape(warn_only, update, raise_ok)
    if not update:
        errors += shape_errors
        warns += shape_warns

    print(
        f"over {SOFT}: {oversoft}, over {HARD}: {overhard}, "
        f"errors: {errors}, warnings: {warns}"
    )

    if update:
        new: dict[str, int] = {}
        for path, prod, _test, _maxtest in rows:
            if prod <= HARD:
                continue
            old = ceilings.get(path)
            if old is None:
                if raise_ok:
                    new[path] = prod
                else:
                    print(
                        f"not listing new oversized file {path} ({prod}); "
                        "use --allow-raise",
                        file=sys.stderr,
                    )
            elif prod > old and not raise_ok:
                new[path] = old
            else:
                new[path] = prod
        content_lines: list[str] = [
            f"# <path> <ceiling>: non-test line ceilings for files above {HARD}. "
            "Ceilings only go down.",
            "# Refresh with scripts/file-size.sh --update-exceptions.",
        ]
        for key in sorted(new):
            content_lines.append(f"{key} {new[key]}")
        Path(EXC).write_text("\n".join(content_lines) + "\n", encoding="utf-8")
        print(f"wrote {EXC} ({len(new)} entries)")
        return 0

    if warn_only:
        return 0
    return 0 if errors == 0 else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
