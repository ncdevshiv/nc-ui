#!/usr/bin/env python3
"""Target-directory disk-budget check.

Reports the size of the Cargo target directory, the Cargo registry cache, and the
comparison-clones directory, and fails if any exceeds a configured ceiling. The
goal is to prevent the "100+ GB of build artifacts" failure mode that plagues
large Rust workspaces: a runaway `target/` is a sign that something is being
rebuilt that should not be, or that old build artifacts are never pruned.

Usage:
    python scripts/disk_budget.py [--check] [--warn-only]

Exit codes:
    0  all measured directories are within budget (or --warn-only)
    1  at least one directory exceeds its ceiling
    2  a usage error

The script is stdlib-only so it runs on any machine that has Python 3.10+,
including CI runners that have not built the Rust workspace yet.
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

# Ceilings in gibibytes (GiB). These are deliberately generous for a workspace
# of this size (19 crates + tools + comparison clones) but finite: a clean
# target/ for this workspace should be well under 20 GiB, and a dirty one under
# 60 GiB. The registry cache and comparison clones are bounded separately.
DEFAULT_CEILINGS_GIB: dict[str, float] = {
    "target": 60.0,
    "cargo_registry": 20.0,
    "comparison_clones": 5.0,
}

GIB = 1024**3


def directory_size_gib(path: Path) -> float | None:
    """Return the size of `path` in GiB, or None if it does not exist.

    Uses an iterative os.scandir walk (faster than os.walk + os.path.getsize on
    Windows because DirEntry.stat() batches metadata). Symlinked directories are
    not followed, since they are common in target/ and following them would
    double-count.
    """
    if not path.exists():
        return None
    total = 0
    stack = [path]
    while stack:
        current = stack.pop()
        try:
            with os.scandir(current) as entries:
                for entry in entries:
                    try:
                        if entry.is_dir(follow_symlinks=False):
                            stack.append(Path(entry.path))
                        elif entry.is_file(follow_symlinks=False):
                            total += entry.stat(follow_symlinks=False).st_size
                    except OSError:
                        pass
        except OSError:
            pass
    return total / GIB


def repo_root() -> Path:
    """Return the repository root.

    Prefers the git toplevel so the script works from any subdirectory; falls
    back to the script's own grandparent (scripts/ -> repo root).
    """
    try:
        import subprocess

        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            encoding="utf-8",
            timeout=5,
        )
        if result.returncode == 0:
            return Path(result.stdout.strip())
    except Exception:
        pass
    return Path(__file__).resolve().parent.parent


def cargo_target_dir(root: Path) -> Path:
    """Return the Cargo target directory.

    Respects CARGO_TARGET_DIR if set (CI often sets it to a shared cache);
    otherwise defaults to <root>/target.
    """
    override = os.environ.get("CARGO_TARGET_DIR")
    if override:
        return Path(override)
    return root / "target"


def cargo_registry_dir() -> Path:
    """Return the Cargo registry cache directory.

    Respects CARGO_HOME if set; otherwise defaults to ~/.cargo/registry.
    """
    cargo_home = os.environ.get("CARGO_HOME")
    if cargo_home:
        return Path(cargo_home) / "registry"
    return Path.home() / ".cargo" / "registry"


def comparison_clones_dir(root: Path) -> Path:
    """Return the comparison-harness clones directory."""
    return root / "comparison" / ".originals"


def measured_directories(root: Path) -> dict[str, Path]:
    """Return the mapping of budget name -> directory to measure."""
    return {
        "target": cargo_target_dir(root),
        "cargo_registry": cargo_registry_dir(),
        "comparison_clones": comparison_clones_dir(root),
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check that build-artifact directories stay within budget."
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="Fail (exit 1) if any directory exceeds its ceiling. This is the default.",
    )
    parser.add_argument(
        "--warn-only",
        action="store_true",
        help="Report sizes but never fail, regardless of ceiling breaches.",
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=None,
        help="Repository root (defaults to the git toplevel or the script's grandparent).",
    )
    args = parser.parse_args()

    root = args.root if args.root else repo_root()
    directories = measured_directories(root)

    failed = False
    print(f"disk budget (repo root: {root})")
    print(f"  {'directory':<20} {'size':>10}  {'ceiling':>10}  status")
    for name, path in directories.items():
        ceiling = DEFAULT_CEILINGS_GIB[name]
        size = directory_size_gib(path)
        if size is None:
            print(f"  {name:<20} {'--':>10}  {ceiling:>9.1f}G  (absent)")
            continue
        status = "ok"
        if size > ceiling:
            status = "OVER BUDGET"
            failed = True
        print(f"  {name:<20} {size:>9.2f}G  {ceiling:>9.1f}G  {status}")

    if failed and not args.warn_only:
        print("\ndisk budget exceeded. Run `cargo clean` or remove stale target/ subdirectories.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
