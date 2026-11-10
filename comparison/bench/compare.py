#!/usr/bin/env python3
"""Cross-framework comparison harness: Electron and Tauri originals against their NC-UI ports.

The harness builds each application, applies the benchmark instrumentation, launches it,
measures the cold start to a visible window, the idle working set of the whole process tree,
the distributed size, and the IPC round-trip latency, and writes a machine-readable report.

    python comparison/bench/compare.py
    python comparison/bench/compare.py --repeats 5 --rounds 200
    python comparison/bench/compare.py --only electron-quick-start

The originals are cloned from the URLs and commits in `comparison/manifest.json` into
`--originals-dir` (default `comparison/.originals`). The benchmark instrumentation lives in
`comparison/bench/instrumentation/` and is copied over the cloned originals before a run, so
the committed originals stay exactly as cloned and the changes are reviewable.

The measurement is Windows-only: the window poll and the process-tree memory walk use the
Win32 user32/kernel32/psapi entry points through `ctypes`.
"""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import shutil
import statistics
import subprocess
import sys
import time
from ctypes import wintypes
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent.parent
COMPARISON = REPO / "comparison"
MANIFEST = COMPARISON / "manifest.json"
INSTRUMENTATION = COMPARISON / "bench" / "instrumentation"
RESULTS = COMPARISON / "bench" / "results"
DEFAULT_ORIGINALS = COMPARISON / ".originals"

# Instrumentation files, mapped from the instrumentation directory to the cloned app.
INSTRUMENTATION_MAP = {
    "electron": {
        "main.js": "main.js",
        "preload.js": "preload.js",
        "renderer.js": "renderer.js",
    },
    "tauri": {
        "main.rs": "src-tauri/src/main.rs",
        "App.jsx": "src/App.jsx",
        "tauri.conf.json": "src-tauri/tauri.conf.json",
    },
}

# ----------------------------------------------------------------------------------------------
# Win32 helpers
# ----------------------------------------------------------------------------------------------

user32 = ctypes.windll.user32
kernel32 = ctypes.windll.kernel32
psapi = ctypes.windll.psapi

ENUM_WINDOWS_PROC = ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)


class PROCESS_MEMORY_COUNTERS(ctypes.Structure):
    _fields_ = [
        ("cb", wintypes.DWORD),
        ("PageFaultCount", wintypes.DWORD),
        ("PeakWorkingSetSize", ctypes.c_size_t),
        ("WorkingSetSize", ctypes.c_size_t),
        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
        ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
        ("PagefileUsage", ctypes.c_size_t),
        ("PeakPagefileUsage", ctypes.c_size_t),
    ]


class PROCESSENTRY32(ctypes.Structure):
    _fields_ = [
        ("dwSize", wintypes.DWORD),
        ("cntUsage", wintypes.DWORD),
        ("th32ProcessID", wintypes.DWORD),
        ("th32DefaultHeapID", ctypes.POINTER(ctypes.c_ulong)),
        ("th32ModuleID", wintypes.DWORD),
        ("cntThreads", wintypes.DWORD),
        ("th32ParentProcessID", wintypes.DWORD),
        ("pcPriClassBase", ctypes.c_long),
        ("dwFlags", wintypes.DWORD),
        ("szExeFile", ctypes.c_char * 260),
    ]


def visible_window_titles() -> list[str]:
    titles: list[str] = []

    def callback(hwnd: int, _lparam: int) -> bool:
        if not user32.IsWindowVisible(hwnd):
            return True
        length = user32.GetWindowTextLengthW(hwnd)
        if length == 0:
            return True
        buffer = ctypes.create_unicode_buffer(length + 1)
        user32.GetWindowTextW(hwnd, buffer, length + 1)
        titles.append(buffer.value)
        return True

    user32.EnumWindows(ENUM_WINDOWS_PROC(callback), 0)
    return titles


