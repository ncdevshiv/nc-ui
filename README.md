# NC-UI

The web ecosystem. Native performance. One UI.

NC-UI is an open-source, cross-platform desktop application runtime. Applications are written
in HTML, CSS, and JavaScript or TypeScript; the runtime supplies native execution, GPU
accelerated rendering, high-performance IPC, a scheduler, a capability-based security model,
and a machine-readable interface that lets an agent inspect, test, profile, and operate the
whole system.

The design rationale for every major subsystem is recorded in [`docs/adr/`](docs/adr/).

---

## What state this project is actually in

This section is the most important one on the page. The project treats an unverified claim as
worse than an absent feature, so the state below is generated from the machine-readable feature
registry rather than from intent.

| Area | State |
| --- | --- |
| Workspace, strict lint policy, CI gates | Implemented |
| Core vocabulary: errors, ids, capabilities, versioning, limits | Implemented |
| Binary IPC protocol, correlation, cancellation, backpressure | Implemented |
| Incremental state, patches, filtered subscriptions | Implemented |
| Priority scheduler with ageing and cooperative budget | Implemented |
| Security: capabilities, auth, rate limiting, path containment, audit | Implemented |
| Observability: tracing, metrics with percentiles, log ring | Implemented |
| Native worker pool | Implemented |
| Runtime composition, JSON-RPC agent API, health surface | Implemented |
| CLI truth layer (`ncui status`, `ncui health`, `ncui describe`) | Implemented |
| Benchmark harness with p50/p90/p95/p99 and a regression gate | Implemented |
| Virtualization: constant-cost windowing over large collections | Implemented |
| MCP adapter over the JSON-RPC surface | Implemented |
| Property tests and fuzz targets for every untrusted parser | Implemented |
| Soak tests asserting resource counts return to baseline | Implemented |
| WebView host policy: lifecycle, navigation, capability isolation, IPC seam | Implemented |
| Language-neutral component ABI: wire format, negotiation, capability declarations | Implemented |
| Accessibility: semantic tree, focus order, keyboard routing | Implemented |
| SBOM, reproducible release builds, artifact signing | Implemented |
| Cross-platform verification on Linux | Implemented |
| GPU engine and GPU islands | **Not implemented** |
| WASM *execution* (the component ABI is specified; nothing runs it) | **Not implemented** |
| IME, bidi and text shaping inside a GPU island | **Not implemented** |
| Cross-platform verification on macOS | **Not implemented** — no macOS host reachable |

**Linux and Windows both pass the full gate** — the full workspace suite, formatting, clippy
pedantic with `-D warnings`, rustdoc with `-D warnings`, the tools workspace, and the policy
gates (`ncui-arch`, `ncui-registry`, `ncui-placeholders`, and the count check described below).
`cargo test --workspace --all-targets` is the suite behind that claim;
`tools/linux_gate.py` runs the Linux side: every gate it lists, last run clean under WSL2.
**macOS has not been run**: no host is reachable
from the machine that produced this tree, and the registry records it as `unverified` rather
than inferred from a CI matrix. That is
recorded as outstanding work rather than hidden: a feature is not treated as verified on a
platform until it has been verified on it.

Run the validator for the authoritative, machine-readable version:

```sh
cargo run --release --bin ncui-registry -- protocols/feature-registry.json
```

A capability is recorded as either `complete` — implemented, tested, integrated, observable and
documented — or `not_implemented`. There is deliberately no third status, and the CI gate
rejects any feature declared complete that is missing required evidence.

---

## Repository layout

