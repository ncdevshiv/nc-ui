# Completion audit

**Date:** 2026-11-24. **Machine:** Windows 11 x86_64, `rustc 1.98.1`, matching
`rust-toolchain.toml`; Linux verification under WSL2 Ubuntu 26.04, same toolchain.
**Question:** does the work satisfy the objective it was given? Each requirement below is
answered with the evidence that settles it, and where the evidence does not support the
requirement, that is said rather than rounded off.

The objective's requirements are in the left column, each reduced to the claim that can be
checked. "Done" means a reader can re-run the cited command and see the cited result;
nothing here rests on a description of a run.

---

## 1. The framework

| Requirement | Standing | Evidence |
| --- | --- | --- |
| A new framework for near-native desktop applications | Done | `ncui` runs an application: manifest, asset server, window, bridge. `examples/counter` runs end to end; `comparison/ports/*` are two more applications running on it. |
| GPU-driven by default | Done, with a precise meaning | The standard release build carries the GPU renderer: `ncui-cli`'s `wry-host-gpu` enables `gpu-islands`, so `ncui run` serves `/__ncui/island` rather than a blank rectangle. The minimal `wry-host` profile deliberately omits it. Verified over real HTTP from the release binary: `200`, 1,228,800 bytes for a 640×480 frame, the row colour at the row's pixels, transparent elsewhere. The page's own rendering is the webview's, which is itself GPU-composited. What "GPU-driven" does **not** mean: the runtime does not draw the whole UI itself. |
| Pure parallel | Done | `ncui-scheduler` (cooperative budgets, no starvation) and `ncui-workers` (bounded queues, cancellation, eviction) are complete with tests that assert the bounds rather than the absence of a crash. |
| Best frontend | Done, as a property not a ranking | A page is an ordinary web page: HTML, CSS and JavaScript, any framework, served over the app's own origin. The bridge is ordinary JavaScript (`packages/ncui-client`, with its own suite). "Best" is not a claim this document can prove; what is claimed is that nothing about the frontend is constrained by the runtime, and that a GPU region's content reaches assistive technology through a description the page mirrors into the DOM. |
| Suitable and supportable | Done | `README.md`, `docs/known-gaps.md`, `docs/remaining-work.md`, the ADR set, `principle.md`, a 77-entry feature registry, and a gate battery that runs in CI on three platforms. |
| Better in every respect than Electron or Tauri | **Not established** | See §3. NC-UI wins against Electron on cold start and size, beats Tauri 2 on IPC, is level with Tauri 2 on start, and all three measure 0.00 % CPU at idle; it loses to Electron on IPC and memory and to Tauri 2 on standard-build size. The objective asked for proof of superiority in every respect; the measurement does not support that claim and it is not made. |

## 2. Every missing capability

The registry tracks 77 capabilities. **`ncui status --registry protocols/feature-registry.json`
reports 76 complete and 1 not implemented.** The one absent entry is
`cross_platform.verification.macos`: the full gate suite run on macOS. Nothing about the gate
is missing; a macOS host is not reachable from the machine that produced the registry, and a
platform claim without a run is exactly what this registry exists to prevent.

Native menus are delivered on Windows and Linux. Live-host tests verify real attachment,
replacement and duplicate refusal, while `tools/probe_native_menu_windows.ps1` sends a real Win32
`WM_COMMAND` and verifies the counter page's menu handler runs; a physical mouse click remains a
one-time human verification. The single Windows unsafe attachment is confined by a
crate-level deny and one function allowance under ADR-012's ninth exception. File and folder dialogs are implemented behind `dialogs`; the colour dialog is
implemented on Windows through the platform tool. Their one real modal selection remains a
one-time human verification rather than an automated claim, and the gaps document says so.

## 3. The comparison: what was measured and what it shows

Two real applications were cloned, ported, run and measured: `electron/electron-quick-start`
at `ecfb901f` and `AlexMercedCoder/example-tauri-app` at `c178b4f8`, both pinned in
`comparison/manifest.json`. The harness (`comparison/bench/compare.py`) builds both sides,
launches them, and writes `comparison/bench/results/windows.json`. The 2026-11-24 fail-closed
run completed five repeats and five 200-round IPC samples per configuration; median reported.

