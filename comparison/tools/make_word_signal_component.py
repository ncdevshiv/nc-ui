#!/usr/bin/env python3
"""Generate the Tauri port's component artifact.

The port of the Tauri word-signal application keeps the original's shape: a page triggers a
native command and displays the answer. On NC-UI that command is a component the page did not
write, loaded under `run.components` and invoked through the component host, exactly as the
counter example's greeter is. The artifact is WAT text so it stays reviewable source.

    python comparison/tools/make_word_signal_component.py            # rewrite the artifact
    python comparison/tools/make_word_signal_component.py --check    # fail if it drifted

The output is `comparison/ports/tauri-word-signal/plugins/word_signal.wat`.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent.parent
OUT = REPO / "comparison" / "ports" / "tauri-word-signal" / "plugins" / "word_signal.wat"

MANIFEST_AT = 64
METHOD_AT = 512
ERR_REFUSED_AT = 544
SCRATCH_AT = 1024
SCRATCH_LEN = 2048

METHOD = "return_string"
ERR_REFUSED = json.dumps({"error": "refused"}, separators=(",", ":"))

MANIFEST = json.dumps(
    {
        "abi_version": "1.0",
        "requested": [],
        "imports": [],
        "exports": [{"name": METHOD, "kind": "function"}],
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
  (data (i32.const {METHOD_AT}) {method})
  (data (i32.const {ERR_REFUSED_AT}) {err_refused})

  ;; Packs a pointer and a length into the single i64 the contract returns.
  (func $pack (param $p i32) (param $n i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $n))))

  ;; Byte-for-byte comparison. The caller always compares lengths first, and in an `if` rather
  ;; than `i32.and`, because wasm `and` does not short-circuit: a guarded comparison that
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
    ;; `return_string` answers with the caller's own bytes, which is the whole method: the
    ;; original Tauri command echoes the word it was given, and this is that command.
    (if (call $is (local.get $np) (local.get $nl) (i32.const {METHOD_AT})
                  (i32.const {method_len}))
      (then (return (call $pack (local.get $ip) (local.get $il)))))
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
        METHOD_AT=METHOD_AT,
        method=json.dumps(METHOD),
        method_len=len(METHOD),
        ERR_REFUSED_AT=ERR_REFUSED_AT,
        err_refused=json.dumps(ERR_REFUSED),
        err_refused_len=len(ERR_REFUSED),
        SCRATCH_AT=SCRATCH_AT,
        SCRATCH_LEN=SCRATCH_LEN,
    )
    highest_literal = max(
        MANIFEST_AT + len(MANIFEST),
        ERR_REFUSED_AT + len(ERR_REFUSED),
    )
    assert highest_literal <= SCRATCH_AT, "a data segment overlaps the scratch region"
    if args.check:
        current = OUT.read_text(encoding="utf-8") if OUT.is_file() else ""
        if current != wat:
            print(
                f"{OUT.relative_to(REPO)} differs from the generated artifact; run "
                "`python comparison/tools/make_word_signal_component.py`",
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