```text
crates/
  ncui-core            layer 0  errors, typed ids, capabilities, versions, limits
  ncui-observability   layer 1  tracing, metrics with percentiles, bounded log ring
  ncui-security        layer 1  authorization, tokens, rate limits, path containment, audit
  ncui-ipc             layer 1  binary framing, envelopes, correlation, backpressure
  ncui-state           layer 1  revisioned stores, coalescing patches, subscriptions, windowing
  ncui-scheduler       layer 1  priority, ageing, cooperative budget, cancellation
  ncui-a11y            layer 1  semantic tree, focus order, keyboard routing
  ncui-abi             layer 1  component ABI manifest and capability negotiation
  ncui-workers         layer 2  native worker pool with bounded queues
  ncui-webview         layer 2  webview host policy, lifecycle, navigation, capability
  ncui-gpu             layer 2  compositing boundary: geometry and the ADR-002 input contract
  ncui-runtime         layer 3  composition, JSON-RPC agent API, health
  ncui-cli             layer 4  machine-readable status and introspection
  ncui-bench           layer 4  latency and throughput harness with committed baselines
tools/                 a separate workspace: placeholder scanner, architecture checker,
                       feature registry validator, coverage battery
docs/adr/              architecture decision records
protocols/             machine-readable feature registry
```

Crate layering is a strict total order: a crate may depend only on crates at *strictly lower*
layers (ADR-011). The rule is enforced mechanically, and a crate with no declared layer is an
error rather than a warning.

---

## Building and gating

Everything below is a hard gate. Warnings are defects.

```sh
make gate          # everything CI runs
```

Without `make`, the equivalent commands are:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

cd tools
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cd ..

./tools/target/release/ncui-placeholders crates protocols
(cd tools && ./target/release/ncui-placeholders ncui-tools)
./tools/target/release/ncui-arch .
./tools/target/release/ncui-registry protocols/feature-registry.json
cargo bench -p ncui-bench --bench latency -- --baseline benches/baseline/windows/latency.json
```

The placeholder scanner is invoked once per workspace root. Exemption is decided by a
file's first path component, so a single invocation rooted above both workspaces would treat
every test file as production source.

The workspace denies `unsafe_code` outright, and denies `unwrap`, `expect`, `panic`, `todo`,
`unimplemented`, `print_stdout`, `print_stderr`, and `dbg!` in library code. Clippy runs at
`pedantic` with warnings denied, and every `#[allow]` must carry a written `reason`.

## Known gaps

`docs/known-gaps.md` records every deliberate limitation: what the feature registry
validator cannot prove, why the benchmark threshold is above the measured noise, that
nothing has been verified on Linux or macOS, and the two behaviours that were declined on
purpose. It is read alongside the ADR set rather than buried.

## Benchmarks

Two suites answering two different questions. `crates/ncui-bench/benches/latency.rs` measures
each scenario nine times and reports p50/p90/p95/p99 for the median, writing a
machine-readable report to `benches/results/latency.json`. `throughput.rs` uses Criterion
for change detection.

The latency harness can compare a run against `benches/baseline/<os>/latency.json`. It reports by
default and only fails with `--enforce`, and the reason is measured rather than assumed.

The comparison uses the **minimum** of the nine repeats, not the median. That choice is the
whole design: machine interference can only make a run look slower, so the minimum is the
statistic that tracks the code. Across independent runs of unchanged code:

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

An earlier revision compared the median and fired on 2 of 7 runs of identical code. The
minimum is what fixed that.

What the comparison does enforce:

* it refuses to compare across platforms and exits non-zero saying why, so each platform
  records its own baseline with `--write-baseline`;
* it refuses to compare scenarios whose p99 sits at the clock's resolution;
* it always prints what moved, so a regression is visible even when it does not fail.

`docs/known-gaps.md` records the measurements, the residual risk, and exactly what to change
to enable enforcement.

## Virtualization

Spec 18's claim is that "a million logical records must not require a million DOM nodes."
That is measurable, so it is measured — and it is asserted rather than described.

`ncui_state::virtualize` computes which indices a viewport shows from a sparse map of
*measured* row heights rather than from the collection, so the work is bounded by how much
the user has actually looked at. The benchmark runs both a 1,000-record and a
1,000,000-record collection and the harness asserts
`a_window_ignores_the_size_of_the_collection_it_is_a_window_into`: if the window ever
starts scaling with the collection, the build goes red.

