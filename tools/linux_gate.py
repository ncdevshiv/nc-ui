"""Runs the full gate on Linux (WSL2) and tallies the results.

Kept as a file because the awk and quoting inside a `wsl.exe bash -lc` one-liner are fragile,
and this script needs to be re-runnable whenever a cross-platform defect is fixed.

The repository is located rather than named: an earlier version hard-coded the drive letter the
tree happened to be checked out on, so the gate could not run at all once the tree moved, and it
failed with a directory error rather than with a result. It is read from this file's own location,
which is the one thing that is always right.

What it runs is `make gate`'s stage list — fmt, clippy, tests, rustdoc, the tools workspace, every
policy gate, and the client package — so the Linux result is comparable with the Windows one. The
`--all-features` variants are not included: they enable the wry engine, which needs the GTK and
WebKit development libraries, and `make gate` does not run them either.

The distro must be able to compile and link: `build-essential`, and `pkg-config` for the crates that
probe for system libraries.
"""

import re
import subprocess
import sys
from pathlib import Path

WSL = ["wsl.exe", "-d", "Ubuntu-26.04", "--", "bash", "-lc"]

# The repository, as the Linux side sees it. `G:\nc-ui` becomes `/mnt/g/nc-ui` — the drive letter # ncui-allow-hygiene: doc example of WSL path mapping
# lower-cased, because the mount point is lower-cased and Linux is case-sensitive about it.
_WINDOWS_ROOT = Path(__file__).resolve().parents[1]
_DRIVE, _, _REST = str(_WINDOWS_ROOT).partition(":")
REPO = f"/mnt/{_DRIVE.lower()}/{_REST.replace(chr(92), '/').lstrip('/')}"

# The policy tools live in `tools/target/`, which the Windows build shares. The two hosts
# overwrite each other there, so a Linux build's ELF binaries shadow the Windows PE ones and
# the next gate on either host fails with `Exec format error`. Building into a host-specific
# directory is what makes running both gates safe in any order.
#
# The directory is under `/var/tmp`, not `/tmp`. On this host `/tmp` is a 12 GiB tmpfs — RAM —
# and a full workspace build (two target directories, all the dependency crates, the test
# binaries) exceeds it, so the linker dies with `ld terminated with signal 7 [Bus error]` once
# the tmpfs fills. `/var/tmp` is on the root filesystem, which has room, and it persists across
# reboots, so the incremental cache survives between gate runs the way a target directory should.
TOOLS_TARGET = "/var/tmp/ncui-tools-linux"
TOOLS_BIN = f"{TOOLS_TARGET}/release"

# The workspace's own target directory is shared in the same way, and the clash is worse there:
# the test binaries are extensionless files that one host cannot execute and the other cannot
# overwrite cleanly. The Windows build keeps its own `target/`; the Linux build keeps this one.
TARGET = "/var/tmp/ncui-target-linux"

# Clipboard and notification tools were unpacked user-locally because this WSL image has no
# root package manager. Their transitive libraries live beside them; a gate that proves only
# that the binaries exist would still miss the runtime failure this environment exposed.
LINUX_TOOLS = "/home/shiv/.local/ncui-linux-tools"
PLATFORM_ENV = (
    "env GDK_BACKEND=x11 DISPLAY=:0 WAYLAND_DISPLAY= LIBGL_ALWAYS_SOFTWARE=1 "
    f"PATH={LINUX_TOOLS}/usr/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin "
    f"LD_LIBRARY_PATH={LINUX_TOOLS}/usr/lib/x86_64-linux-gnu "
)
# WSLg exposes a device node without granting this user access. Mesa's software Vulkan driver
# still executes the real render, readback and island-route assertions without a silent skip.
GPU_ENV = "VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json WGPU_BACKEND=vulkan "

