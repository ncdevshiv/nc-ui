#!/usr/bin/env python3
"""Audit Linux verification gaps without changing the feature registry.

A full workspace gate is not evidence that every feature was exercised on Linux: window,
browser, GPU and platform integrations can be feature-gated or return early. Verification
flags therefore require an explicit review of the evidence for each feature. This tool only
reports the outstanding reviews; it never rewrites the registry.

    python tools/record_linux_verification.py
    python tools/record_linux_verification.py --fail-on-gap
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

REGISTRY = Path(__file__).resolve().parent.parent / "protocols" / "feature-registry.json"


def linux_gaps(registry: dict) -> list[str]:
    """Completed features whose Linux platform state is still unverified."""
    return [
        feature["id"]
        for feature in registry["features"]
        if feature.get("status") == "complete"
        and feature.get("platforms", {}).get("linux") == "unverified"
    ]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--fail-on-gap",
        action="store_true",
        help="exit 1 when a completed feature still lacks Linux verification",
    )
    args = parser.parse_args()
    registry = json.loads(REGISTRY.read_text(encoding="utf-8"))
    gaps = linux_gaps(registry)
    print(f"linux verification audit: {len(gaps)} completed feature(s) unverified")
    for feature_id in gaps:
        print(f"  {feature_id}")
    print("audit only: the registry was not changed")
    return 1 if args.fail_on_gap and gaps else 0


if __name__ == "__main__":
    raise SystemExit(main())