| Metric | Electron | NC-UI | Tauri 2 | NC-UI |
| --- | ---: | ---: | ---: | ---: |
| Cold start to a visible window | 268.5 ms | **61.2 ms** | 140.0 ms | **59.8 ms** |
| Idle working set (process tree) | **298.0 MiB** | 332.5 MiB | **320.5 MiB** | 332.6 MiB |
| Idle CPU (framework process) | **0.00 %** | **0.00 %** | **0.00 %** | **0.00 %** |
| IPC round trip (200 calls) | **0.278 ms** | 0.363 ms | 1.452 ms | **0.436 ms** |
| Distributed size | 356.8 MiB | **12.3 MiB** | **8.5 MiB** | 12.3 MiB |

**Where NC-UI wins:** 4.4× faster to a window than Electron and 29× smaller; 2.3× faster to a
window than Tauri 2 and 3.3× faster on its IPC row; level with both on framework-process idle CPU.

**Where it loses, and why** (`comparison/README.md` carries the full analysis):

- **IPC.** A page call crosses the webview boundary twice and the host's event loop runs on
  its own thread, so the response makes a thread hop Electron's C++ IPC does not. The
  blocking wait took this from 16.4 ms to 0.52 ms, and removing the asset server's 2 ms
  accept poll — which was waking the process five hundred times a second — took it to
  0.375 ms. The current medians are 0.363 ms for the Electron port and 0.436 ms for the Tauri
  port. The in-process dispatch is measured at a p99 of 900 ns, so the remaining cost is
  the boundary: the reply can only be delivered by evaluating a script, because `wry`
  exposes no `PostWebMessageAsJson` seam.
- **Memory.** The current NC-UI trees are 332.5 MiB and 332.6 MiB, above Electron's 298.0 MiB
  and Tauri 2's 320.5 MiB. An earlier per-process decomposition measured `ncui.exe` at about
  26 MiB against Electron's 95 MiB main process, and the WebView2 renderer and GPU processes smaller
  than Electron's. The tree is larger because the WebView2 runtime keeps a browser process
  of its own at about 129 MiB, plus two utility processes and a crashpad handler. The pages
  differ, so the renderer row is not a clean framework comparison; the runtime's own
  footprint is smaller.
- **Idle CPU.** Level, and it was not always: NC-UI read 0.78 % against 0.00 % for both
  incumbents until the asset server's accept loop stopped polling. The page loop was the
  first suspect and was disproved by measurement — raising its wait fourfold changed
  nothing — before the accept loop was found and fixed.
- **Size.** The runtime carries the scheduler, metrics, audit log, worker pool, capability
  machinery, native menus and the GPU island renderer. The current standard build is 12.3 MiB:
  29× smaller than Electron and 1.4× larger than Tauri 2. An earlier isolated build measured
  the renderer at 6.4 MiB, the same host without it at 4.88 MiB, and no window host at 1.64 MiB;
  those are historical decomposition figures, not a fresh breakdown of today's binary.

The honest reading is that NC-UI wins time to a window against both incumbents in this run and
beats Tauri 2 on IPC, but remains behind Electron on IPC and behind both on idle working set;
it also remains larger than Tauri 2 on disk. The gaps have known causes and are recorded with
their measurements rather than hidden.

## 4. Clone, port, test end to end

| Requirement | Evidence |
| --- | --- |
| Clone a real Electron app | `comparison/.originals/electron-quick-start`, cloned by the harness from the pinned commit; the harness clones rather than trusting a copy and re-checks `HEAD` against the pin after checkout, so the measurement is of the revision the manifest names. |
| Clone a real Tauri app | `comparison/.originals/example-tauri-app`, same mechanism. |
| Port both to NC-UI | `comparison/ports/electron-quick-start`, `comparison/ports/tauri-word-signal`. Each preserves what the original *does*; the Tauri port's native command is a real WebAssembly component under `plugins/word_signal.wat`, generated by `comparison/tools/make_word_signal_component.py` and checked against the generator by the policy gate. |
| Test everything end to end | `crates/ncui-runtime/tests/ports.rs` launches **both ports** in real windows, serves their assets over loopback HTTP, drives the page's bridge calls and asserts every round trip completes — 2 tests, passing. `crates/ncui-runtime/tests/islands.rs` fetches a GPU-rendered frame over real HTTP and asserts the island's colour is where the island was, that everything else is transparent, that scrolling moves the rows and that a row carrying text comes back with ink — 13 tests, passing on Windows; Linux still lacks a GPU adapter. `crates/ncui-runtime/tests/page_bridge.rs` drives the page→runtime→page path including refusal, consent-token validation, embedded children and audit — 23 tests, passing. |
| Benchmark both | The harness above; the report is tracked beside it and a CI job re-runs it. |

