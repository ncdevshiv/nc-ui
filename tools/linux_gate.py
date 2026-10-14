"""Runs the full gate on Linux (WSL2) and tallies the results.

Kept as a file because the awk and quoting inside a `wsl.exe bash -lc` one-liner are fragile,
and this script needs to be re-runnable whenever a cross-platform defect is fixed.
"""

import re
import subprocess
import sys

WSL = ["wsl.exe", "-d", "Ubuntu-26.04", "--", "bash", "-lc"]
REPO = "/mnt/f/nc-ui"

# The policy tools live in `tools/target/`, which the Windows build shares. The two hosts
# overwrite each other there, so a Linux build's ELF binaries shadow the Windows PE ones and
# the next gate on either host fails with `Exec format error`. Building into a host-specific
# directory is what makes running both gates safe in any order.
TOOLS_TARGET = "/tmp/ncui-tools-linux"

GATES = [
    # The policy tools live in one target directory shared with the Windows build, so a
    # Windows `cargo build --release` leaves a PE binary at the extensionless path and this
    # gate gets `Exec format error`. Building here first makes the binaries Linux ones.
    ("build tools", "cd tools && CARGO_TARGET_DIR=/tmp/ncui-tools-linux cargo build --release"),
    ("fmt", "cargo fmt --all -- --check"),
    ("clippy", "cargo clippy --workspace --all-targets -- -D warnings"),
    ("tests", "cargo test --workspace --all-targets"),
    (
        "docs",
        'RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps',
    ),
    ("tools tests", "cd tools && cargo test --all-targets"),
    ("tools clippy", "cd tools && cargo clippy --all-targets -- -D warnings"),
    ("arch", "/tmp/ncui-tools-linux/release/ncui-arch ."),
    (
        "registry",
        "/tmp/ncui-tools-linux/release/ncui-registry protocols/feature-registry.json",
    ),
    # The registry validator proves a cited file exists, not that the file says what the
    # citation claims about it. This is the half that *is* mechanically checkable: a test
    # count re-derived from the file it names. See tools/correct_test_counts.py.
    ("registry counts", "python3 tools/correct_test_counts.py --check"),
    ("placeholders crates", "/tmp/ncui-tools-linux/release/ncui-placeholders crates protocols"),
    ("placeholders tools", "cd tools && /tmp/ncui-tools-linux/release/ncui-placeholders ncui-tools"),
]

RESULT = re.compile(r"^test result: ok\. (\d+) passed; (\d+) failed", re.M)


def run(command):
    """Runs one gate under WSL and returns (exit_code, combined_output)."""
    full = f"source ~/.cargo/env && cd {REPO} && {command}"
    proc = subprocess.run(
        WSL + [full], capture_output=True, text=True, timeout=3600
    )
    return proc.returncode, proc.stdout + proc.stderr


def main():
    failures = 0
    for name, command in GATES:
        code, output = run(command)
        if code == 0:
            extra = ""
            tally = RESULT.findall(output)
            if tally:
                passed = sum(int(p) for p, _ in tally)
                failed = sum(int(f) for _, f in tally)
                extra = f" ({passed} passed, {failed} failed)"
            print(f"  PASS  {name}{extra}")
        else:
            failures += 1
            print(f"  FAIL  {name} (exit {code})")
            for line in output.splitlines():
                if re.match(r"^(error|warning|test .* FAILED)", line):
                    print(f"        {line}")

    print(f"\nlinux gate: {len(GATES) - failures} passed, {failures} failed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())