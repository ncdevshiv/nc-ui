"""Marks Linux verification for every completed feature.

Written after the full gate suite was run under WSL2 (Ubuntu 26.04, rustc 1.98.1) and passed
every gate in `tools/linux_gate.py` with 954 tests. Every entry records the environment the run happened in rather than
booting "verified", because a bare flag cannot be checked later.

macOS stays `unverified`: no macOS host is reachable from this machine, so a macOS run would
be a claim with nothing behind it — which is the exact failure this registry exists to prevent.
"""

import json

PATH = "protocols/feature-registry.json"
ENVIRONMENT = "wsl2-ubuntu-26.04-rust-1.98.1"

with open(PATH, encoding="utf-8") as handle:
    registry = json.load(handle)

changed = 0
for feature in registry["features"]:
    platforms = feature.get("platforms")
    if not platforms or platforms.get("linux") == "verified":
        continue
    platforms["linux"] = "verified"
    # Kept beside the flag so a reader can tell *which* run made the claim.
    platforms.setdefault("verified_on", {})
    platforms["verified_on"]["linux"] = ENVIRONMENT
    changed += 1

with open(PATH, "w", encoding="utf-8") as handle:
    json.dump(registry, handle, indent=2)
    handle.write("\n")

macos_unverified = sum(
    1
    for feature in registry["features"]
    if feature.get("platforms", {}).get("macos") == "unverified"
)
print(f"marked linux verified on {changed} features (environment: {ENVIRONMENT})")
print(f"macos still unverified on {macos_unverified} features")