The Tauri application's `Menu` submenu is reproduced as a real native menu: `Hello` maximises the
window and `Goodbye` minimises it. The port declares those commands in its manifest, receives them
as `menu` events and applies them through `window_state`, reporting the state the platform returns.
`tools/probe_native_menu_windows.ps1` verifies the port's `Hello` item through a live Win32
`WM_COMMAND`, observing the page report `maximized true, minimized false`. The original's standard `Copy` item is platform chrome rather than an application-defined behavior.

## 5. No mock, fake, stub, TODO, placeholder or MVP

Mechanically checked, not asserted:

- `ncui-placeholders` scans every source root — `crates`, `protocols`, `tools/ncui-tools`,
  `fuzz`, `packages`, `examples`, `comparison/ports`, `comparison/bench/instrumentation` — and
  reports no prohibited markers. The scanner's own adversarial tests are the only exempt files,
  and the exemption is per-file in its configuration.
- The workspace lints deny `unwrap`, `expect`, `panic`, `todo`, `unimplemented`, `dbg!` and
  `print!`/`println!` in production code, and forbid `unsafe_code` except for the one
  package-scoped Windows menu attachment recorded in ADR-012. That exception cannot spread to
  another crate or platform without a deliberate policy change.
- `docs/sweep-adjudication.md` records the mechanical marker sweep and the adjudication of
  every hit: 204 hits, the majority in `target/` build output, the rest either fixed at the
  root or justified with evidence.
- Generated artifacts are generated: `examples/counter/plugins/greeter.wat` and
  `comparison/ports/tauri-word-signal/plugins/word_signal.wat` are produced by checked-in
  generators, and `make policy` fails if either drifts from its generator's output.

## 6. Windows first, then another OS

- **The working tree is complete; the last commit is not.** The earlier snapshot had 146 paths
  that had never been added to the index — three entire crates (`ncui-app`, `ncui-native`,
  `ncui-component`), the page client package, the tier-1 gate scripts, four ADRs,
  `principle.md`, `CHANGELOG.md`, and the counter example's assets. A clone of that commit could
  not resolve the workspace: `members = ["crates/*"]` and the workspace dependencies on
  `ncui-native` and `ncui-app` named manifests that were absent. Every gate in this audit runs
  against the working tree, and the current index is empty because no commit was requested.
  A green battery is therefore evidence about this working tree, not yet about a fresh clone;
  the final audit must export the intended source set, excluding local `.zvec/` and
  `.learnings/` data, and compile that export before making a fresh-clone claim (§8).
- **Windows.** The full gate battery passes: `cargo fmt --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace --all-targets` (exit 0),
  `RUSTDOCFLAGS="-D warnings" cargo doc`, the tools workspace's own fmt/clippy/tests, the
  policy gates, and the page bridge's Node suite. The real-window tests
  (`--features wry-host`: islands, page bridge, both comparison ports — 32 tests) pass.
  Every CI job that can run on this machine was run against the final tree: the `rust` job's
  four steps, the `adapters` job's four steps with `--all-features` (which is what caught the
  clippy defect in §8), the `client` suite, the `policy` job, the `benchmark` job's latency
  suite, the `soak` job at its elevated 2000 rounds, the `comparison` harness, and the
  `release` pipeline. `cargo deny check` also passes after locking cookie 0.18.3.
  `cargo-cyclonedx` was provisioned; the release pipeline generated a 103-component SBOM and
  two clean builds produced the identical SHA-256
  `ee3be54581946f5a7b72c55243b9a6643cf76e078bedc5fd6db49742511cb32f`.
  The fuzz job still needs `cargo-fuzz` and a nightly toolchain, and no signing key is configured.
- **The gate scripts were broken on Windows and are fixed.** Every shell wrapper calls
  `python3`, which on this machine is the WindowsApps interpreter with a cp1252 locale
  default, and the Python tools decoded subprocess output with `text=True` and no encoding.
  The pre-commit hook therefore crashed with `UnicodeDecodeError` on any staged diff
  containing a non-ASCII byte — the gate that is supposed to run on every commit could not
  run at all here, and would have taught its user to reach for `--no-verify`. Seventeen
  subprocess calls across the four gate scripts, the release tooling, the coverage probe and
  the comparison harness now name `encoding="utf-8"`; the hook runs clean, and rejects a
  staged diff that carries a banned pattern.
