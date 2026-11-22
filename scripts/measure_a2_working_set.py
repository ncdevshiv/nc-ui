"""Measures NC-UI's idle working set against the A2 target.

A2 (`docs/goal-prompt.md` §2) commits to an idle process-tree working set of ≤ 300 MiB,
to be proven by measurement rather than claimed. This runner reuses the exact functions
`comparison/bench/compare.py` uses for the comparison table — `wait_for_window`,
`process_cpu_percent` and `process_tree_memory`, all Win32 — so the number it reports is
methodologically identical to the table's "Idle working set (process tree)" row, not a
parallel measurement that could disagree with it.

It measures the counter example (`examples/counter`), the same app the comparison ports
mirror: a window with a page, which is precisely the shape A2 describes ("a window-with-a-
page does not pay for [subsystems it does not touch]"). The worker pool is now lazy, so this
run is the after-picture for the worker-pool half of A2.

Run from the repo root:
    python scripts/measure_a2_working_set.py [--repeats 5]
"""

from __future__ import annotations

import argparse
import importlib.util
import statistics
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
COMPARE = REPO / "comparison" / "bench" / "compare.py"
NCUI_EXE = REPO / "target" / "release" / "ncui.exe"
COUNTER = REPO / "examples" / "counter"
WINDOW_TITLE = "NC-UI Counter"
TARGET_MIB = 300.0


def load_compare_module():
    """Imports `compare.py` as a module so its Win32 measurement walk is reused, not copied.

    Copying the memory walk would risk the A2 number and the comparison-table number
    drifting apart; importing the one source of truth keeps them the same by construction.
    """
    spec = importlib.util.spec_from_file_location("ncui_compare", COMPARE)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load {COMPARE}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["ncui_compare"] = module
    spec.loader.exec_module(module)
    return module


def process_tree_decomposition(compare, root_pid: int) -> dict[str, int]:
    """Per-process working set under `root_pid`, summed by executable name.

    A2 requires the decomposition "per process as §3 of the audit does", because the tree
    total is dominated by WebView2's own browser/renderer/GPU processes, which the host
    code does not allocate. A total alone cannot tell the runtime's memory from the web
    engine's, so this names each contributor.
    """
    entries = compare.snapshot_processes()
    children: dict[int, list[int]] = {}
    for pid, (_, parent) in entries.items():
        children.setdefault(parent, []).append(pid)
    stack = [root_pid]
    seen: set[int] = set()
    by_name: dict[str, int] = {}
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        memory = compare.process_memory(pid)
        name = entries.get(pid, ("<unknown>", 0))[0]
        if memory:
            by_name[name] = by_name.get(name, 0) + memory
        stack.extend(children.get(pid, []))
    return dict(sorted(by_name.items(), key=lambda item: item[1], reverse=True))


def measure_idle_working_set(compare, repeats: int, app_dir: Path, window_title: str) -> dict:
    """Launches `app_dir` `repeats` times and walks the process tree at idle."""
    if not NCUI_EXE.is_file():
        raise RuntimeError(
            f"{NCUI_EXE} not found — build it first: cargo build --release -p ncui-cli"
        )

    memories: list[float] = []
    cold_starts: list[float] = []
    idle_cpu: list[float] = []
    last_decomposition: dict[str, int] = {}
    path_prefix = str(NCUI_EXE.parent)
    command = [str(NCUI_EXE), "run", str(app_dir)]

    for attempt in range(repeats):
        compare.kill_by_path_prefix(path_prefix)
        time.sleep(0.5)
        process = subprocess.Popen(
            command,
            cwd=str(REPO),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        started = time.monotonic()
        if not compare.wait_for_window(window_title, 30):
            compare.kill_tree(process.pid)
            raise RuntimeError(f"attempt {attempt}: window {window_title!r} did not appear")
        cold_starts.append((time.monotonic() - started) * 1000.0)
        # Settle to idle before sampling, exactly as `compare.measure` does, so the number
        # is the idle working set and not startup's peak.
        time.sleep(5.0)
        idle_cpu.append(compare.process_cpu_percent(process.pid, 2.0))
        memories.append(float(compare.process_tree_memory(process.pid)))
        last_decomposition = process_tree_decomposition(compare, process.pid)
        compare.kill_tree(process.pid)
        time.sleep(0.5)

    decomposition_mib = {
        name: memory / (1024 * 1024) for name, memory in last_decomposition.items()
    }
    return {
        "repeats": repeats,
        "working_set_mib": {
            "min": min(memories) / (1024 * 1024),
            "median": statistics.median(memories) / (1024 * 1024),
            "max": max(memories) / (1024 * 1024),
        },
        "cold_start_ms_median": statistics.median(cold_starts),
        "idle_cpu_percent_median": statistics.median(idle_cpu),
        "decomposition_mib_last_run": decomposition_mib,
        "target_mib": TARGET_MIB,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument(
        "--app",
        type=Path,
        default=COUNTER,
        help="app directory whose manifest names the window to wait for",
    )
    args = parser.parse_args()

    app_dir = args.app.resolve()
    manifest_path = app_dir / "ncui.app.json"
    import json

    window_title = json.loads(manifest_path.read_text(encoding="utf-8"))["window"]["title"]

    compare = load_compare_module()
    result = measure_idle_working_set(compare, args.repeats, app_dir, window_title)
    working_set = result["working_set_mib"]
    within_target = working_set["median"] <= result["target_mib"]

    print(
        f"NC-UI idle working set (process tree), {app_dir.name} example, "
        f"{result['repeats']} repeats:"
    )
    print(f"  min    {working_set['min']:.1f} MiB")
    print(f"  median {working_set['median']:.1f} MiB")
    print(f"  max    {working_set['max']:.1f} MiB")
    print(f"  target <= {result['target_mib']:.1f} MiB (A2)")
    print(f"  cold start to window (median) {result['cold_start_ms_median']:.1f} ms")
    print(f"  idle CPU (median) {result['idle_cpu_percent_median']:.2f} %")
    print("  per-process decomposition (last run, MiB):")
    for name, memory in result["decomposition_mib_last_run"].items():
        print(f"    {name:<28} {memory:7.1f}")
    print(
        "  A2 verdict: "
        + ("WITHIN target" if within_target else "OVER target")
        + f" (median {working_set['median']:.1f} MiB vs {result['target_mib']:.1f} MiB)"
    )
    return 0 if within_target else 1


if __name__ == "__main__":
    raise SystemExit(main())