GATES = [
    # The policy tools live in one target directory shared with the Windows build, so a
    # Windows `cargo build --release` leaves a PE binary at the extensionless path and this
    # gate gets `Exec format error`. Building here first makes the binaries Linux ones.
    ("build tools", f"cd tools && CARGO_TARGET_DIR={TOOLS_TARGET} cargo build --release"),
    ("fmt", "cargo fmt --all -- --check"),
    ("clippy", f"CARGO_TARGET_DIR={TARGET} cargo clippy --workspace --all-targets -- -D warnings"),
    # The default configuration excludes the window host, so nothing above compiles
    # `ncui-webview-wry` over `wry`/`tao`, the renderer over `wgpu`, or the shaper. The
    # release binary is built with `wry-host`, and that configuration had never been
    # compiled on this host: an earlier revision of this file claimed the GTK and WebKit
    # development libraries were unavailable, and they are present. This is the stage that
    # makes the shipped configuration a compiled one on Linux rather than a Windows-only
    # assumption. The real-window tests run in a later stage once the session has a display.
    (
        "release configuration compiles",
        f"CARGO_TARGET_DIR={TARGET} cargo check -p ncui-cli --features ncui-cli/wry-host",
    ),
    # The unit tests of the release configuration, which need no display: the runtime's
    # dispatch, window registry, transport and application logic all run against the real
    # engine feature set, and the host, renderer and CLI are compiled and exercised the way
    # the shipped binary is.
    (
        "release configuration tests",
        f"CARGO_TARGET_DIR={TARGET} cargo test -p ncui-runtime --features wry-host --lib"
        f" && CARGO_TARGET_DIR={TARGET} cargo test -p ncui-webview-wry --features wry-engine,native-menu --lib"
        f" && CARGO_TARGET_DIR={TARGET} cargo test -p ncui-gpu-renderer --features text-shaping,wgpu-device --lib"
        f" && CARGO_TARGET_DIR={TARGET} cargo test -p ncui-cli --features wry-host --lib",
    ),
    # The real-window integration tests. Pin the X11 session rather than inheriting WSLg's
    # ambient display: the runtime's application suite opens genuine windows, while the
    # runtime page bridge uses a fake engine and the wry page bridge exercises the browser.
    # Naming both targets keeps a policy-only pass from being reported as browser evidence.
    (
        "real-window integration tests",
        f"{PLATFORM_ENV}CARGO_TARGET_DIR={TARGET} cargo test -p ncui-runtime "
        "--features wry-host --test application --test page_bridge --test ports"
        f" && {PLATFORM_ENV}CARGO_TARGET_DIR={TARGET} cargo test -p ncui-webview-wry "
        "--features wry-engine --test page_bridge",
    ),
    (
        "platform capability integration tests",
        f"{PLATFORM_ENV}NCUI_REQUIRE_CLIPBOARD=1 CARGO_TARGET_DIR={TARGET} cargo test "
        "-p ncui-native --lib clipboard::tests::text_written_to_the_clipboard_is_read_back "
        "-- --exact"
        f" && {PLATFORM_ENV}python3 tools/with_notification_daemon.py -- env "
        f"NCUI_REQUIRE_NOTIFICATION=1 CARGO_TARGET_DIR={TARGET} cargo test -p ncui-native "
        "--lib notify::tests::a_real_notification_is_accepted_by_the_platform -- --exact",
    ),
    # GPU integration tests normally return early on a machine without an adapter. That is
    # useful for a developer's default suite, but it is not platform evidence: libtest counts
    # the early return as a pass. This stage turns adapter absence into a failure and runs
    # both the renderer's pixel readback and the runtime's live island route.
    (
        "GPU integration tests",
        f"{PLATFORM_ENV}{GPU_ENV}NCUI_REQUIRE_GPU=1 CARGO_TARGET_DIR={TARGET} cargo test "
        "-p ncui-gpu-renderer --features text-shaping,wgpu-device "
        "--test render_offscreen --test island_pixels"
        f" && {PLATFORM_ENV}{GPU_ENV}NCUI_REQUIRE_GPU=1 CARGO_TARGET_DIR={TARGET} cargo test "
        "-p ncui-runtime --features wry-host,gpu-islands --test islands",
    ),
    ("tests", f"CARGO_TARGET_DIR={TARGET} cargo test --workspace --all-targets"),
    (
        "docs",
        f'RUSTDOCFLAGS="-D warnings" CARGO_TARGET_DIR={TARGET} cargo doc --workspace --no-deps',
    ),
    ("tools fmt", "cd tools && cargo fmt --all -- --check"),
    (
        "tools clippy",
        f"cd tools && CARGO_TARGET_DIR={TOOLS_TARGET} cargo clippy --all-targets -- -D warnings",
    ),
    ("tools tests", f"cd tools && CARGO_TARGET_DIR={TOOLS_TARGET} cargo test --all-targets"),
    ("arch", f"{TOOLS_BIN}/ncui-arch ."),
    (
        "registry",
        f"{TOOLS_BIN}/ncui-registry protocols/feature-registry.json",
    ),
    # The registry validator proves a cited file exists, not that the file says what the
    # citation claims about it. This is the half that *is* mechanically checkable: a test
    # count re-derived from the file it names. See tools/correct_test_counts.py.
    ("registry counts", "python3 tools/correct_test_counts.py --check"),
    ("placeholders crates", f"{TOOLS_BIN}/ncui-placeholders crates protocols"),
    ("placeholders tools", f"cd tools && {TOOLS_BIN}/ncui-placeholders ncui-tools"),
    ("placeholders packages", f"{TOOLS_BIN}/ncui-placeholders packages"),
    ("placeholders examples", f"{TOOLS_BIN}/ncui-placeholders examples"),
    (
        "placeholders comparison",
        f"{TOOLS_BIN}/ncui-placeholders comparison/ports comparison/bench/instrumentation",
    ),
    ("component example", "python3 tools/make_greeter_component.py --check"),
    (
        "component word signal",
        "python3 comparison/tools/make_word_signal_component.py --check",
    ),
    ("bundler", f"CARGO_TARGET_DIR={TARGET} cargo test -p ncui-cli --test bundle"),
    # The files are named rather than the directory given: `node --test <directory>` resolves the
    # directory as a module on some Node versions and scans it on others, so the same command
    # passes on one host and reports `MODULE_NOT_FOUND` on the next.
    ("client", "node --test packages/ncui-client/tests/*.test.js"),
]

RESULT = re.compile(r"^test result: ok\. (\d+) passed; (\d+) failed", re.M)


def run(command):
    """Runs one gate under WSL and returns (exit_code, combined_output)."""
    full = f"source ~/.cargo/env && cd {REPO} && {command}"
    proc = subprocess.run(
        WSL + [full], capture_output=True, text=True, encoding="utf-8", timeout=3600
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