- **Linux.** `tools/linux_gate.py` runs twenty-three stages under WSL2: formatting, clippy,
  default and release-configuration tests, docs, architecture and registry checks, placeholder
  scans, both component generators, the bundler, the client suite and hygiene. Its printed test
  tally is authoritative and the latest observed result is recorded in §8. The real-window stage
  opens genuine GTK/WebKit windows through WSLg for the page bridge and both comparison ports.
  That required a root-cause fix: `ncui-webview-wry` had created its event loop on a worker thread
  and set tao's `with_any_thread(true)` on Windows only, so the Unix backend panicked and `ncui
  run` could not open a window at all. Earlier claims that GTK/WebKit or a display were unavailable
  were wrong. The GPU-dependent island tests also run under WSL2 with Mesa lavapipe, asserting
  rendered pixels and the live HTTP route without requiring access to the physical device node.
- **The registry's per-feature Linux flags stay deliberately conservative.** A flag is set only
  when the feature's whole evidence set ran on that platform; a green general gate does not prove
  every benchmark, metric and integration slot. Native menus and embedded child webviews also
  have live Linux evidence from this session. `webview.cdp` has a narrower Linux contract — the
  WebKitGTK inspector is real, while Chromium's CDP transport is deliberately refused as
  `cdp_platform_unsupported` — so its flag must follow that exact evidence rather than imply a
  Linux WebView2 CDP endpoint.
- **macOS.** No macOS host is reachable. `cross_platform.verification.macos` is the single
  `not implemented` entry, and every feature's macOS platform state is `unverified`. This is
  the objective's "then other OS" stopped by hardware, disclosed rather than claimed.

## 7. The engineering standard

The Rust Engineering Excellence instruction's load-bearing requirements, and where each is
met:

| Requirement | Evidence |
| --- | --- |
| Real implementation, no fakes | §5; the gaps document states what is absent and why rather than filling it. |
| Root-cause fixes | `docs/known-gaps.md` records the cause and the fix for each defect found: the IPC sleep, the deadline test's nested build, the registry's platform claims, the `windows` crate version split, the installer's missing executable bit found by the Linux gate. |
| Measured optimisation | The page bridge's blocking wait: 16.4 ms → 0.52 ms, measured before and after, with the cause named; then the asset server's accept poll: idle CPU 0.78 % → 0.00 % and IPC 0.52 ms → 0.375 ms, with the page-loop suspect disproved by a fourfold change that moved nothing. The comparison records its methodology, its repeats and its spread. |
| Evidence over speculation | Every capability carries evidence a reader can check; `tools/correct_test_counts.py` re-derives each cited test count from the file it names, because a hand-typed count against a changed file passes a weaker check. |
| Change and removal audit | Removals and behaviour changes are recorded as changelog fragments under `changelog/unreleased/`, and `docs/sweep-adjudication.md` is the record for the marker sweep. The instruction's recommended `docs/engineering/change-removal-log.md` is not used; this repository's convention is the changelog plus git history, and the divergence is recorded here rather than left implicit. |
| Incomplete-work transparency | `docs/remaining-work.md` and `docs/known-gaps.md` state what is absent and what remains. |
| Documentation kept in step | This audit exists because a figure transcribed into a document drifts from the artifact it cites; the project has made that defect and corrected it twice. This pass found six more: `docs/tier1-plan.md` still called itself "design only" while CI runs its guards, the audit itself said eleven ADRs when there are sixteen, the README layout omitted `ncui-component`, ADR-011's layer table omitted the three layer-2 crates added since it was written, `arch.rs`'s diagram did the same, and `docs/sweep-adjudication.md` described one root fix twice under two headings while its table claimed two. That last one is now two distinct fixes described once each, with its stale line numbers removed and its `#[allow]` count stated as a snapshot beside the gate that actually holds the invariant. |
| A documented invariant is only as good as its gate | `docs/sweep-adjudication.md` claimed every `#[allow]` carries a written reason, "verified by reading the full attribute from source". The workspace already sets `clippy::allow_attributes_without_reason`, and a probe — an `#[allow(dead_code)]` with no reason — fails clippy with exit 101. The document now names the gate rather than the reading. `principle.md` states that every text file ends with exactly one trailing newline "and the staged-content check gates it"; nothing gated it and 23 tracked files had no final newline. The hygiene scanner now checks both halves in `--tree` and `--staged`, the files are fixed, and the corpus is excluded because a seed without a newline is a valid fuzzer input. |

## 8. Verification log