def wait_for_window(title: str, timeout: float = 30.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if any(title in window for window in visible_window_titles()):
            return True
        time.sleep(0.05)
    return False


def process_path(pid: int) -> str | None:
    handle = kernel32.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
    if not handle:
        return None
    try:
        buffer = ctypes.create_unicode_buffer(260)
        size = wintypes.DWORD(260)
        if kernel32.QueryFullProcessImageNameW(handle, 0, buffer, ctypes.byref(size)):
            return buffer.value
        return None
    finally:
        kernel32.CloseHandle(handle)


def process_memory(pid: int) -> int | None:
    handle = kernel32.OpenProcess(0x1000 | 0x0010, False, pid)
    if not handle:
        return None
    try:
        counters = PROCESS_MEMORY_COUNTERS()
        counters.cb = ctypes.sizeof(counters)
        if psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
            return counters.WorkingSetSize
        return None
    finally:
        kernel32.CloseHandle(handle)


def snapshot_processes() -> dict[int, tuple[str, int]]:
    snapshot = kernel32.CreateToolhelp32Snapshot(0x00000002, 0)  # TH32CS_SNAPPROCESS
    if snapshot == -1:
        return {}
    entries: dict[int, tuple[str, int]] = {}
    entry = PROCESSENTRY32()
    entry.dwSize = ctypes.sizeof(PROCESSENTRY32)
    if kernel32.Process32First(snapshot, ctypes.byref(entry)):
        while True:
            entries[entry.th32ProcessID] = (
                entry.szExeFile.decode(errors="replace"),
                entry.th32ParentProcessID,
            )
            if not kernel32.Process32Next(snapshot, ctypes.byref(entry)):
                break
    kernel32.CloseHandle(snapshot)
    return entries


def process_tree_memory(root_pid: int) -> int:
    entries = snapshot_processes()
    children: dict[int, list[int]] = {}
    for pid, (_, parent) in entries.items():
        children.setdefault(parent, []).append(pid)
    stack = [root_pid]
    seen: set[int] = set()
    total = 0
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        memory = process_memory(pid)
        if memory:
            total += memory
        stack.extend(children.get(pid, []))
    return total


def process_cpu_100ns(pid: int) -> int | None:
    handle = kernel32.OpenProcess(0x1000, False, pid)
    if not handle:
        return None
    try:
        creation = wintypes.FILETIME()
        exit_time = wintypes.FILETIME()
        kernel = wintypes.FILETIME()
        user = wintypes.FILETIME()
        if kernel32.GetProcessTimes(
            handle,
            ctypes.byref(creation),
            ctypes.byref(exit_time),
            ctypes.byref(kernel),
            ctypes.byref(user),
        ):
            kernel_total = (kernel.dwHighDateTime << 32) | kernel.dwLowDateTime
            user_total = (user.dwHighDateTime << 32) | user.dwLowDateTime
            return kernel_total + user_total
        return None
    finally:
        kernel32.CloseHandle(handle)


def process_cpu_percent(pid: int, window: float = 1.0) -> float:
    """One process's CPU use over `window` seconds, as a percentage of one core.

    The framework process is the stable signal: the webview's own processes do
    background work that belongs to the page, not to the runtime.
    """
    start = process_cpu_100ns(pid)
    time.sleep(window)
    end = process_cpu_100ns(pid)
    if not start or not end:
        return 0.0
    return (end - start) / 10_000_000.0 / window * 100.0


def kill_tree(pid: int) -> None:
    subprocess.run(
        ["taskkill", "/F", "/PID", str(pid), "/T"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def kill_by_path_prefix(prefix: str) -> None:
    for pid in snapshot_processes():
        path = process_path(pid)
        if path and path.lower().startswith(prefix.lower()):
            kill_tree(pid)


def wait_for_file(path: Path, timeout: float = 30.0) -> dict | None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.is_file():
            try:
                return json.loads(path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                pass
        time.sleep(0.05)
    return None


def dir_size(path: Path) -> int:
    if not path.exists():
        return 0
    total = 0
    for entry in path.rglob("*"):
        if entry.is_file():
            try:
                total += entry.stat().st_size
            except OSError:
                pass
    return total


def app_size(path: Path, exclude: set[str]) -> int:
    total = 0
    for entry in path.rglob("*"):
        if entry.is_file() and not any(part in exclude for part in entry.parts):
            try:
                total += entry.stat().st_size
            except OSError:
                pass
    return total


# ----------------------------------------------------------------------------------------------
# Building
# ----------------------------------------------------------------------------------------------


def resolve_command(cmd: list[str]) -> list[str]:
    """Resolves the executable, routing `.cmd`/`.bat` through `cmd /c`.

    `subprocess` on Windows uses `CreateProcess`, which does not run a `.cmd` directly;
    `npm` is `npm.cmd`, so an unresolved `npm` is `WinError 2`.
    """
    executable = shutil.which(cmd[0])
    if executable is None:
        return cmd
    if executable.lower().endswith((".cmd", ".bat")):
        return ["cmd", "/c", executable, *cmd[1:]]
    return [executable, *cmd[1:]]


def run(cmd: list[str], cwd: Path, timeout: float = 900, env: dict | None = None) -> str:
    merged = os.environ.copy()
    if env:
        merged.update(env)
    completed = subprocess.run(
        resolve_command(cmd),
        cwd=str(cwd),
        env=merged,
        capture_output=True,
        text=True, encoding="utf-8",
        timeout=timeout,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"command failed ({completed.returncode}): {' '.join(cmd)}\n"
            f"{completed.stdout[-4000:]}\n{completed.stderr[-4000:]}"
        )
    return completed.stdout


def ensure_originals(originals_dir: Path, manifest: dict) -> None:
    originals_dir.mkdir(parents=True, exist_ok=True)
    for app in manifest["apps"]:
        dest = originals_dir / app["name"]
        if not dest.exists():
            run(
                ["git", "clone", "--depth", "1", app["source"], str(dest)],
                cwd=originals_dir,
                timeout=600,
            )
        commit = run(["git", "rev-parse", "HEAD"], cwd=dest, timeout=60).strip()
        if commit != app["commit"]:
            run(
                ["git", "fetch", "--depth", "1", "origin", app["commit"]],
                cwd=dest,
                timeout=600,
            )
            run(["git", "checkout", app["commit"]], cwd=dest, timeout=60)
            # The pin *is* the measurement: a checkout that left the tree on another revision
            # would produce a report about an application a reader cannot identify. Checking
            # afterwards turns the manifest's commit from an intention into a fact about the
            # run.
            checked = run(["git", "rev-parse", "HEAD"], cwd=dest, timeout=60).strip()
            if checked != app["commit"]:
                raise RuntimeError(
                    f"{app['name']} is at {checked}, not the pinned {app['commit']}"
                )


# The instrumentation file that registers the bridge command the manifest names for each
# framework. The manifest's `ipc` field is a statement about the tree, not a note beside it:
# a rename on either side has to fail here, where the mismatch is obvious, rather than in a
# report a reader cannot reproduce.
INSTRUMENTATION_HANDLER = {
    "electron": "main.js",
    "tauri": "main.rs",
}


def apply_instrumentation(framework: str, command: str, dest: Path) -> None:
    for source_name, target_name in INSTRUMENTATION_MAP[framework].items():
        source = INSTRUMENTATION / framework / source_name
        target = dest / target_name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
    handler = INSTRUMENTATION / framework / INSTRUMENTATION_HANDLER[framework]
    if command not in handler.read_text(encoding="utf-8"):
        raise RuntimeError(
            f"the {framework} instrumentation does not handle {command!r}, "
            "which comparison/manifest.json names as its bridge command"
        )


def build_electron(dest: Path) -> Path:
    if not (dest / "node_modules").exists():
        run(["npm", "install"], cwd=dest, timeout=900)
    binary = dest / "node_modules" / "electron" / "dist" / "electron.exe"
    if not binary.exists():
        # npm's install-scripts policy can skip Electron's postinstall, which is the step
        # that downloads the binary.
        run(["node", "node_modules/electron/install.js"], cwd=dest, timeout=900)
    if not binary.exists():
        raise RuntimeError(f"electron binary not found at {binary}")
    return binary


def build_tauri(dest: Path) -> Path:
    if not (dest / "node_modules").exists():
        run(["npm", "install"], cwd=dest, timeout=900)
    if not (dest / "node_modules" / "esbuild-windows-64" / "esbuild.exe").exists():
        # npm's install-scripts policy can skip esbuild's postinstall, which is the step
        # that downloads the binary `vite build` links against.
        run(["node", "node_modules/esbuild/install.js"], cwd=dest, timeout=300)
    run(["npm", "run", "build"], cwd=dest, timeout=900)
    run(["npm", "run", "tauri", "--", "build", "-b", "none"], cwd=dest, timeout=1200)
    binary = dest / "src-tauri" / "target" / "release" / "app.exe"
    if not binary.exists():
        candidates = list((dest / "src-tauri" / "target" / "release").glob("*.exe"))
        if not candidates:
            raise RuntimeError("tauri binary not found")
        binary = candidates[0]
    return binary


def build_ncui() -> Path:
    # Built unconditionally, not only when the binary is missing: a `target/release/ncui.exe`
    # left by an earlier default build is a real binary that does not carry `wry-host`, and the
    # size and island behaviour this harness measures are properties of the feature set. An
    # incremental cargo build is a no-op when the tree is already built, so the cost of being
    # sure is nothing; the cost of reusing a stale binary is a report about a build that was
    # never measured.
    binary = REPO / "target" / "release" / "ncui.exe"
    run(
        [
            "cargo",
            "build",
            "--release",
            "-p",
            "ncui-cli",
            "--features",
            "ncui-cli/wry-host",
        ],
        cwd=REPO,
        timeout=1200,
    )
    if not binary.exists():
        raise RuntimeError("ncui binary not found")
    return binary


# ----------------------------------------------------------------------------------------------
# Measurement
# ----------------------------------------------------------------------------------------------


def percentiles(samples: list[float]) -> dict:
    if not samples:
        return {"min": None, "p50": None, "p90": None, "max": None}
    ordered = sorted(samples)
    return {
        "min": ordered[0],
        "p50": statistics.median(ordered),
        "p90": ordered[min(len(ordered) - 1, int(round(0.9 * (len(ordered) - 1))))],
        "max": ordered[-1],
    }


def measure(
    label: str,
    app: str,
    framework: str,
    cmd: list[str],
    cwd: Path,
    idle_env: dict,
    bench_env: dict,
    title: str,
    path_prefix: str,
    bench_output: Path,
    enable_bench: callable,
    disable_bench: callable,
    repeats: int,
) -> dict:
    enable_bench()
    startups: list[float] = []
    memories: list[int] = []
    cpu_samples: list[float] = []
    ipc: dict | None = None
    ipc_samples: list[float] = []
    for _ in range(repeats):
        # The idle run carries no benchmark trigger, so the page settles at its own
        # baseline. Measuring the working set after a benchmark would charge the
        # benchmark's own allocations to the framework.
        disable_bench()
        kill_by_path_prefix(path_prefix)
        time.sleep(0.5)
        merged = os.environ.copy()
        merged.update(idle_env)
        process = subprocess.Popen(
            cmd,
            cwd=str(cwd),
            env=merged,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        started = time.monotonic()
        if not wait_for_window(title, 30):
            kill_tree(process.pid)
            raise RuntimeError(f"{label}: window {title!r} did not appear")
        startups.append((time.monotonic() - started) * 1000.0)
        time.sleep(5.0)
        cpu_samples.append(process_cpu_percent(process.pid, 2.0))
        memories.append(process_tree_memory(process.pid))
        kill_tree(process.pid)
        time.sleep(0.5)

        # The benchmark run enables the trigger and reads what the page measured.
        enable_bench()
        kill_by_path_prefix(path_prefix)
        time.sleep(0.5)
        merged = os.environ.copy()
        merged.update(bench_env)
        process = subprocess.Popen(
            cmd,
            cwd=str(cwd),
            env=merged,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if not wait_for_window(title, 30):
            kill_tree(process.pid)
            raise RuntimeError(f"{label}: window {title!r} did not appear")
        result = wait_for_file(bench_output, 30)
        if result is not None:
            ipc = result
            per_round = result.get("per_round_ms")
            if isinstance(per_round, (int, float)):
                ipc_samples.append(float(per_round))
        kill_tree(process.pid)
        time.sleep(0.5)
    disable_bench()
    if ipc is not None and ipc_samples:
        ipc["per_round_ms_samples"] = ipc_samples
        ipc["per_round_ms_min"] = min(ipc_samples)
        ipc["per_round_ms"] = statistics.median(ipc_samples)
        ipc["per_round_ms_max"] = max(ipc_samples)
    return {
        "label": label,
        "app": app,
        "framework": framework,
        "startup_ms": percentiles(startups),
        "memory_bytes": percentiles([float(value) for value in memories]),
        "cpu_percent": percentiles(cpu_samples),
        "ipc": ipc,
    }


def write_bench_config(port: Path, rounds: int) -> None:
    (port / "bench-config.json").write_text(
        json.dumps({"rounds": rounds, "output": "bench-result.json"}),
        encoding="utf-8",
    )


def clear_ncui_bench_files(port: Path) -> None:
    for name in ("bench-config.json", "bench-result.json"):
        path = port / name
        if path.exists():
            path.unlink()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--rounds", type=int, default=200)
    parser.add_argument("--only", default=None, help="only this application name")
    parser.add_argument("--originals-dir", default=str(DEFAULT_ORIGINALS))
    parser.add_argument("--output", default=str(RESULTS / "windows.json"))
    args = parser.parse_args()

    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    originals_dir = Path(args.originals_dir)
    ensure_originals(originals_dir, manifest)

    ncui_exe = build_ncui()
    RESULTS.mkdir(parents=True, exist_ok=True)

    configs = []
    for app in manifest["apps"]:
        if args.only and app["name"] != args.only:
            continue
        name = app["name"]
        original = originals_dir / name
        port = COMPARISON / app["port"]
        title = app["window_title"]

        if app["framework"] == "electron":
            apply_instrumentation("electron", app["ipc"], original)
            electron_exe = build_electron(original)
            configs.append(
                {
                    "label": f"{name} on Electron",
                    "app": name,
                    "framework": "electron",
                    "cmd": [str(electron_exe), "."],
                    "cwd": original,
                    "idle_env": {},
                    "bench_env": {
                        "NCUI_BENCH_OUTPUT": str(RESULTS / f"{name}-electron-ipc.json"),
                        "NCUI_BENCH_ROUNDS": str(args.rounds),
                    },
                    "title": title,
                    "path_prefix": str(original / "node_modules" / "electron" / "dist"),
                    "bench_output": RESULTS / f"{name}-electron-ipc.json",
                    "enable_bench": lambda: None,
                    "disable_bench": lambda name=name: (RESULTS / f"{name}-electron-ipc.json").unlink(missing_ok=True),
                    "size": dir_size(original / "node_modules" / "electron" / "dist")
                    + app_size(original, {".git", "node_modules"}),
                }
            )
        elif app["framework"] == "tauri":
            apply_instrumentation("tauri", app["ipc"], original)
            tauri_exe = build_tauri(original)
            configs.append(
                {
                    "label": f"{name} on Tauri",
                    "app": name,
                    "framework": "tauri",
                    "cmd": [str(tauri_exe)],
                    "cwd": original,
                    "idle_env": {},
                    "bench_env": {
                        "NCUI_BENCH_OUTPUT": str(RESULTS / f"{name}-tauri-ipc.json"),
                        "NCUI_BENCH_ROUNDS": str(args.rounds),
                    },
                    "title": title,
                    "path_prefix": str(tauri_exe.parent),
                    "bench_output": RESULTS / f"{name}-tauri-ipc.json",
                    "enable_bench": lambda: None,
                    "disable_bench": lambda name=name: (RESULTS / f"{name}-tauri-ipc.json").unlink(missing_ok=True),
                    "size": tauri_exe.stat().st_size + dir_size(original / "dist"),
                }
            )
        else:
            raise RuntimeError(f"unknown framework {app['framework']}")

        configs.append(
            {
                "label": f"{name} on ncui",
                "app": name,
                "framework": "ncui",
                "cmd": [str(ncui_exe), "run", str(port)],
                "cwd": REPO,
                "idle_env": {},
                "bench_env": {},
                "title": title,
                "path_prefix": str(ncui_exe.parent),
                "bench_output": port / "bench-result.json",
                "enable_bench": lambda port=port: write_bench_config(port, args.rounds),
                "disable_bench": lambda port=port: clear_ncui_bench_files(port),
                "size": ncui_exe.stat().st_size + dir_size(port),
            }
        )

    report = {
        "schema_version": 1,
        "platform": "windows",
        "repeats": args.repeats,
        "rounds": args.rounds,
        "configs": [],
    }
    for config in configs:
        print(f"measuring {config['label']}…", flush=True)
        measurement = measure(
            label=config["label"],
            app=config["app"],
            framework=config["framework"],
            cmd=config["cmd"],
            cwd=config["cwd"],
            idle_env=config["idle_env"],
            bench_env=config["bench_env"],
            title=config["title"],
            path_prefix=config["path_prefix"],
            bench_output=config["bench_output"],
            enable_bench=config["enable_bench"],
            disable_bench=config["disable_bench"],
            repeats=args.repeats,
        )
        measurement["size_bytes"] = config["size"]
        report["configs"].append(measurement)
        startup = measurement["startup_ms"]["p50"]
        memory = measurement["memory_bytes"]["p50"]
        cpu = measurement["cpu_percent"]["p50"]
        per_round = (measurement["ipc"] or {}).get("per_round_ms")
        print(
            f"  startup p50 {startup:.1f} ms · idle p50 {memory / 1_048_576:.1f} MiB · "
            f"cpu p50 {cpu:.2f} % · "
            f"ipc {per_round if per_round is None else f'{per_round:.3f} ms/round'} · "
            f"size {measurement['size_bytes'] / 1_048_576:.1f} MiB",
            flush=True,
        )

    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"\nwrote {output}")


if __name__ == "__main__":
    main()
