# Known gaps and open decisions

Everything here is a deliberate, recorded decision — not a task someone forgot. Each entry
says what is not done, why, what was measured, and what would change it.

## The benchmark regression gate reports; it does not fail

**This is the most important entry in this file.**

`crates/ncui-bench/benches/latency.rs` can compare a run against a committed baseline
(`benches/baseline/<os>/latency.json`), and it does. What it does **not** do is exit non-zero,
by default, on a regression.

### Why, with the measurements

The gate compares the **minimum of nine independent repeats** per scenario, not a single
run and not the median. That statistic was chosen because machine interference can only
ever make a run look *slower*, so the minimum is the one that tracks the code.

The choice is the reason this gate is stable at all. Measuring worst/best spread across
independent runs of **unchanged code** on the machine the baseline was recorded on:

The measured spread is **generated, not transcribed**. Run:

```
cargo bench -p ncui-bench --bench latency
```

The harness prints the table on every run and records it under `noise` in
`benches/baseline/windows/latency.json`. An earlier revision of this document carried the
table by hand in three places; the copies drifted apart, and the worst figure used to
justify the tolerance turned out not to reproduce when measured.

Measured on Windows x86_64, nine repeats each. **Regenerate rather than copy:**

```
cargo bench -p ncui-bench --bench latency
```

The harness prints this table on every run and records it under `noise` in
`benches/baseline/windows/latency.json`. An earlier revision of this document carried the
table by hand; it drifted between three copies, and the figure used to justify the
tolerance turned out not to reproduce.

The median is what a *user* experiences and is what gets reported. The minimum is what a
*gate* should compare, and the difference between the two columns is the whole reason an
earlier revision of this gate — which compared median values — fired on 2 of 7 runs of
identical code, and 4 of 7 in an independent audit.

> An earlier revision of this document quoted 7.5x and 4.3x as the minimum-of-9 spread.
> Those numbers were the **median** spread, misattributed to the minimum, which overstated
> how noisy the compared statistic is. The table above is the corrected measurement. The
> design decision is unchanged, and is now better supported than the incorrect version
> claimed: the median is far too noisy to gate on, and the minimum is not.

Even so, a `security.constant_time_eq_mismatched_length` outlier of 2.0x appeared in
sampling, so the tolerance is set from that measurement rather than at the observed worst case. A gate set
exactly at the noise ceiling has no margin on a busier day.

### What the gate does do

* **It refuses to compare across platforms and exits non-zero saying so.** A baseline
  recorded on Windows compared against a Linux run is a comparison of two different
  machines. Each platform records its own baseline with `--write-baseline` on its first run.
* **It refuses to compare scenarios whose p99 sits at the clock's resolution**
  (`REPORTED_ONLY_FLOOR_NS`). A p99 of 100 ns on a ~100 ns clock is a statement about the
  timer.
* **It always prints what moved**, so a regression is visible to a human even when it does
  not fail the build.
* **`--enforce` exists** and fails on a difference. Given the measurements above it is
  close to defensible on this machine, but it is not enabled by default, because a
  dedicated runner is quieter than a workstation and a gate tuned to one is not tuned to
  the other. The choice is a measurement, not an opinion.

### What would change this

Record baselines on dedicated runners — no competing load, fixed frequency, pinned to the
machine — and re-measure. If the minimum-of-9 spread stays under about 1.5x over a long
sampling run, add `--enforce` to the benchmark step in `.github/workflows/ci.yml`. The
number to change is `ALLOWED_REGRESSION`; the switch is that flag.

## The virtualisation work bound does not catch every shape of regression

Two of the four mutation shapes run against `ncui-state`'s window suite are **not** caught,
and the gap is deliberate rather than discovered:

* **An early-returning O(n) walk** over the collection. The timing bound is on the ratio of
  collection sizes; an O(n) walk and a correct O(m) walk both scale with *something* large,
  and at a fixed `m` the two are separated only if the walk is charged to the counter — which
  a mutation is free to skip.
* **A fixed constant-factor slowdown** — say, 500 redundant iterations per measured item.
  Every bound here is a ratio, and a ratio is blind to a constant.

What *is* caught: an O(n) walk that charges the counter honestly, and an overscan scaled by
the collection size. Both were run, not assumed.

The fix for the rest is instrumentation rather than thresholding — a cost accounted at the
operation's boundary rather than inside it. That is real work and it is not done, so the
bound is documented as partial rather than presented as airtight.