| Command | Result |
| --- | --- |
| `cargo test --workspace --all-targets` | exit 0 |
| `cargo test -p ncui-runtime --features wry-host --test ports --test islands --test page_bridge` | 32 passed, 0 failed |
| `powershell -File tools/probe_native_menu_windows.ps1`, then the same probe with the Tauri port's `Hello` item | both pass: the real Win32 command reaches each page, and the port reports `maximized true, minimized false` |
| `cargo deny check` | advisories, bans, licences and sources pass after locking cookie 0.18.3 |
| `python scripts/file-size.py` | 0 errors |
| `python scripts/changelog-fold.py --check` | passes |
| `python scripts/hygiene-scan.py --tree` | 0 errors |
| `./tools/target/release/ncui-arch .` | layering satisfied, 20 crates |
| `./tools/target/release/ncui-registry protocols/feature-registry.json` | 76 complete, 1 not implemented, every completed feature carries evidence |
| `python tools/correct_test_counts.py --check` | all 71 cited counts match |
| `python tools/make_greeter_component.py --check` | matches the generator |
| `python comparison/tools/make_word_signal_component.py --check` | matches the generator |
| `python tools/linux_gate.py` | 23 stages pass, 1373 + 52 tests, 0 failed |
| `cargo build --release -p ncui-cli --features ncui-cli/wry-host` | builds; `ncui run examples/counter` serves `/__ncui/island` with 200 and the expected pixels |
| the same tree with `gpu-islands` removed from `wry-host`, and with no window host | 4.880 MiB and 1.641 MiB against the release build's 11.269 MiB — the renderer's 6.4 MiB, measured by building it rather than asserted |
| `NCUI_SOAK_ROUNDS=2000 cargo test -p ncui-runtime --test soak` | 5 passed, 0 failed |
| `python tools/test_build_release.py` | 0 failures, one skip (no signing key on this machine) |
| `python tools/build_release.py` | passes: 103-component SBOM, two clean release builds with identical SHA-256 `ee3be54581946f5a7b72c55243b9a6643cf76e078bedc5fd6db49742511cb32f`; no signing key configured |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features` | clean |
| an earlier staged-index export to an empty directory: `cargo metadata --no-deps` then `cargo check --workspace --all-targets` | both succeeded for that earlier tree; the final working-tree export must be rechecked after the final edits |
| `cargo test -p ncui-app --all-targets` | 18 passed, 0 failed, including every server stop |
| `cargo test -p ncui-gpu-renderer --features text-shaping` | 54 passed, 0 failed — six of them pin the island row wrapping, on Windows and again under WSL2 |
| the island route, over real HTTP from the release binary | a 64×96 row with `aaaa bbbb cccc dddd` at 16 px draws ink across 40 of the row's 96 pixel rows; the same row with a single letter draws 11; the accessible description carries the row's whole text |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | clean; it caught a collapsible-`if` warning in `crates/ncui-runtime/tests/ports.rs` that the default-feature lint never sees |
| `cargo clippy --workspace --all-targets --features ncui-cli/wry-host -- -D warnings` | clean; the release configuration had a `needless_pass_by_value` warning that only appears without the tray feature |
| `python comparison/bench/compare.py` | 2026-11-24: fail-closed pass, 5 repeats and 5 complete 200-round IPC samples per configuration; startup p50 268.5/61.2/140.0/59.8 ms, idle tree p50 298.0/332.5/320.5/332.6 MiB, IPC p50 0.278/0.363/1.452/0.436 ms (Electron, NC-UI, Tauri 2, NC-UI) |
| idle CPU, ten-second sample of `ncui run` | 0.94 % before the asset server fix, 0.00 % after |
| `cargo bench -p ncui-bench --bench latency` | `runtime.page_command_dispatch` p99 900 ns — the in-process half of the IPC round trip |
| per-process working set of the ported app | `ncui.exe` 26 MiB, WebView2 browser 129 MiB, renderer 53 MiB, GPU 68 MiB; Electron's main 95 MiB, renderer 65 MiB, GPU 94 MiB |

## 9. What remains

1. **macOS verification** — needs a Mac host, and now has a known blocker on top of that: tao's macOS backend requires the event loop on the main thread and offers no any-thread switch, while this host creates it on a worker thread by design, so the release configuration cannot open a window there. Documented in `docs/known-gaps.md` with the source; the host's tests are compiled out on macOS with that reason.
2. **IPC and memory parity with the incumbents** — the measured gaps, with their causes in
   `comparison/README.md`. The IPC path is the most tractable: the remaining cost is the
   WebView2 boundary and a thread hop.
3. **A zero-copy GPU composition path** — `wry`'s public API exposes no shared-texture seam,
   so the pixels cross the CPU. A different port or a different composition boundary would be
   needed.
4. **Release signing** — the SBOM and reproducible-build checks pass, but no signing key is
   configured on this machine, so the detached-signature step remains unexercised here.
