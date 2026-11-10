#!/usr/bin/env python3
"""Hygiene scan (Tier 1).

Banned patterns (minimum, rune-dev without weakening + Windows extension):

    /Users/  /private/var/  .ts.net  ses_[A-Za-z0-9]{20}  # ncui-allow-hygiene: this docstring names the patterns below; they are not paths
    C:\\Users\\  G:\\nc-  UNC \\\\ host  (Windows home / dev-machine roots)  # ncui-allow-hygiene: the same
    api-key / secret / token / password assignments, ghp_, github_pat_, sk-

Modes:

    --staged          scan `git diff --cached --no-color -U0` added lines only
    --tree [paths...] scan the tracked tree (or given paths)

Also checked, in both modes: every text file ends with exactly one trailing newline, as
`principle.md` requires. `--staged` reads git's own "No newline at end of file" marker.

Suppression requires inline ``ncui-allow-hygiene: <reason>`` with a non-empty
reason; an empty suppression is itself a finding. First-component exemption
(tests/fixtures/examples/docs + changelog/unreleased/README.md) is reported
as exempt, never silently skipped.

Usage: python3 scripts/hygiene-scan.py (--staged | --tree [paths...])
"""
from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

USAGE: str = "Usage: python3 scripts/hygiene-scan.py (--staged | --tree [paths...])"

PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("Users", re.compile(r"/Users/")),  # ncui-allow-hygiene: the pattern itself, not a path
    ("private-var", re.compile(r"/private/var/")),  # ncui-allow-hygiene: the pattern itself, not a path
    ("ts-net", re.compile(r"\.ts\.net")),
    ("session", re.compile(r"ses_[A-Za-z0-9]{20}")),
    # Windows extension: one-or-more backslashes so both raw `C:\Users` and
    # Rust-escaped `C:\\Users` in source match (plan §4.1, tuned for escaping).
    ("windows-users", re.compile(r"[A-Za-z]:\\+Users\\+")),
    ("dev-root", re.compile(r"[A-Za-z]:\\+nc-")),
    # UNC: 2+ backslashes at a token start, hostname, then separator. The
    # start anchor avoids flagging mid-path `src\\main` escapes as UNC; the
    # hostname+separator requirement avoids flagging `\u{...}` escapes.
    ("unc", re.compile(r"(^|[\s\"'(\[])\\{2,}[A-Za-z0-9._-]{2,}\\+" )),
    # Quoted value required: `token = CancellationToken::new()` must not fire.
    # Real secrets are quoted; unquoted 16+ identifiers are code, not secrets.
    ("secret-assign", re.compile(
        r"(?i)(api[_-]?key|secret|token|password)\s*[:=]\s*['\"][A-Za-z0-9/_+\-]{16,}"
    )),
    ("ghp", re.compile(r"ghp_[A-Za-z0-9]{36}")),
    ("github-pat", re.compile(r"github_pat_[A-Za-z0-9_]{22,}")),
    ("sk-key", re.compile(r"sk-[A-Za-z0-9]{20,}")),
)

EXEMPT_FIRST: frozenset[str] = frozenset({"tests", "fixtures", "examples", "docs"})
EXEMPT_PATHS: frozenset[str] = frozenset({"changelog/unreleased/README.md"})
SUPPRESS_RE: re.Pattern[str] = re.compile(r"ncui-allow-hygiene:\s*(.*)$")


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


def is_exempt(path: str) -> bool:
    norm = path.replace("\\", "/").lstrip("./")
    if norm in EXEMPT_PATHS:
        return True
    first = norm.split("/", 1)[0] if norm else ""
    return first in EXEMPT_FIRST


