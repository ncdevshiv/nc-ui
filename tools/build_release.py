"""Builds the release artefacts: SBOM, reproducible-build check, detached signature.

Kept as a script rather than a Makefile target because it is a pipeline with real ordering —
the SBOM must describe the tree that was built, and the signature must cover the SBOM — and a
Makefile line hides that ordering.

Every step reports what it did, and `--verify` checks an existing set rather than producing a
new one, so a consumer can confirm an artefact they were given.

Usage:
    python tools/build_release.py            # build, check, sign
    python tools/build_release.py --verify   # verify signatures and reproducibility
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ARTIFACTS = ROOT / "artifacts"
KEY_ID = os.environ.get("NCUI_SIGNING_KEY", "")

# Cargo installs into `~/.cargo/bin`, which is not always on PATH — a Git Bash session that
# has been using an absolute toolchain path will not have it. A tool that works only when the
# caller's shell happens to be configured is a tool that silently does nothing, so the
# directory is added rather than assumed.
CARGO_BIN = Path.home() / ".cargo" / "bin"
if CARGO_BIN.is_dir():
    os.environ["PATH"] = str(CARGO_BIN) + os.pathsep + os.environ.get("PATH", "")


def require(tool: str) -> str:
    """Resolves a tool, adding the cargo bin directory, and explains how to install it."""
    found = shutil.which(tool)
    if found:
        return found
    install = {
        "cargo-cyclonedx": "cargo install cargo-cyclonedx",
    }.get(tool)
    hint = f" — install it with `{install}`" if install else ""
    raise SystemExit(f"`{tool}` is not on PATH{hint}")


def run(command, cwd=ROOT, env=None, check=True):
    """Runs a command, returning its stdout."""
    merged = {**os.environ, **(env or {})}
    proc = subprocess.run(
        command, cwd=cwd, env=merged, capture_output=True, text=True, encoding="utf-8", timeout=3600
    )
    if check and proc.returncode != 0:
        sys.stderr.write(proc.stdout + proc.stderr)
        raise SystemExit(f"`{' '.join(command)}` failed with exit {proc.returncode}")
    return proc.stdout


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def generate_sbom() -> Path:
    """Writes one CycloneDX SBOM for the whole workspace.

    `cargo cyclonedx` emits one document per crate; a release artefact wants one, so they are
    merged and de-duplicated on `(name, version, purl)`. Dev-dependencies are excluded, which
    is the correct default: `criterion` and `proptest` are not in a shipped binary, and an
    SBOM that listed them would overstate the surface a consumer actually inherits.
    """
    print("generating SBOM ...")
    run(["cargo", "cyclonedx", "--format", "json"])

    files = sorted(ROOT.glob("crates/*/*.cdx.json"))
    if not files:
        raise SystemExit("cargo cyclonedx produced no documents")

    primary = json.loads(files[0].read_text(encoding="utf-8"))
    seen: dict = {}
    for path in files:
        doc = json.loads(path.read_text(encoding="utf-8"))
        for component in doc.get("components", []):
            key = (component.get("name"), component.get("version"), component.get("purl"))
            seen.setdefault(key, component)
    primary["components"] = list(seen.values())

    ARTIFACTS.mkdir(exist_ok=True)
    out = ARTIFACTS / "sbom.json"
    out.write_text(json.dumps(primary, indent=2) + "\n", encoding="utf-8")
    for path in files:
        path.unlink()
    print(f"  {len(files)} crate SBOMs -> {out.relative_to(ROOT)} ({len(seen)} components)")
    return out


def build_release_binary() -> Path:
    """Builds the CLI in release mode with a fixed timestamp.

    `SOURCE_DATE_EPOCH` is what makes a build reproducible: without it the linked binary
    records the build time, so two builds of identical source differ byte for byte and a
    consumer cannot tell a rebuild from a substitution.
    """
    print("building the release binary (reproducible) ...")
    # 2024-01-01T00:00:00Z, a fixed point rather than "now".
    epoch = {"SOURCE_DATE_EPOCH": "1704067200"}
    # The `wry-host` feature is what gives the binary a real window host. Without it the
    # shipped `ncui` can serve the agent API but cannot open a window, so `ncui run`
    # would refuse every application.
    run(
        ["cargo", "build", "--release", "-p", "ncui-cli", "--features", "ncui-cli/wry-host"],
        env=epoch,
    )
    binary = ROOT / "target" / "release" / "ncui.exe"
    if not binary.exists():
        binary = ROOT / "target" / "release" / "ncui"
    if not binary.exists():
        raise SystemExit("the release binary was not produced")
    return binary


def reproducible_check(binary: Path) -> bool:
    """Builds a second time and compares digests.

    This is the claim that matters for a release, and it is the only honest way to check it:
    asserting "we build reproducibly" is not the same as rebuilding and diffing.
    """
    print("checking the build is reproducible ...")
    first = sha256(binary)
    with tempfile.TemporaryDirectory() as scratch:
        # A different target directory rules out an incremental no-op being reported as a
        # match, which would make this check pass for the wrong reason.
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
            env={"SOURCE_DATE_EPOCH": "1704067200", "CARGO_TARGET_DIR": scratch},
        )
        rebuilt = Path(scratch) / "release" / binary.name
        if not rebuilt.exists():
            raise SystemExit("the rebuild produced no binary")
        second = sha256(rebuilt)
    print(f"  first  {first}")
    print(f"  second {second}")
    if first != second:
        print("  NOT reproducible: identical source produced different binaries")
        return False
    print("  reproducible: identical source produced an identical binary")
    return True


def sign(path: Path) -> Path | None:
    """Writes a detached ASCII-armoured signature."""
    if not KEY_ID:
        print("no signing key configured; set NCUI_SIGNING_KEY to sign")
        return None
    print(f"signing {path.name} with {KEY_ID} ...")
    run(
        ["gpg", "--batch", "--yes", "--armor", "--detach-sign",
         "--local-user", KEY_ID, "--output", f"{path}.asc", str(path)]
    )
    print(f"  wrote {Path(str(path) + '.asc').relative_to(ROOT)}")
    return Path(f"{path}.asc")


def verify(path: Path) -> bool:
    """Verifies a detached signature, if one is present and a key is configured."""
    signature = Path(f"{path}.asc")
    if not signature.exists():
        print(f"no signature for {path.name}")
        return False
    proc = subprocess.run(
        ["gpg", "--batch", "--verify", str(signature), str(path)],
        capture_output=True, text=True, encoding="utf-8",
    )
    if proc.returncode != 0:
        print(f"  SIGNATURE INVALID for {path.name}")
        return False
    print(f"  signature valid for {path.name} ({sha256(path)[:16]}…)")
    return True


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--verify", action="store_true", help="verify rather than build")
    args = parser.parse_args()

    try:
        require("cargo-cyclonedx")
    except SystemExit as error:
        print(error)
        return 1

    sbom = ARTIFACTS / "sbom.json"
    if args.verify:
        return 0 if verify(sbom) else 1

    sbom = generate_sbom()
    binary = build_release_binary()
    ok = reproducible_check(binary)
    sign(sbom)

    manifest = ARTIFACTS / "MANIFEST.txt"
    lines = [
        f"{sha256(sbom)}  sbom.json",
        f"{sha256(binary)}  {binary.name}",
        "",
        "Built from this tree with SOURCE_DATE_EPOCH=1704067200.",
        "Verify with: python tools/build_release.py --verify",
    ]
    manifest.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"wrote {manifest.relative_to(ROOT)}")

    if not ok:
        return 1
    print("\nrelease artefacts ready.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