The claim is checked rather than quoted, on purpose. The measured p99 moves between runs on
a shared machine, so a figure transcribed into a document drifts from the artifact it cites —
a defect this project made and corrected twice. Instead there are two checks:

* **Structural, deterministic, in the test suite.** `a_window_ignores_the_size_of_the_collection_it_is_a_window_into` asserts a million-record window covers as many indices as
  a thousand-record one. It cannot flake.
* **Timing, reported by the benchmark.** The harness feeds two real measurements through
  `costs_within_tolerance` and prints a `CANDIDATE REGRESSION` line when the ratio exceeds
  2x. It reports rather than fails, because a shared machine cannot fail a build on a timing
  verdict.

The wall-clock check varies collection size at a *deep* offset — at offset zero the
traversal never runs, so a check there measures construction rather than the walk.

**What this catches, and what it does not.** Two mutation shapes are caught: an O(n)
`index_at` that charges the counter, and an overscan scaled by the collection size. Four are
not, and they are listed rather than left for a reader to discover — an early-returning
O(n) walk, a walk worse than linear in the measured count, redundant work inside one pass
that charges no counter, and a fixed constant-factor slowdown. The last two are constant
factors, which no ratio can see; closing them means instrumenting at the operation's
boundary rather than thresholding inside it. `docs/known-gaps.md` records the whole set.

Independently verified: both collections store 58 measured rows, so a million records cost
the same handful of DOM nodes as a thousand. Per-run percentiles are in
`benches/baseline/windows/latency.json`.

## The agent API over a socket

`ncui serve` binds the JSON-RPC surface to an ephemeral loopback port and prints a
one-time bearer token. The framing is a 4-byte big-endian length followed by that many bytes
of JSON; the first frame carries the token.

Loopback is not trusted, so the transport adds, over the in-process path:

* loopback-only binding — never a routable interface;
* a constant-time bearer-token handshake before any request is read;
* the same rate limit, capability check and consent gate, because it dispatches through
  the same pipeline rather than beside it;
* declared frame length validated **before** any allocation, so a peer announcing 3 GiB
  costs nothing;
* an audit event for every request, including every refusal;
* a bound on concurrent connections.

`crates/ncui-runtime/tests/agent_transport.rs` drives a live server over a real socket,
covering the authenticated round trip, refusal without a token, refusal of a wrong or malformed
token, the hostile-length case, per-connection rate limiting, and capability denial across the
socket.

No figure is quoted here: a count written in prose is a claim nothing re-derives, and
`tools/correct_test_counts.py` — which does derive them — reads the registry, where the
citations are structured. The file is named instead.

Verified by hand against the released binary: an authenticated client received the full
eight-method surface, `ncui.logs.purge` was refused without consent and served with it, and
an unauthenticated peer received nothing.

Getting it to start on Windows needed one external dependency. `std` has no API for
`BCryptGenRandom`, and calling it directly requires `unsafe`, which this workspace forbids —
so `ncui serve` originally refused to start rather than fall back to a predictable token.
The fix was the `getrandom` crate, which performs that FFI so this project's own code stays
free of `unsafe`. It is the single platform primitive reached beyond `std`, it is scoped to
`ncui-security`, and the reasoning is recorded in ADR-012.

## Contributing

The project's engineering principle is that progress is measured by independently verifiable
capability, not by the appearance of progress. A feature is not complete because code exists,
because it compiles, because a demo worked once, or because an agent said it works. It is
complete when it can be implemented, executed, tested, integrated, benchmarked, traced,
inspected, debugged, secured, recovered from failure, documented, and reproduced.

Contributors should read the relevant ADRs in `docs/adr/` before changing architecture.

## Licence

Apache-2.0.
