"""Records execution provenance into the feature registry after a gate test leg.

The registry's `verified` citations are only as good as the last time the tests behind
them actually ran. A citation can look alive for months while the test it names is
compiled out behind a feature flag, skipped for want of a display, or quietly rotten.
That is the lie this recorder exists to catch (roadmap F4): after a test leg passes, the
entries whose test evidence the leg *actually executed* are stamped with the instant and
the machine class. Entries the leg did not execute are left alone — and degrade to stale,
because a feature re-proven nowhere is a feature proven nowhere.

Usage:

    python tools/record_execution_provenance.py <registry.json> <cargo-test-log>...

The log is the captured stdout/stderr of a `cargo test` run. A test target counts as
executed when its binary appears in the log with at least one passed test. A binary that
was never built — a feature-gated file the leg did not enable — appears nowhere, which is
exactly the distinction between "tests exist" and "tests ran here".
"""

from __future__ import annotations

import json
import platform
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

# `Running unittests src\lib.rs (target\debug\deps\ncui_core-9f2c1a.exe)` and the same
# with forward slashes; the binary stem is what identifies the target.
RUNNING_RE = re.compile(r"Running .*\(([^()]+?)(?:\.exe)?\)")
RESULT_RE = re.compile(r"test result: ok\. (\d+) passed")
HASH_SUFFIX_RE = re.compile(r"-[0-9a-f]{16}$")

# A citation token that names a Rust source file, e.g. `crates/ncui-core/src/error.rs`.
RS_PATH_RE = re.compile(r"(?:crates|tools)/[A-Za-z0-9_-]+/(?:src|tests|benches)/[A-Za-z0-9_\-./]+\.rs")


def executed_targets(log_paths: list[Path]) -> dict[str, int]:
    """The test targets a cargo run executed, with their passed-test counts.

    The binary stem (`ncui_core`, `page_bridge`, `island_pixels`) is the identity: it is
    the crate name for unit tests and the file stem for integration tests and benches.
    """
    executed: dict[str, int] = {}
    for log_path in log_paths:
        try:
            text = log_path.read_text(encoding="utf-8", errors="replace")
        except OSError as error:
            raise SystemExit(f"record-provenance: cannot read {log_path}: {error}")
        # A leg that failed anywhere proves nothing anywhere: the gate chains stamping
        # behind passing legs, and this refusal is the same discipline inside the tool.
        if (
            "test result: FAILED" in text
            or "error: test failed" in text
            or "... FAILED" in text
        ):
            raise SystemExit(
                f"record-provenance: {log_path} contains a test failure; refusing to stamp"
            )
        current: str | None = None
        current_is_bench = False
        for line in text.splitlines():
            running = RUNNING_RE.search(line)
            if running:
                binary = Path(running.group(1)).name
                binary = HASH_SUFFIX_RE.sub("", binary)
                if binary.endswith(".exe"):
                    binary = binary[: -len(".exe")]
                current = binary
                # Harness-free benches print no `test result` line: the binary exiting
                # zero *is* the pass, and a failed bench fails the whole cargo leg, which
                # the failure refusal above has already excluded. The `Running` line names
                # the target kind (`Running benches\latency.rs (...)`) before the binary.
                current_is_bench = bool(re.search(r"Running benches[\\/]", line))
                if current_is_bench:
                    executed[current] = max(executed.get(current, 0), 1)
                continue
            result = RESULT_RE.search(line)
            if result and current is not None:
                executed[current] = executed.get(current, 0) + int(result.group(1))
    return executed


def file_carries_tests(path: Path) -> bool:
    """Whether a cited source file holds tests — the difference between a
    test-bearing citation and an existence claim.

    A crate whose tests all live under `tests/` has an empty lib test binary:
    it runs and passes zero, and demanding a passed count from it would make the
    feature unstamplable for a reason that has nothing to do with evidence. A
    file that cannot be read stays test-bearing: an unreadable citation must not
    quietly shed its requirement.
    """
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return True
    return bool(re.search(r"#\[[^\]]*test", text))


def required_targets(evidence_texts: list[str], repo_root: Path) -> set[str]:
    """The test targets a feature's citations require, derived from the cited paths.

    `crates/<crate>/tests/<file>.rs` and `crates/<crate>/benches/<file>.rs` build into a
    binary named after the file stem. `crates/<crate>/src/…` unit tests run in the crate's
    own lib test binary, named after the crate with dashes turned to underscores — and only
    when the cited file itself contains tests; a test-free source citation is an existence
    claim, which the registry validator already checks.
    """
    targets: set[str] = set()
    for text in evidence_texts:
        for match in RS_PATH_RE.finditer(text.replace("\\", "/")):
            parts = match.group(0).split("/")
            if len(parts) < 4:
                continue
            crate = parts[1]
            kind = parts[2]
            if kind in ("tests", "benches"):
                targets.add(Path(parts[-1]).stem)
            elif kind == "src" and file_carries_tests(repo_root / match.group(0)):
                targets.add(crate.replace("-", "_"))
    return targets


def machine_class() -> str:
    system = platform.system().lower()
    os_name = {"windows": "windows", "linux": "linux", "darwin": "macos"}.get(system, system)
    machine = platform.machine().lower()
    arch = {"amd64": "x86_64", "x86_64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
    return f"{os_name}-{arch}"


def insert_execution(feature: dict, provenance: dict) -> dict:
    """The feature dict with `execution` placed where the schema serializes it."""
    rebuilt: dict = {}
    for key, value in feature.items():
        if key == "execution":
            continue
        rebuilt[key] = value
        if key == "platforms":
            rebuilt["execution"] = provenance
    if "execution" not in rebuilt:
        rebuilt["execution"] = provenance
    return rebuilt


def main() -> int:
    args = sys.argv[1:]
    if len(args) < 2:
        print(__doc__)
        return 2
    registry_path = Path(args[0])
    log_paths = [Path(arg) for arg in args[1:]]

    data = json.loads(registry_path.read_text(encoding="utf-8"))
    executed = executed_targets(log_paths)
    if not executed:
        print("record-provenance: no executed test targets found in the log(s); nothing stamped")
        return 1

    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    provenance = {"last_executed": stamp, "machine_class": machine_class()}

    # Decide first, mutate second: a feature is stamped only when every test target its
    # citations require appears in the log with at least one passed test.
    stamped_ids: set[str] = set()
    skipped: list[tuple[str, str]] = []
    for feature in data["features"]:
        if feature.get("status") != "complete":
            continue
        texts = [
            item.get("evidence", "")
            for item in feature.get("evidence", {}).values()
            if isinstance(item, dict) and item.get("verified")
        ]
        targets = required_targets(texts, registry_path.parent.parent)
        if not targets:
            skipped.append((feature["id"], "no test-bearing citation"))
            continue
        missing = sorted(target for target in targets if executed.get(target, 0) == 0)
        if missing:
            skipped.append((feature["id"], f"not executed here: {', '.join(missing)}"))
            continue
        stamped_ids.add(feature["id"])

    data["features"] = [
        insert_execution(feature, provenance) if feature["id"] in stamped_ids else feature
        for feature in data["features"]
    ]

    registry_path.write_text(
        json.dumps(data, indent=2, ensure_ascii=True) + "\n", encoding="utf-8"
    )
    print(
        f"record-provenance: stamped {len(stamped_ids)} feature(s) at {stamp} on {machine_class()}"
    )
    for feature_id, reason in skipped:
        print(f"  not stamped: {feature_id} — {reason}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