## The feature registry can be made harder to evade, not unimpeachable

The validator (`tools/ncui-tools/src/registry.rs`) checks that:

* every required evidence slot is verified, and **the required set is owned by the
  validator** (`required_for()`), not authored by the registry;
* every verified citation names a repository file that exists and resolves **inside** the
  repository;
* **a citation that names no file is rejected** — so "we wrote some tests, trust us" cannot
  satisfy `unit_tests`;
* feature ids are well-formed lowercase dotted identifiers, so no rule is dodged by
  changing one letter's case;
* named crates exist in a workspace;
* a completed feature is verified on at least one platform.

Ten documented evasions were attempted against this validator — path traversal, absolute
escapes, globs, flags, directories, multi-path burying, case-dodged ids, invented crates —
and all ten are rejected.

**What it still cannot do.** `verified: true` is, at the end of the chain, a claim made by
the party being audited. The validator proves a named file exists; it cannot prove that
file demonstrates what the citation says. A determined author could cite a file that exists
but is irrelevant.

### Most citations are self-certifying

Every one of the 27 completed features cites, for at least one slot, a file inside its own
crate. `runtime.virtualization` cites `crates/ncui-state/src/virtualize.rs` for all nine
slots, including `security_review` and `api_review`.

That is a real limit and it is worth stating plainly: the validator proves a file **exists**,
not that the file contains what the citation says about it. A `security_review` that cites
the file it reviewed is circular in the same way a mark scheme that marks its own homework is
circular — the check is real, but it is not independent.

Closing it means either deriving the registry from evidence rather than validating
self-asserted claims (below), or adding a second evidence class — an artefact produced by a
different party — so `security_review` and `api_review` cannot cite the thing they are
reviewing. Neither is built.

The fix that removes the whole class rather than narrowing it is to **generate the registry
from evidence** — have the gate discover each feature's tests and benchmarks and
derive `verified` as an output rather than accepting it as an input. That is the right
long-term design and it has not been built. It is recorded here so the current validator is
not mistaken for something stronger than it is.

## Nothing has been verified on Linux or macOS

Every feature is recorded `unverified` on both platforms. The CI matrix defines the jobs; the
results have not been observed. The registry prints these gaps on every run so they cannot
be forgotten.

## The scheduler declines one requested behaviour

See "Rejected alternatives" in `docs/adr/ADR-005-scheduler.md`. Ageing that could promote
background work above interactive work would break the no-starvation invariant, so the
invariant was kept and the promotion ceiling lowered instead.

## `require_destructive` takes a plain `bool` consent signal

A caller that writes `true` unconditionally has opted out of the gate. The correct design is
an unforgeable consent token minted by the runtime at the point of user confirmation; that
belongs in the runtime's UI surface, which is not implemented. Until then the gate fails
closed: `AgentSession::require_consent` treats an absent consent parameter as a refusal.

## `SafePath` does not follow symlinks

Containment is lexical. A symlink planted inside a root can still point outside it.
`canonicalize` would follow symlinks, making the answer a time-of-check/time-of-use race,
and errors on paths that do not exist yet — turning "create a new file" into a denial. The
limitation is documented on `SafePath`; the mitigation is forbidding symlink creation inside a
root, or re-checking containment after canonicalising.

## The registry validator checks existence, not truth — and that was worse than documented

The entry above says the validator "cannot prove that file demonstrates what the citation
says", and frames the limit as "a determined author could cite a file that exists but is
irrelevant". The measured limit is stronger than that.

Every citation was re-checked against the file it names. The files existed in all cases. The
*claims inside them did not hold*:

- Three `unit_tests` slots quoted a crate-wide total against a single file —
  `core.error_model` claimed 35 tests in `error.rs`, which has 5;
  `observability.metrics` claimed 142 in `metrics.rs`, which has 52.
- `ipc.binary_framing.failure_tests` cited "a deterministic PRNG stream of malformed frames"
  in `frame.rs`. No such test exists. The property is real — it is now asserted by
  `decoding_is_deterministic` — but the cited evidence was fictional.
- `runtime.virtualization.documentation` cited `virtualize.rs` for "why the arithmetic is
  **O(log m)**". The file says the opposite three times, explicitly retracting an earlier
  version's claim. The registry documented the retracted statement.
- `core.resource_limits.tracing` cited `logring.rs` for a limit-saturation emission. That
  1,073-line file contains no `Limits` field and no such emission.
