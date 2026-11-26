#!/usr/bin/env python3
"""Run a command in a temporary D-Bus session with the real dunst notification service."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile

DUNST = Path("/home/shiv/.local/ncui-linux-tools/usr/bin/dunst")
SERVICE = "org.freedesktop.Notifications"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("a command is required")
    if not DUNST.is_file():
        parser.error(f"the notification daemon is missing: {DUNST}")

    with tempfile.TemporaryDirectory(prefix="ncui-notification-session-") as directory:
        root = Path(directory)
        services = root / "data" / "dbus-1" / "services"
        services.mkdir(parents=True)
        (services / f"{SERVICE}.service").write_text(
            f"[D-BUS Service]\nName={SERVICE}\nExec={DUNST}\nSystemdService=\n",
            encoding="utf-8",
        )
        environment = os.environ.copy()
        environment.pop("WAYLAND_DISPLAY", None)
        environment["GDK_BACKEND"] = "x11"
        environment["XDG_DATA_HOME"] = str(root / "data")
        environment["XDG_CONFIG_HOME"] = str(root / "config")
        return subprocess.run(
            ["dbus-run-session", "--", *command], env=environment, check=False
        ).returncode


if __name__ == "__main__":
    sys.exit(main())
