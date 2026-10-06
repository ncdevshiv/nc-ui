"""Tests the release tooling's own decisions.

The pipeline is shell-adjacent and mostly exercised by running it, so these cover the parts
that are *logic* rather than process invocation: tool resolution, digest computation, and the
ordering constraint that makes the artefact trustworthy.

`python tools/test_build_release.py` — no test framework, so it runs anywhere the pipeline does.
"""

import hashlib
import importlib.util
import json
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("build_release", ROOT / "tools" / "build_release.py")
assert spec and spec.loader
build_release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build_release)

FAILURES: list[str] = []


def check(condition: bool, message: str) -> None:
    if condition:
        print(f"  PASS  {message}")
    else:
        print(f"  FAIL  {message}")
        FAILURES.append(message)


def test_sha256_matches_hashlib() -> None:
    """The digest must be the standard one, or a consumer recomputing it gets a different
    answer and concludes the artefact was tampered with when it was not."""
    with tempfile.TemporaryDirectory() as scratch:
        path = Path(scratch) / "sample.bin"
        payload = b"ncui release artefact"
        path.write_bytes(payload)
        check(
            build_release.sha256(path) == hashlib.sha256(payload).hexdigest(),
            "sha256 matches hashlib",
        )


def test_sha256_distinguishes_different_content() -> None:
    """Two artefacts differing in one byte must not collide, or the MANIFEST is decorative."""
    with tempfile.TemporaryDirectory() as scratch:
        first = Path(scratch) / "a"
        second = Path(scratch) / "b"
        first.write_bytes(b"ncui")
        second.write_bytes(b"ncUj")
        check(
            build_release.sha256(first) != build_release.sha256(second),
            "a one-byte change changes the digest",
        )


def test_cargo_bin_is_added_to_path() -> None:
    """A tool that only works when the caller's shell happens to be configured is a tool that
    silently does nothing, and cargo installs into a directory that is not always on PATH."""
    check(
        str(build_release.CARGO_BIN) in build_release.os.environ["PATH"].split(
            build_release.os.pathsep
        ),
        f"{build_release.CARGO_BIN} is on PATH after import",
    )


def test_require_reports_how_to_install() -> None:
    """A missing tool must say how to install it rather than failing with a bare name."""
    try:
        build_release.require("cargo-cyclonedx-not-a-real-tool")
    except SystemExit as error:
        message = str(error)
        check(
            "cargo install" in message or "not on PATH" in message,
            f"a missing tool explains itself: {message[:60]}",
        )
    else:
        check(False, "a missing tool raises SystemExit")


def test_verify_rejects_a_missing_signature() -> None:
    """An artefact with no signature must not be reported as verified, because a consumer
    asking "is this signed" deserves a no rather than a quiet pass."""
    with tempfile.TemporaryDirectory() as scratch:
        path = Path(scratch) / "uncovered.json"
        path.write_text("{}", encoding="utf-8")
        check(not build_release.verify(path), "an unsigned artefact does not verify")


def test_verify_detects_tampering() -> None:
    """The check must actually fail on modified content, or it is a decoration."""
    with tempfile.TemporaryDirectory() as scratch:
        artefact = Path(scratch) / "sbom.json"
        artefact.write_text(json.dumps({"components": [1, 2, 3]}), encoding="utf-8")
        signature = Path(scratch) / "sbom.json.asc"
        signed = subprocess.run(
            ["gpg", "--batch", "--yes", "--armor", "--detach-sign",
             "--output", str(signature), str(artefact)],
            capture_output=True, text=True,
        )
        if signed.returncode != 0:
            print("  SKIP  tampering test: no signing key available")
            return
        check(build_release.verify(artefact), "an unmodified signed artefact verifies")

        artefact.write_text(json.dumps({"components": []}), encoding="utf-8")
        check(not build_release.verify(artefact), "a modified signed artefact does not verify")


def test_sbom_covers_the_workspace() -> None:
    """The SBOM must describe the crates that are actually in the build, not an empty
    document — a signed empty SBOM is worse than none, because it looks authoritative."""
    sbom = ROOT / "artifacts" / "sbom.json"
    if not sbom.exists():
        print("  SKIP  sbom test: run tools/build_release.py first")
        return
    doc = json.loads(sbom.read_text(encoding="utf-8"))
    names = {component.get("name") for component in doc.get("components", [])}

    check(bool(names), "the SBOM lists components")
    # The dependency the workspace actually ships, and the one ADR-012 calls out as the
    # single external crate reaching a platform primitive.
    for required in ("serde", "serde_json", "getrandom"):
        check(required in names, f"the SBOM lists {required}")


def test_manifest_matches_the_artefacts() -> None:
    """A MANIFEST whose digests do not match its files is worse than no MANIFEST."""
    manifest = ROOT / "artifacts" / "MANIFEST.txt"
    sbom = ROOT / "artifacts" / "sbom.json"
    if not manifest.exists() or not sbom.exists():
        print("  SKIP  manifest test: run tools/build_release.py first")
        return

    recorded = {}
    for line in manifest.read_text(encoding="utf-8").splitlines():
        parts = line.split("  ", 1)
        if len(parts) == 2 and len(parts[0]) == 64:
            recorded[parts[1]] = parts[0]

    check("sbom.json" in recorded, "the MANIFEST records the SBOM")
    if "sbom.json" in recorded:
        check(
            recorded["sbom.json"] == build_release.sha256(sbom),
            "the recorded digest matches the SBOM on disk",
        )


def main() -> int:
    print("release tooling tests")
    test_sha256_matches_hashlib()
    test_sha256_distinguishes_different_content()
    test_cargo_bin_is_added_to_path()
    test_require_reports_how_to_install()
    test_verify_rejects_a_missing_signature()
    test_verify_detects_tampering()
    test_sbom_covers_the_workspace()
    test_manifest_matches_the_artefacts()

    print(f"\n{len(FAILURES)} failure(s)")
    return 1 if FAILURES else 0


if __name__ == "__main__":
    sys.exit(main())