class Scanner:
    def __init__(self) -> None:
        self.errors: int = 0
        self.exempt: int = 0
        self.gh: bool = os.environ.get("GITHUB_ACTIONS") == "true"

    def check_line(self, path: str, lineno: int, line: str) -> None:
        marker = SUPPRESS_RE.search(line)
        if marker:
            reason = marker.group(1).strip().strip('"').strip("'")
            if reason:
                return
            # Empty suppression is itself a finding.
            self.errors += 1
            print(f"{path}:{lineno}:empty-suppression")
            if self.gh:
                print(
                    f"::error file={path},line={lineno}::"
                    "empty ncui-allow-hygiene suppression needs a reason"
                )
            return
        for name, pattern in PATTERNS:
            if pattern.search(line):
                if is_exempt(path):
                    self.exempt += 1
                    print(f"exempt: {path}:{lineno}:{name}")
                else:
                    self.errors += 1
                    print(f"{path}:{lineno}:{name}")
                    if self.gh:
                        print(
                            f"::error file={path},line={lineno}::"
                            f"hygiene pattern {name} matched"
                        )
                break

    def check_file(self, path: str) -> None:
        try:
            raw = Path(path).read_bytes()
        except OSError:
            return
        if not raw or b"\x00" in raw:
            return
        text = raw.decode("utf-8", errors="ignore")
        self.check_final_newline(path, text)
        for idx, line in enumerate(text.splitlines(), start=1):
            self.check_line(path, idx, line)

    def check_final_newline(self, path: str, text: str) -> None:
        """A text file ends with exactly one newline, as `principle.md` requires.

        The rule is stated in the project's principles and was, until this check existed,
        enforced by nobody: ten files in this tree had no final newline at all. Both the
        missing newline and a trailing blank line are findings.
        """
        if not text:
            return
        normalised = text.replace("\r\n", "\n")
        if not normalised.endswith("\n"):
            self.report_newline(path, "does not end with a newline")
        elif normalised.endswith("\n\n"):
            self.report_newline(path, "ends with a blank line rather than one newline")

    def report_newline(self, path: str, why: str) -> None:
        self.errors += 1
        print(f"{path}:final-newline ({why})")
        if self.gh:
            print(f"::error file={path}::{why}")

    def check_staged(self) -> None:
        try:
            diff = subprocess.check_output(
                ["git", "diff", "--cached", "--no-color", "-U0"],
                text=True, encoding="utf-8", stderr=subprocess.DEVNULL,
            )
        except (subprocess.CalledProcessError, FileNotFoundError, OSError) as exc:
            print(f"error: cannot read staged diff: {exc}", file=sys.stderr)
            self.errors += 1
            return
        current = ""
        new_line = 0
        # The file whose last added line was seen, so git's "No newline at end of file"
        # marker can be attributed to the new file rather than to the old one.
        last_added = ""
        for raw in diff.splitlines():
            if raw.startswith("+++ "):
                target = raw[4:].strip()
                if target == "/dev/null" or target == "---":
                    current = ""
                elif target.startswith("b/"):
                    current = target[2:]
                else:
                    current = target
                # The fuzz corpus is generated test-vector data, not text; a seed
                # without a trailing newline is a valid input, not a defect.
                if current.startswith("fuzz/corpus/"):
                    current = ""
                last_added = ""
                continue
            if raw.startswith("@@ "):
                m = re.search(r"\+(\d+)", raw)
                if m:
                    new_line = int(m.group(1))
                last_added = ""
                continue
            if raw.startswith("+") and not raw.startswith("+++"):
                body = raw[1:]
                if current:
                    self.check_line(current, new_line, body)
                    last_added = current
                new_line += 1
                continue
            if raw.startswith("-") and not raw.startswith("---"):
                last_added = ""
                continue
            if raw.startswith("\\"):
                # "\ No newline at end of file" — it follows the line it describes, so
                # it is a finding only when that line was added.
                if last_added:
                    self.report_newline(last_added, "does not end with a newline")
                    last_added = ""
                continue
            # Context / other diff lines end an hunk.
            if raw.startswith("diff ") or raw.startswith("index "):
                continue

    def check_tree(self, paths: list[str]) -> None:
        files: list[str] = []
        if paths:
            for token in paths:
                p = Path(token)
                if p.is_dir():
                    for sub in p.rglob("*"):
                        if sub.is_file():
                            try:
                                rel = sub.relative_to(".").as_posix()
                            except ValueError:
                                rel = sub.as_posix()
                            files.append(rel)
                elif p.is_file():
                    files.append(p.as_posix())
                else:
                    # Keep the token so the missing-file case is visible.
                    files.append(token)
        else:
            try:
                out = subprocess.check_output(
                    ["git", "ls-files", "-z"],
                    text=True, encoding="utf-8", stderr=subprocess.DEVNULL,
                )
                files = [
                    e for e in out.split("\x00") if e.strip()
                ]
            except (subprocess.CalledProcessError, FileNotFoundError, OSError):
                files = sorted(
                    p.as_posix() for p in Path(".").rglob("*") if p.is_file()
                )
        for f in files:
            if not Path(f).is_file():
                continue
            # Skip generated / build output by first-component convention.
            # fuzz/corpus/ holds generated fuzz seeds (binary test vectors,
            # cf. §4.4 Cargo.lock-digest rationale), not source; scanning them
            # flags random backslash bytes, not leaks.
            if (
                f.startswith("target/")
                or f.startswith("benches/results/")
                or f.startswith("fuzz/corpus/")
            ):
                continue
            self.check_file(f)


def main(argv: list[str]) -> int:
    if "--help" in argv or "-h" in argv:
        print(USAGE)
        return 0
    allowed = {"--staged", "--tree"}
    staged = False
    tree = False
    paths: list[str] = []
    tree_paths: list[str] = []
    saw_tree = False
    for arg in argv:
        if arg == "--staged":
            staged = True
            saw_tree = False
        elif arg == "--tree":
            tree = True
            saw_tree = True
        elif arg.startswith("--"):
            print(f"unknown option: {arg}", file=sys.stderr)
            print(USAGE, file=sys.stderr)
            return 2
        else:
            if saw_tree:
                tree_paths.append(arg)
            else:
                paths.append(arg)
    if not staged and not tree:
        print("error: specify --staged or --tree", file=sys.stderr)
        print(USAGE, file=sys.stderr)
        return 2
    if paths and not tree:
        print(f"unknown option: {paths[0]}", file=sys.stderr)
        return 2

    root = repo_root()
    try:
        os.chdir(root)
    except OSError:
        pass

    scanner = Scanner()
    # --tree wins when both are given (shims prepend --staged for parity).
    if tree:
        scanner.check_tree(tree_paths)
    elif staged:
        scanner.check_staged()

    print(
        f"hygiene: errors: {scanner.errors}, exempt: {scanner.exempt}"
    )
    return 0 if scanner.errors == 0 else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
