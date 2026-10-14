#!/usr/bin/env python3
"""Re-derive the feature registry's per-file test counts from the files themselves.

Citations quoted a crate-wide total against a single file — `frame.rs — 95 unit tests` when the
crate has 95 and the file has 20 — which `ncui-registry` cannot catch: it proves a cited file
exists, not that the file contains what is claimed. That is the same defect as a figure typed by
hand into prose that nothing re-derives, and it recurred three times before this script existed.

    python tools/correct_test_counts.py           # correct the registry
    python tools/correct_test_counts.py --check   # report only, exit 1 on any mismatch

`--check` is what CI, `make policy` and the Linux gate run. It rewrites nothing, so a mismatch
fails the build instead of being quietly fixed.

**What it covers.** The one canonical form: a Rust path, a dash, and the figure —
`crates/ncui-ipc/src/frame.rs — 20 unit tests`. Every citation in that form is checked; the
success message prints how many it matched, so a run that matched nothing cannot be mistaken for
a run that found nothing wrong.

**What it deliberately does not.** Three other shapes occur and are left alone, because guessing
at them is worse than leaving them unchecked:

* **non-Rust files** — a `.py` file's "tests" are `def test_…` functions, not `#[test]`, and
  `tests_in` only recognises the Rust form. `test_build_release.py — 8 tests` is therefore
  outside this gate's reach; it was wrong at 14 and is corrected by hand, not by this tool;

* multi-file aggregates — `…/lifecycle.rs, …/navigation.rs and …/seam.rs hold 37 unit tests`,
  where the figure is a *sum*. Matching only the last path would compare 11 against 37 and report
  a correct citation as wrong;
* a file named in prose with no figure at all — `focus.rs — route can only move focus …` —
  where a nearby number belongs to a different sentence.

An earlier version tried to cover all three by splitting prose into clauses. It over-matched so
badly that one clause swallowed eighteen paths and summed them, and under-matched so badly that
splitting on `.` cut `negotiation.rs` in half — after which it reported "every count matches"
while three wrong figures sat in the file. A gate that guesses is worse than a gate that checks
one form precisely, and the three it misses are the three that need a human.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
REGISTRY = REPO / "protocols" / "feature-registry.json"

# Only the registry is scanned, and deliberately so.
#
# An earlier version also scanned `README.md` and `docs/known-gaps.md` and reported coverage
# it did not have: prose cites a file in backticks rather than after a dash, so the scan
# matched **zero** citations in either file while the success message still said "every count
# matches the registry and 2 prose files". Injecting a wrong figure into the README exited 0.
# A gate that reports all-clear over zero inputs is indistinguishable from a working gate by
# its exit code, which is the worst failure mode there is.
#
# So prose no longer states counts at all — `README.md` names the file and points here. The
# registry is *parsed*, not read as text, because its em-dashes are JSON-escaped and a text
# scan sees `\u2014`, matching nothing for the same reason.

# The canonical citation: a path, a dash, and the figure.
#
#   "crates/ncui-ipc/src/frame.rs — 20 unit tests covering magic, version, …"
#   "crates/ncui-runtime/tests/agent_transport.rs — 20 tests driving a live server"
#
# Two shapes are deliberately NOT matched, because a wrong guess about them is worse than
# leaving them unchecked:
#
#   multi-file aggregates  "…/lifecycle.rs, …/navigation.rs and …/seam.rs hold 37 unit
#                          tests" — the figure is a sum, and matching only the last path
#                          would compare 11 against 37 and report a correct citation as wrong.
#   a file named in prose  "crates/ncui-a11y/src/focus.rs — route can only move focus…" —
#                          no figure at all, and a nearby number belongs to another clause.
#
# An earlier version tried to handle all three by splitting prose into clauses and guessing
# which paths belonged to which figure. It over-matched so badly that one clause swallowed
# eighteen paths and summed them, and it under-matched so badly that splitting on `.` cut
# `negotiation.rs` in half — reporting "every count matches" while a real error sat in front of
# it. A gate that guesses is worse than a gate that checks one form precisely.
CITATION = re.compile(
    r"(?P<path>(?:crates|tools)/[A-Za-z0-9_./-]+\.rs)"
    # `\s*` before the dash: the citations read `frame.rs — 20`, with a space between the
    # path and the punctuation. Em dash, en dash, colon or hyphen.
    r"\s*[—–:-]\s*"
    r"(?P<count>\d+)\s+(?:unit\s+)?tests\b"
)

def claims(text: str) -> list[tuple[list[str], int, int]]:
    """Every `(paths, claimed, count_start)` in a body of prose.

    `count_start` indexes into `text`, so a wrong figure can be rewritten in place.
    """
    return [
        ([m.group("path")], int(m.group("count")), m.start("count"))
        for m in CITATION.finditer(text)
    ]


def expected(paths: list[str]) -> int:
    """The figure a citation should carry: one file's count, or the sum of several."""
    return sum(tests_in(REPO / p) for p in paths)


