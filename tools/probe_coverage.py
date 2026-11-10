#!/usr/bin/env python3
"""Regenerate the mutation-coverage figures the `ncui-gpu` contract comment quotes.

The comment above `the_contract_probe_reports_exactly_the_items_the_canonical_host_demonstrates`
states how many probes a mutation kills, in three directions. Those numbers were measured by
hand and they drift: add a probe arm, or change what `first_row_point` gates on, and the
sentence is wrong again with nothing to notice. This script re-derives them.

It mutates a byte-copy of the crate in a scratch directory and never touches the repository,
so it is safe to run at any time.

    python tools/probe_coverage.py            # print the table
    python tools/probe_coverage.py --json     # machine-readable

Exit code is 0 for every measurement this reports — it does not gate the build. It is
deliberately not part of the `gate` target, for the same reason the benchmark regression gate
reports without failing: see "The benchmark regression gate reports; it does not fail" in
`docs/known-gaps.md`. Wiring it into CI as a non-blocking step is a separate decision, not one
taken here.

The one exception is exit code 2, which means `ITEMS` has drifted from `INPUT_CONTRACT`. That
is a refusal to *report*, not a quality gate: a figure measured against the wrong set of probes
is worse than no figure, because it looks like the real one.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CRATE = REPO / "crates" / "ncui-gpu"

# The contract items that have a probe to mutate, in the order `INPUT_CONTRACT` lists them.
#
# Ten of the eleven: `Accessibility` is deliberately absent. It is listed in `UNIMPLEMENTED`
# (see `input.rs`), so `probes` returns before reaching a dispatch arm and there is no probe body
# to force. Adding it here would report `PROBE NOT FOUND` in the body directions and a silent
# survivor in the dispatch direction — which is why `check_items_current` rejects the drift up
# front rather than letting it measure the wrong set.
ITEMS = [
    "Mouse",
    "Keyboard",
    "Focus",
    "Scrolling",
    "Clipboard",
    "Selection",
    "Resize",
    "HighDpi",
    "DragAndDrop",
    "Ime",
]


def patch_arm(text: str, item: str, body: str) -> str:
    """Replace `InputContract::<item> => ...` (through its trailing comma) with `body`."""
    lines = text.split("\n")
    start = next(
        (i for i, l in enumerate(lines) if l.strip().startswith(f"InputContract::{item} =>")),
        None,
    )
    if start is None:
        raise SystemExit(f"no dispatch arm for {item} — the shape changed, update this script")
    end = start
    while not lines[end].rstrip().endswith(","):
        end += 1
    indent = " " * (len(lines[start]) - len(lines[start].lstrip()))
    lines[start : end + 1] = [f"{indent}{body},"]
    return "\n".join(lines)


def run(cmd: list[str], cwd: Path) -> int:
    return subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, encoding="utf-8").returncode


def first_failing_test(output: str) -> str | None:
    """One failing test name, or `None`.

    The *first* one the runner emitted, which is not a stable or causal attribution: a single
    mutation commonly fails several tests, and the parallel runner's interleaving decides which
    surfaces here. Treat this as "a test that noticed", never as "the test that guards this".
    The counts, by contrast, are deterministic.
    """
    match = re.search(r"^test (\S+) \.\.\. FAILED", output, re.M)
    return match.group(1) if match else None


def battery(direction: str) -> dict[str, dict[str, object]]:
    """Force each probe in one direction and report which test notices."""
    results: dict[str, dict[str, object]] = {}
    with tempfile.TemporaryDirectory(prefix="ncui-probe-coverage-") as tmp:
        scratch = Path(tmp) / "repo"
        # Only `target/` and `.git/` are excluded. Narrowing further (e.g. `benches/`) breaks
        # `ncui-bench`'s manifest and cargo then fails before a single test runs — which looks
        # exactly like "every mutation was killed" if the return code is not also inspected.
        shutil.copytree(
            REPO,
            scratch,
            ignore=shutil.ignore_patterns("target", ".git"),
        )
        input_rs = scratch / "crates" / "ncui-gpu" / "src" / "input.rs"
        original = input_rs.read_text(encoding="utf-8")

        for item in ITEMS:
            if direction == "dispatch-true":
                # The arm is replaced wholesale; `host` must stay referenced or it will not
                # compile, which would be a compile error rather than a measurement.
                mutated = patch_arm(original, item, f"InputContract::{item} => {{ let _ = &host; true }}")
                path = input_rs
            else:
                # Force the probe body's own return value.
                marker = f"fn probe_{snake(item)}("
                if marker not in original:
                    results[item] = {"status": "probe not found", "killed_by": None}
                    continue
                idx = original.index(marker)
                params = original[original.index("(", idx) + 1 : original.index(")", idx)]
                names = [p.split(":")[0].strip() for p in params.split(",") if p.strip()]
                # Bind every parameter away first. An early `return` leaves them unused, which
                # is a *warning* under plain `cargo test` and would otherwise read as a kill on
                # a build that runs `-D warnings`.
                bind = ", ".join(names)
                prelude = f"\n    let _ = ({bind});" if bind else ""
                brace = original.index(" {", idx) + 2
                mutated = (
                    original[:brace]
                    + prelude
                    + f"\n    return {'true' if direction == 'body-true' else 'false'};"
                    + original[brace:]
                )
                path = input_rs
            path.write_text(mutated, encoding="utf-8", newline="")
            proc = subprocess.run(
                ["cargo", "test", "-p", "ncui-gpu"],
                cwd=scratch,
                capture_output=True,
                text=True, encoding="utf-8",
            )
            # A compile error is NOT a kill. Reporting it as one would make an unbuildable
            # mutation look like a covered one — which is exactly how the first version of this
            # script reported 10/10 for every direction.
            combined = proc.stdout + proc.stderr
            # A compile error is NOT a kill. But a *failing test* also prints a line starting
            # with "error:" ("error: test failed, to rerun pass ...") — so the failing-test check
            # must come first, or every real kill reads as a compile error.
            failing = first_failing_test(proc.stdout)
            if failing is not None:
                results[item] = {"status": "killed", "killed_by": failing}
            elif "error[" in combined or "error: could not compile" in combined:
                detail = next(
                    (
                        l
                        for l in combined.splitlines()
                        if l.startswith("error") and "to rerun pass" not in l
                    ),
                    "unknown build failure",
                )
                results[item] = {"status": "COMPILE ERROR", "killed_by": None, "detail": detail}
            elif proc.returncode != 0:
                results[item] = {"status": "FAILED, no test named", "killed_by": None}
            else:
                results[item] = {"status": "survives", "killed_by": None}
            path.write_text(original, encoding="utf-8", newline="")
    return results


def snake(item: str) -> str:
    out = []
    for i, ch in enumerate(item):
        if ch.isupper() and i > 0 and not item[i - 1].isupper():
            out.append("_")
        out.append(ch.lower())
    return "".join(out)


def check_items_current() -> list[str]:
    """Every `INPUT_CONTRACT` variant that has a probe function must be listed in `ITEMS`.

    Without this the script would quietly measure a stale subset: rename a probe, and every
    battery reports a figure that still looks plausible. Returns the problems found.
    """
    source = (CRATE / "src" / "input.rs").read_text(encoding="utf-8")
    problems: list[str] = []
    contract = re.search(r"pub const INPUT_CONTRACT: &\[InputContract\] = &\[(.*?)\];", source, re.S)
    if not contract:
        return ["could not locate INPUT_CONTRACT in input.rs"]
    declared = re.findall(r"InputContract::(\w+)", contract.group(1))
    unimplemented = re.search(
        r"const UNIMPLEMENTED: &\[InputContract\] = &\[(.*?)\];", source, re.S
    )
    deferred = set(re.findall(r"InputContract::(\w+)", unimplemented.group(1))) if unimplemented else set()
    expected = [d for d in declared if d not in deferred]
    if expected != ITEMS:
        problems.append(
            f"ITEMS has drifted from INPUT_CONTRACT: script has {ITEMS}, "
            f"code implies {expected} (excluding UNIMPLEMENTED={sorted(deferred)})"
        )
    for item in ITEMS:
        if f"fn probe_{snake(item)}(" not in source:
            problems.append(f"no probe function named probe_{snake(item)} for {item}")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit JSON instead of a table")
    args = parser.parse_args()

    drift = check_items_current()
    if drift:
        for problem in drift:
            print(f"STALE: {problem}", file=sys.stderr)
        print("Fix ITEMS in this script before trusting any figure below.", file=sys.stderr)
        return 2

    report = {
        "body-false": battery("body-false"),
        "body-true": battery("body-true"),
        "dispatch-true": battery("dispatch-true"),
    }

    if args.json:
        print(json.dumps(report, indent=2))
        return 0

    for direction, items in report.items():
        killed = [k for k, v in items.items() if v["status"] == "killed"]
        survivors = [k for k, v in items.items() if v["status"] != "killed"]
        print(f"\n{direction}: {len(killed)}/{len(ITEMS)} killed")
        for item in ITEMS:
            entry = items[item]
            mark = "killed" if entry["status"] == "killed" else entry["status"].upper()
            detail = f"  <- {entry['killed_by']}" if entry["killed_by"] else ""
            print(f"  {mark:>13}  {item}{detail}")
        if survivors:
            print(f"  survivors: {', '.join(survivors)}")
    print("\nUpdate the comment above the golden-vector test if any figure moved.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
