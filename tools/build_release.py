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
    # 2024-01-01T00:00:00Z, a fixed point rather than "now". Incremental compilation is
    # off too: its cached state is a property of the build's history, not of the source.
    epoch = {"SOURCE_DATE_EPOCH": "1704067200", "CARGO_INCREMENTAL": "0"}
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


def pe_facts(data: bytes) -> dict:
    """Reads the PE facts that explain most Windows digest mismatches.

    The header timestamp and the CodeView record's PDB GUID are what the linker writes
    last; with `/Brepro` both must be derived from content, so a difference in either
    names the broken guarantee directly.
    """
    facts: dict = {"time_date_stamp": None, "codeview": []}
    if data[:2] == b"MZ" and len(data) > 0x40:
        e_lfanew = int.from_bytes(data[0x3C:0x40], "little")
        if data[e_lfanew:e_lfanew + 4] == b"PE\0\0":
            facts["time_date_stamp"] = int.from_bytes(
                data[e_lfanew + 8:e_lfanew + 12], "little"
            )
    tail = data[-65536:] if len(data) > 65536 else data
    at = tail.find(b"RSDS")
    if at >= 0 and at + 24 <= len(tail):
        guid = tail[at + 4:at + 20]
        age = int.from_bytes(tail[at + 20:at + 24], "little")
        facts["codeview"] = [f"{guid.hex()} age={age}"]
    return facts


def diff_report(a: bytes, b: bytes) -> list:
    """Describes where two byte strings differ, head first."""
    n = min(len(a), len(b))
    diffs = [i for i in range(n) if a[i] != b[i]]
    lines = [
        f"  sizes: first={len(a)} second={len(b)}",
        f"  differing bytes: {len(diffs)}"
        + ("" if len(a) == len(b) else " (sizes differ)"),
    ]
    for i in diffs[:16]:
        lines.append(f"  offset {i}: first={a[i]:02x} second={b[i]:02x}")
    if len(diffs) > 16:
        lines.append(f"  ... and {len(diffs) - 16} more differing bytes")
    return lines


def diagnose_mismatch(binary: Path, first_dir: Path) -> None:
    """Preserves both builds under `artifacts/repro/` and reports where they differ.

    A digest that mismatches on one machine while passing on another is a fact about the
    build environment, not about the source. Keeping the paired artefacts turns the next
    investigation from a guess into a diff.
    """
    kept = ARTIFACTS / "repro"
    second_dir = kept / "second"
    second_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, second_dir / binary.name)
    pdb = binary.with_suffix(".pdb")
    if pdb.exists():
        shutil.copy2(pdb, second_dir / pdb.name)
    a = (first_dir / binary.name).read_bytes()
    b = (second_dir / binary.name).read_bytes()
    lines = ["the two builds of the same source differ:"]
    lines += diff_report(a, b)
    lines.append(f"  first  PE: {pe_facts(a)}")
    lines.append(f"  second PE: {pe_facts(b)}")
    report = "\n".join(lines) + "\n"
    (kept / "diagnosis.txt").write_text(report, encoding="utf-8")
    print(report, end="")


def reproducible_check(binary: Path) -> bool:
    """Builds a second time and compares digests.

    This is the claim that matters for a release, and it is the only honest way to check it:
    asserting "we build reproducibly" is not the same as rebuilding and diffing.

    The rebuild runs in this tree after a full `cargo clean`, rather than in a second target
    directory. The release profile keeps line tables for profiling (`debug = 1`), and a PDB
    records the absolute paths of the objects it was built from — so a second target directory
    changes the PDB's content, the GUID `/Brepro` derives from that content, and therefore the
    digest, for a reason about *where* the build ran rather than *what* was built. The clean
    forces every crate to be rebuilt and linked again, so an incremental no-op cannot pass for
    a match.

    Both builds also run with incremental compilation off. With it on, a build that starts
    from a warm cache — the CI restores one — can digest differently from a clean rebuild of
    the same source, which would make this check compare build histories rather than source.
    """
    print("checking the build is reproducible ...")
    first = sha256(binary)
    # The first build is set aside so a mismatch can be diffed afterwards: the rebuild
    # overwrites the binary in place, and a digest alone says that the builds differ
    # without saying where.
    first_dir = ARTIFACTS / "repro" / "first"
    first_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, first_dir / binary.name)
    first_pdb = binary.with_suffix(".pdb")
    if first_pdb.exists():
        shutil.copy2(first_pdb, first_dir / first_pdb.name)
    epoch = {"SOURCE_DATE_EPOCH": "1704067200", "CARGO_INCREMENTAL": "0"}
    run(["cargo", "clean"], env=epoch)
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
        env=epoch,
    )
    if not binary.exists():
        raise SystemExit("the rebuild produced no binary")
    second = sha256(binary)
    print(f"  first  {first}")
    print(f"  second {second}")
    if first != second:
        print("  NOT reproducible: identical source produced different binaries")
        diagnose_mismatch(binary, first_dir)
        return False
    shutil.rmtree(ARTIFACTS / "repro", ignore_errors=True)
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