def tests_in(path: Path) -> int:
    """The number of `#[test]` functions in a Rust source file.

    Counted textually rather than by running the suite: the figure a citation makes is a
    claim about the file, and a figure derived from `cargo test` would be the crate's total,
    which is the very mistake being corrected.
    """
    return len(re.findall(r"^\s*#\[test\]", path.read_text(encoding="utf-8"), re.M))


def walk(node, path: str = ""):
    """Yield every `(json_path, string_value)` pair in the document."""
    if isinstance(node, dict):
        for key, value in node.items():
            yield from walk(value, f"{path}.{key}" if path else key)
    elif isinstance(node, list):
        for index, value in enumerate(node):
            yield from walk(value, f"{path}[{index}]")
    elif isinstance(node, str):
        yield path, node


def find_citations() -> list[tuple[str, list[str], int, int]]:
    """Every canonical count citation in the registry.

    Returns `(label, paths, claimed, span_start_of_the_numeral)`. Discovery rather than an
    allowlist: a fixed list of files means a citation of a sixth file is silently unchecked,
    which re-creates the very defect this script exists to close.
    """
    registry = json.loads(REGISTRY.read_text(encoding="utf-8"))
    # Serialised with unicode intact so the em-dashes survive; the registry is read this way
    # rather than as text precisely because they do not.
    flat = json.dumps(registry, ensure_ascii=False)
    return [("registry", paths, claimed, at) for paths, claimed, at in claims(flat)]


def check() -> list[str]:
    """Report every citation whose number disagrees with the files it names."""
    problems: list[str] = []
    for document, paths, claimed, _ in find_citations():
        want = expected(paths)
        if claimed != want:
            named = paths[0]
            problems.append(
                f"{document}: {named} claimed {claimed}, file holds {want}"
            )
    return problems


def rewrite_registry() -> list[str]:
    """Correct the registry in place. Prose files are reported, never rewritten."""
    registry = json.loads(REGISTRY.read_text(encoding="utf-8"))
    changes: list[str] = []
    for json_path, text in list(walk(registry)):
        spans: list[tuple[int, int, int]] = []
        for paths, claimed, at in claims(text):
            want = expected(paths)
            if claimed == want:
                continue
            spans.append((at, claimed, want))
            changes.append(f"{paths[0]}: claimed {claimed}, holds {want}")
        # Applied right-to-left, and really so: replacing "95" with "20" shortens the text by
        # one character, which would shift every span found after it. Iterating forwards, as
        # an earlier version did while claiming otherwise, corrupted the second citation in
        # any string that held two — `— 999 unit tests and … — 88 unit tests` came out as
        # `— 85unit tests`, and the mangled string then stopped matching CITATION, so the
        # citation dropped out of coverage silently on the next run.
        spans = sorted(spans, key=lambda s: s[0], reverse=True)
        for at, claimed, want in spans:
            end = at + len(str(claimed))
            text = text[:at] + str(want) + text[end:]
        _assign(registry, json_path, text)
    # Written the way the sibling registry writers write it: plain `json.dump(indent=2)`.
    # Matching their encoding matters — `ensure_ascii=False` here rewrote every em-dash as an
    # escape, so whichever tool ran last decided the file's format and the others produced a
    # whole-document diff.
    #
    # Guarded: writing unconditionally rewrites the file even when nothing changed, and on
    # Windows that flips every line LF→CRLF. A no-op run should leave the bytes alone.
    if changes:
        REGISTRY.write_text(json.dumps(registry, indent=2) + "\n", encoding="utf-8")
    return changes


def _assign(registry: dict, json_path: str, value: str) -> None:
    """Set the string at `json_path` in a parsed document."""
    node = registry
    keys = [(a or b) for a, b in re.findall(r"([^.\[\]]+)|\[(\d+)\]", json_path)]
    for key in keys[:-1]:
        node = node[int(key)] if isinstance(node, list) else node[key]
    last = keys[-1]
    if isinstance(node, list):
        node[int(last)] = value
    else:
        node[last] = value



def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="report mismatches and exit 1 without writing",
    )
    args = parser.parse_args()
    problems = check()

    if args.check:
        if not problems:
            print(
                f"all {len(find_citations())} registry test count(s) match the file "
                "they cite"
            )
            return 0
        for problem in problems:
            print(f"WRONG: {problem}")
        print(
            "\nThese are the class of claim the validator cannot check: it proves a cited file\n"
            "exists, not that the file contains what is said about it.\n"
            "Run `python tools/correct_test_counts.py` to re-derive the registry's.",
            file=sys.stderr,
        )
        return 1

    changes = rewrite_registry()
    for change in changes:
        print(f"corrected: {change}")
    if not changes:
        print(f"every test count matches the file it cites "
              f"({len(find_citations())} citation(s) checked)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
