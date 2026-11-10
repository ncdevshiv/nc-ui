# Fuzzing

Two halves of one capability, `process.fuzzing`, with deliberately different requirements.

## The half that runs everywhere

`crates/*/tests/fuzz_properties.rs` hold `proptest` property tests that run in the ordinary
test suite, so a regression in decoder totality or path containment fails a CI build rather
than waiting for someone to remember to fuzz. They use no nightly toolchain and no sanitizer.

| Crate | File | Properties |
| --- | --- | --- |
| `ncui-ipc` | `crates/ncui-ipc/tests/fuzz_properties.rs` | 6 |
| `ncui-core` | `crates/ncui-core/tests/fuzz_properties.rs` | 7 |
| `ncui-security` | `crates/ncui-security/tests/fuzz_properties.rs` | 10 |

They are stated as **universals**, not as lists of known-bad inputs:

- `a_resolved_path_is_always_contained` — *no* input resolves outside the root.
- `decoding_arbitrary_bytes_is_total` — arbitrary bytes produce a `Result`, never a panic.
- `climbing_segments_never_resolve_outside_the_root` — generated traversal shapes, not a
  fixed corpus.

That distinction is not stylistic. The `..`-past-the-root escape in `SafePath` was invisible
to a hand-written list of bad inputs for exactly as long as the crate existed.

## The half that needs a sanitizer

`fuzz_targets/` holds six `libfuzzer` targets: the frame decoder, the message decoder, the
semver parser, path resolution, token handling, and the registry's JSON parser.

```sh
cargo +nightly fuzz run fuzz_path_resolve -- -max_total_time=120
```

This requires nightly **and** an x64 sanitizer runtime.

### Status on this machine

**The libFuzzer targets have not been executed here.** Two independent blockers, both
environmental rather than code:

1. `clang_rt.asan_dynamic_runtime_thunk-x86_64.lib` is absent from the installed MSVC
   toolchain — only the `i386` AddressSanitizer libraries are present — so
   `cargo +nightly fuzz build` fails at link.
2. The GNU route fails earlier: `libfuzzer-sys` compiles a C++ shim with `__builtin_function_start`,
   which the MinGW `g++` on this machine rejects.

Per this project's own rule — *a feature that cannot be verified on this machine stays
`absent` rather than being marked done* — the engine run is recorded as unverified rather than
claimed.

### Why the targets are not therefore unverified code

`crates/ncui-bench/tests/fuzz_targets_compile.rs` compiles every target body against the real
crates, with `fuzz_target!` stubbed, and runs it over generated input. So a target that no
longer type-checks fails an ordinary CI run, and the invariants each target asserts are
actually exercised rather than merely written.

This caught two real defects in the targets themselves, which is the reason it exists:

- `fuzz_message_decode` panicked on **empty input** (`&data[1..]`), so it would have died on
  the first thing a fuzzer tries and never fuzzed anything.
- `fuzz_path_resolve` asserted on `requested.contains("..")`, which fails on an ordinary file
  named `report..final` — asserting something other than what it meant.

## Layout

`fuzz/` is a **separate workspace**. `libfuzzer-sys` needs nightly and must never reach a
shipped build; a member of the main workspace could be depended upon by anything.

`fuzz/target-macro` re-exports `fuzz_target!` under a name the compile-check can stub, so one
source of truth serves both `cargo fuzz run` and the ordinary suite.