- Five slots cited a module that emits no metric at all as evidence for a `metrics` slot.

All thirteen are corrected. The lesson is the one the validator's own documentation already
implied but understated: **path traversal is the narrow failure, and a claim that is merely
about the wrong thing is the wide one.** Nothing about the validator needed to change to catch
the narrow case, and nothing about it catches the wide case.

The three newly registered capabilities (`runtime.mcp_adapter`, `process.fuzzing`,
`process.soak_and_leaks`) are the first entries whose `security_review` and `api_review`
slots cite a file other than the one under review.

## The libFuzzer engine runs on Linux, not on the Windows host

`cargo fuzz run` cannot execute on this Windows machine: the x64 AddressSanitizer runtime is
absent from the MSVC toolchain (only the `i386` libraries are present), and the MinGW `g++`
rejects libFuzzer's C++ shim. It runs perfectly well under WSL2, where Rust ships its own
ASan runtime, so that is where the targets were executed:

| Target | Executed units | Coverage |
| --- | --- | --- |
| `fuzz_frame_decode` | 953,542 | 220 |
| `fuzz_message_decode` | 1,402,499 | 581 |
| `fuzz_semver_parse` | 1,895,778 | 280 |
| `fuzz_path_resolve` | 779,456 | 196 |
| `fuzz_token_hex` | 9,854,158 | 42 |
| `fuzz_registry_parse` | 566,023 | 851 |

Thirteen and a half million executions, no crashes. `tools/run_fuzzers.sh` runs the set with
a bounded duration per target and prints the unit count, so the number above is regenerated
rather than transcribed.

The residual limitation is the *host*, not the fuzzing: these results are Linux results. The
Windows binary of each target is built by CI's `fuzz` job on ubuntu and has not been executed
on Windows, because this machine cannot execute it.

What *is* verified, and runs in CI: 23 property tests across three crates, and
`crates/ncui-bench/tests/fuzz_targets_compile.rs`, which compiles every target body against
the real crates with `fuzz_target!` replaced by a stand-in and runs it over generated input.
That check earned its place immediately — it found two defects in the targets themselves, one
of which would have stopped a target from fuzzing at all:

- `fuzz_message_decode` indexed `&data[1..]`, panicking on **empty input** — the first thing a
  fuzzer tries.
- `fuzz_path_resolve` asserted on `requested.contains("..")`, which fails on an ordinary file
  named `report..final`. It now tests for an actual `ParentDir` component.

What was originally true here — that the engine had never been run — was a consequence of the
host rather than of the work, and running the same targets under WSL2 resolved it.

## Four capabilities recorded as blocked were not blocked

The first assessment of what this machine could build concluded that six capabilities needed
platforms it lacked. Checking rather than inheriting that conclusion changed the picture: five
of the six were buildable, and one was blocked on something a single command would generate.

| Recorded as blocked on | What was actually there |
| --- | --- |
| "Linux and macOS runners" | WSL2 with Ubuntu 26.04; the full gate runs on it (909 tests, 10/10 gates) |
| "a signing key" | `gpg` is installed; a key is generatable and the pipeline signs |
| "a WASM component-model toolchain" | The wire format, negotiation and capability declarations need none — only execution does, and execution is separately recorded as absent |
| "a platform Webview library" | The WebView2 runtime is installed and a window can be created *and shown* in this session; what was missing was the host policy, which is pure logic |

The lesson generalises past this repository: **a capability recorded as blocked is a claim
about the environment, not about the code, and it goes stale.** The cost of trusting it is that
the registry says `absent` for something that is done.

## `gpu.islands` is the one that is genuinely blocked

The remaining capability needs a renderer, not a device. This machine has an RTX 3070 and a
window station that can show a window — more than a CI runner offers — but ADR-002 requires the
full input contract (mouse, keyboard, focus, scrolling, clipboard, selection, resize, high-DPI,
drag and drop, IME, accessibility) and calls a bare canvas "not acceptable". The half that can
exist without a renderer has been built first, as ADR-002's own sequencing asks: `ncui-a11y`
holds the semantic tree, focus order, keyboard routing and `IslandDescriptor`.

Marking it complete on the strength of that would be the same overclaim this registry exists to
prevent.

## macOS has no host here

`cross_platform.verification` was one entry covering two platforms. Linux ran; macOS did not.
It is now two entries, because a single entry would have let the completed half hide the
unrun one — which is what the registry's `unverified` platform states exist to prevent.
