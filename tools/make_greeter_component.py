#!/usr/bin/env python3
"""Generate the counter example's component artifact.

The artifact is WAT text: the engine assembles it, so what ships is reviewable source rather
than a binary blob whose provenance nobody can check. Every length the component compares
against is computed here from the string it belongs to, because a hand-counted offset that is
one byte out produces a component that reads a truncated document and fails at run time.

    python tools/make_greeter_component.py            # rewrite the artifact
    python tools/make_greeter_component.py --check    # fail if the file in the tree differs

The output is `examples/counter/plugins/greeter.wat`, rewritten in place unless `--check` is
given, in which case nothing is written and a difference exits non-zero.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OUT = REPO / "examples" / "counter" / "plugins" / "greeter.wat"

# The layout. Every offset is named, and each literal's length is measured rather than typed.
MANIFEST_AT = 64
CAP_AT = 512  # "filesystem.read"
SERVICE_AT = 544  # "fs.read_text"
REQUEST_AT = 576  # {"path":"notes.txt"}
METHOD_ECHO_AT = 640
METHOD_GREET_AT = 672
ERR_REFUSED_AT = 704
SCRATCH_AT = 1024
SCRATCH_LEN = 2048
HOST_OUT_AT = 4096
HOST_OUT_LEN = 2048

CAPABILITY = "filesystem.read"
SERVICE = "fs.read_text"
REQUEST = json.dumps({"path": "notes.txt"}, separators=(",", ":"))
METHOD_ECHO = "echo"
METHOD_GREET = "greet"
ERR_REFUSED = json.dumps({"error": "refused"}, separators=(",", ":"))

# The manifest the component carries. It declares the one service it will ask for and the two
# methods it offers; the host enforces both lists, so a component that omits either is refused
# rather than tolerated.
MANIFEST = json.dumps(
    {
        "abi_version": "1.0",
        "requested": [CAPABILITY],
        "imports": [
            {
                "module": "ncui",
                "name": SERVICE,
                "kind": "capability_call",
                "requires": CAPABILITY,
            }
        ],
        "exports": [
            {"name": METHOD_ECHO, "kind": "function"},
            {"name": METHOD_GREET, "kind": "function"},
        ],
    },
    separators=(",", ":"),
)

TEMPLATE = """(module
  ;; The two imports the contract allows, and nothing else: the host defines exactly these, so
  ;; any other import in this module would fail to link rather than reach something in the host.
  (import "ncui" "capability_check" (func $check (param i32 i32) (result i32)))
  (import "ncui" "host_call" (func $host (param i32 i32 i32 i32 i32 i32) (result i32)))

  ;; One page of memory, four allowed. The host caps growth far below the module's own maximum,
  ;; so growing past the cap is the host's decision and not this module's.
  (memory (export "memory") 1 4)

  (data (i32.const {MANIFEST_AT}) {manifest})
  (data (i32.const {CAP_AT}) {capability})
  (data (i32.const {SERVICE_AT}) {service})
  (data (i32.const {REQUEST_AT}) {request})
  (data (i32.const {METHOD_ECHO_AT}) {method_echo})
  (data (i32.const {METHOD_GREET_AT}) {method_greet})
  (data (i32.const {ERR_REFUSED_AT}) {err_refused})

  ;; Packs a pointer and a length into the single i64 the contract returns.
  (func $pack (param $p i32) (param $n i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $n))))

  ;; Byte-for-byte comparison. The caller always compares lengths first, and in an `if` rather
  ;; than an `i32.and`, because wasm `and` does not short-circuit: a guarded comparison that
  ;; still read the bytes would trap on a bad pointer instead of falling through.
  (func $streq (param $a i32) (param $b i32) (param $n i32) (result i32)
    (local $i i32)
    (local $bad i32)
    (loop $cont
      (local.set $bad (i32.or (local.get $bad)
        (i32.ne (i32.load8_u (i32.add (local.get $a) (local.get $i)))
                (i32.load8_u (i32.add (local.get $b) (local.get $i))))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $cont (i32.lt_u (local.get $i) (local.get $n))))
    (i32.eqz (local.get $bad)))

  (func $is (param $p i32) (param $n i32) (param $lit i32) (param $lit_len i32) (result i32)
    (if (result i32)
      (i32.ne (local.get $n) (local.get $lit_len))
      (then (i32.const 0))
      (else (call $streq (local.get $p) (local.get $lit) (local.get $lit_len)))))

  ;; The calling-convention revision this artifact was built against.
  (func (export "ncui_abi_version") (result i32) i32.const 1)

  (func (export "ncui_manifest") (result i64)
    (call $pack (i32.const {MANIFEST_AT}) (i32.const {manifest_len})))

  ;; Where the host may write the method name and the request document.
  (func (export "ncui_scratch") (result i64)
    (call $pack (i32.const {SCRATCH_AT}) (i32.const {SCRATCH_LEN})))

  (func (export "ncui_invoke") (param $np i32) (param $nl i32) (param $ip i32) (param $il i32)
        (result i64)
    (local $rc i32)
    ;; `echo` answers with the caller's own bytes, which is the whole method.
    (if (call $is (local.get $np) (local.get $nl) (i32.const {METHOD_ECHO_AT})
                  (i32.const {method_echo_len}))
      (then (return (call $pack (local.get $ip) (local.get $il)))))
    ;; `greet` asks the host to read the application's notes file. It cannot read it itself:
    ;; the host serves the call only because this component declared the service and the
    ;; negotiation granted the capability behind it.
    (if (call $is (local.get $np) (local.get $nl) (i32.const {METHOD_GREET_AT})
                  (i32.const {method_greet_len}))
      (then
        (local.set $rc (call $host (i32.const {SERVICE_AT}) (i32.const {service_len})
                                   (i32.const {REQUEST_AT}) (i32.const {request_len})
                                   (i32.const {HOST_OUT_AT}) (i32.const {HOST_OUT_LEN})))
        (if (i32.lt_s (local.get $rc) (i32.const 0))
          (then (return (call $pack (i32.const {ERR_REFUSED_AT}) (i32.const {err_refused_len})))))
        (return (call $pack (i32.const {HOST_OUT_AT}) (local.get $rc)))))
    ;; Anything else is a method this component never declared.
    (return (call $pack (i32.const {ERR_REFUSED_AT}) (i32.const {err_refused_len}))))
)
"""


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="do not write; exit 1 if the artifact in the tree differs from the generated one",
    )
    args = parser.parse_args()

    wat = TEMPLATE.format(
        MANIFEST_AT=MANIFEST_AT,
        manifest=json.dumps(MANIFEST),
        manifest_len=len(MANIFEST),
        CAP_AT=CAP_AT,
        capability=json.dumps(CAPABILITY),
        SERVICE_AT=SERVICE_AT,
        service=json.dumps(SERVICE),
        REQUEST_AT=REQUEST_AT,
        request=json.dumps(REQUEST),
        METHOD_ECHO_AT=METHOD_ECHO_AT,
        method_echo=json.dumps(METHOD_ECHO),
        method_echo_len=len(METHOD_ECHO),
        METHOD_GREET_AT=METHOD_GREET_AT,
        method_greet=json.dumps(METHOD_GREET),
        method_greet_len=len(METHOD_GREET),
        ERR_REFUSED_AT=ERR_REFUSED_AT,
        err_refused=json.dumps(ERR_REFUSED),
        err_refused_len=len(ERR_REFUSED),
        service_len=len(SERVICE),
        request_len=len(REQUEST),
        SCRATCH_AT=SCRATCH_AT,
        SCRATCH_LEN=SCRATCH_LEN,
        HOST_OUT_AT=HOST_OUT_AT,
        HOST_OUT_LEN=HOST_OUT_LEN,
    )
    # The data segments must not overlap the region the host writes into.
    highest_literal = max(
        MANIFEST_AT + len(MANIFEST),
        ERR_REFUSED_AT + len(ERR_REFUSED),
    )
    assert highest_literal <= SCRATCH_AT, "a data segment overlaps the scratch region"
    assert HOST_OUT_AT + HOST_OUT_LEN <= 65536, "the host's output region leaves the page"
    if args.check:
        current = OUT.read_text(encoding="utf-8") if OUT.is_file() else ""
        if current != wat:
            print(
                f"{OUT.relative_to(REPO)} differs from the generated artifact; run "
                "`python tools/make_greeter_component.py`",
                file=sys.stderr,
            )
            raise SystemExit(1)
        print(f"{OUT.relative_to(REPO)} matches the generator ({len(wat)} bytes of WAT)")
        return
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(wat, encoding="utf-8", newline="\n")
    print(f"wrote {OUT.relative_to(REPO)} ({len(wat)} bytes of WAT)")


if __name__ == "__main__":
    main()
