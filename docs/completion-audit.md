# Completion audit

**Date:** 2026-11-08. **Machine:** Windows 11 x86_64, `rustc 1.98.1`, matching
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
| GPU-driven by default | Done, with a precise meaning | The release build carries the GPU renderer: `ncui-cli`'s `wry-host` enables `gpu-islands`, so `ncui run` serves `/__ncui/island` rather than a blank rectangle. Verified over real HTTP from the release binary: `200`, 1,228,800 bytes for a 640×480 frame, the row colour at the row's pixels, transparent elsewhere. The page's own rendering is the webview's, which is itself GPU-composited. What "GPU-driven" does **not** mean: the runtime does not draw the whole UI itself. |
| Pure parallel | Done | `ncui-scheduler` (cooperative budgets, no starvation) and `ncui-workers` (bounded queues, cancellation, eviction) are complete with tests that assert the bounds rather than the absence of a crash. |
| Best frontend | Done, as a property not a ranking | A page is an ordinary web page: HTML, CSS and JavaScript, any framework, served over the app's own origin. The bridge is ordinary JavaScript (`packages/ncui-client`, with its own suite). "Best" is not a claim this document can prove; what is claimed is that nothing about the frontend is constrained by the runtime, and that a GPU region's content reaches assistive technology through a description the page mirrors into the DOM. |
| Suitable and supportable | Done | `README.md`, `docs/known-gaps.md`, `docs/remaining-work.md`, sixteen ADRs, `principle.md`, a 72-entry feature registry, and a gate battery that runs in CI on three platforms. |
| Better in every respect than Electron or Tauri | **Not established** | See §3. NC-UI wins against Electron on cold start and size, is level with Tauri on start, and all three measure 0.00 % CPU at idle; it loses to both on IPC and memory and to Tauri on size. The objective asked for a proof of superiority in every respect; the measurement does not support that claim and it is not made. |

## 2. Every missing capability

The registry tracks 72 capabilities. **`ncui status --registry protocols/feature-registry.json`
reports 71 complete and 1 not implemented.** The one absent entry is
`cross_platform.verification.macos`: the full gate suite run on macOS. Nothing about the gate
is missing; a macOS host is not reachable from the machine that produced the registry, and a
platform claim without a run is exactly what this registry exists to prevent.

Two further capabilities are recorded `absent` in `docs/known-gaps.md` with their reasons: a
native menu bar on Windows (`muda`'s only Windows entry point is `pub unsafe fn`, and
`unsafe_code = "forbid"` is workspace-wide) and file/colour dialogs (a dialog blocks until a
person answers, so its core cannot be exercised by a test). Both are disclosed rather than
faked; the alternatives are named in the gaps document.

## 3. The comparison: what was measured and what it shows

Two real applications were cloned, ported, run and measured: `electron/electron-quick-start`
at `ecfb901f` and `AlexMercedCoder/example-tauri-app` at `c178b4f8`, both pinned in
`comparison/manifest.json`. The harness (`comparison/bench/compare.py`) builds both sides,
launches them, and writes `comparison/bench/results/windows.json`. Five repeats per figure,
median reported.

| Metric | Electron | NC-UI | Tauri | NC-UI |
| --- | ---: | ---: | ---: | ---: |
| Cold start to a visible window | 308.8 ms | **51.9 ms** | 52.4 ms | **51.8 ms** |
| Idle working set (process tree) | 301.3 MiB | 349.1 MiB | 335.9 MiB | 350.9 MiB |
| Idle CPU (framework process) | **0.00 %** | **0.00 %** | **0.00 %** | **0.00 %** |
| IPC round trip (200 calls) | **0.188 ms** | 0.375 ms | **0.281 ms** | 0.329 ms |
| Distributed size | 356.8 MiB | **11.3 MiB** | **3.2 MiB** | 11.3 MiB |

**Where NC-UI wins:** 5.9× faster to a window than Electron and 32× smaller; level with Tauri
on cold start and level with both on idle CPU.

**Where it loses, and why** (`comparison/README.md` carries the full analysis):

- **IPC.** A page call crosses the webview boundary twice and the host's event loop runs on
  its own thread, so the response makes a thread hop Electron's C++ IPC does not. The
  blocking wait took this from 16.4 ms to 0.52 ms, and removing the asset server's 2 ms
  accept poll — which was waking the process five hundred times a second — took it to
  0.375 ms. The in-process dispatch is measured at a p99 of 900 ns, so the remaining cost is
  the boundary: the reply can only be delivered by evaluating a script, because `wry`
  exposes no `PostWebMessageAsJson` seam.
- **Memory.** Measured per process, not inferred: `ncui.exe` is about 26 MiB against
  Electron's 95 MiB main process, and the WebView2 renderer and GPU processes are smaller
  than Electron's. The tree is larger because the WebView2 runtime keeps a browser process
  of its own at about 129 MiB, plus two utility processes and a crashpad handler. The pages
  differ, so the renderer row is not a clean framework comparison; the runtime's own
  footprint is smaller.
- **Idle CPU.** Level, and it was not always: NC-UI read 0.78 % against 0.00 % for both
  incumbents until the asset server's accept loop stopped polling. The page loop was the
  first suspect and was disproved by measurement — raising its wait fourfold changed
  nothing — before the accept loop was found and fixed.
- **Size.** The runtime carries the scheduler, metrics, audit log, worker pool, capability
  machinery and the GPU island renderer. The renderer is 6.4 MiB of the 11.3, measured by
  building the same tree with the window host and without it (4.88 MiB); a build with no
  window host at all is 1.64 MiB and refuses to open a window.

The honest reading is that the framework is competitive with the best incumbent on the metric
users notice first — time to a window — and behind on the metrics that matter under load. The
gaps have known causes and are recorded with their measurements rather than hidden.

## 4. Clone, port, test end to end

| Requirement | Evidence |
| --- | --- |
| Clone a real Electron app | `comparison/.originals/electron-quick-start`, cloned by the harness from the pinned commit; the harness clones rather than trusting a copy and re-checks `HEAD` against the pin after checkout, so the measurement is of the revision the manifest names. |
| Clone a real Tauri app | `comparison/.originals/example-tauri-app`, same mechanism. |
| Port both to NC-UI | `comparison/ports/electron-quick-start`, `comparison/ports/tauri-word-signal`. Each preserves what the original *does*; the Tauri port's native command is a real WebAssembly component under `plugins/word_signal.wat`, generated by `comparison/tools/make_word_signal_component.py` and checked against the generator by the policy gate. |
| Test everything end to end | `crates/ncui-runtime/tests/ports.rs` launches **both ports** in real windows, serves their assets over loopback HTTP, drives the page's bridge calls and asserts every round trip completes — 2 tests, passing. `crates/ncui-runtime/tests/islands.rs` fetches a GPU-rendered frame over real HTTP and asserts the island's colour is where the island was, that everything else is transparent, that scrolling moves the rows and that a row carrying text comes back with ink — 10 tests, passing. `crates/ncui-runtime/tests/page_bridge.rs` drives the page→runtime→page path including refusal and audit — 8 tests, passing. |
| Benchmark both | The harness above; the report is tracked beside it and a CI job re-runs it. |

One behaviour of the Tauri application is deliberately not reproduced: its native menu bar.
The reason is project policy, not an oversight — a Windows menu needs `muda`'s
`init_for_hwnd`, which is `pub unsafe fn`. It is recorded as absent in `docs/known-gaps.md`
rather than faked with a page-side button that does something else.

## 5. No mock, fake, stub, TODO, placeholder or MVP

Mechanically checked, not asserted:

- `ncui-placeholders` scans every source root — `crates`, `protocols`, `tools/ncui-tools`,
  `fuzz`, `packages`, `examples`, `comparison/ports`, `comparison/bench/instrumentation` — and
  reports no prohibited markers. The scanner's own adversarial tests are the only exempt files,
  and the exemption is per-file in its configuration.
- The workspace lints deny `unwrap`, `expect`, `panic`, `todo`, `unimplemented`, `dbg!` and
  `print!`/`println!` in production code, and forbid `unsafe_code` outright. A shortcut cannot
  be expressed, so it is not present.
- `docs/sweep-adjudication.md` records the mechanical marker sweep and the adjudication of
  every hit: 204 hits, the majority in `target/` build output, the rest either fixed at the
  root or justified with evidence.
- Generated artifacts are generated: `examples/counter/plugins/greeter.wat` and
  `comparison/ports/tauri-word-signal/plugins/word_signal.wat` are produced by checked-in
  generators, and `make policy` fails if either drifts from its generator's output.

## 6. Windows first, then another OS

- **The committed tree was incomplete, and it is now whole.** `git status` showed 146 paths
  that had never been added to the index — three entire crates (`ncui-app`, `ncui-native`,
  `ncui-component`), the page client package, the tier-1 gate scripts, four ADRs,
  `principle.md`, `CHANGELOG.md`, and the counter example's assets. A clone of the last commit
  would not resolve the workspace: `members = ["crates/*"]` and the workspace dependencies on
  `ncui-native` and `ncui-app` name manifests that were not in the tree. Every gate run in this
  audit was against the working tree, so no gate could have caught it — a green battery is
  evidence about the tree it ran on, not about the tree it would take to a fresh machine. The
  whole tree is now staged, and the staged index was exported to an empty directory and compiled
  there to prove the point (§8). Bringing those files into the index also brought them into the
  gates that enumerate tracked files: the hygiene scan went from 0 findings to 11, all of them
  fixtures and pattern strings rather than real paths or secrets, each now carrying an inline
  reason.
- **Windows.** The full gate battery passes: `cargo fmt --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace --all-targets` (exit 0),
  `RUSTDOCFLAGS="-D warnings" cargo doc`, the tools workspace's own fmt/clippy/tests, the
  policy gates, and the page bridge's Node suite. The real-window tests
  (`--features wry-host`: islands, page bridge, both comparison ports — 21 tests) pass.
  Every CI job that can run on this machine was run against the final tree: the `rust` job's
  four steps, the `adapters` job's four steps with `--all-features` (which is what caught the
  clippy defect in §8), the `client` suite, the `policy` job, the `benchmark` job's latency
  suite, the `soak` job at its elevated 2000 rounds, the `comparison` harness, and the
  `release` job's tooling tests. Three jobs cannot run here and are covered only by CI:
  `supply-chain` needs `cargo-deny`, `fuzz` needs `cargo-fuzz` and a nightly toolchain, and
  the `release` job's SBOM step needs `cargo-cyclonedx` — none of the three is installed on
  this machine, and that is a statement about the machine rather than about the tree.
- **The gate scripts were broken on Windows and are fixed.** Every shell wrapper calls
  `python3`, which on this machine is the WindowsApps interpreter with a cp1252 locale
  default, and the Python tools decoded subprocess output with `text=True` and no encoding.
  The pre-commit hook therefore crashed with `UnicodeDecodeError` on any staged diff
  containing a non-ASCII byte — the gate that is supposed to run on every commit could not
  run at all here, and would have taught its user to reach for `--no-verify`. Seventeen
  subprocess calls across the four gate scripts, the release tooling, the coverage probe and
  the comparison harness now name `encoding="utf-8"`; the hook runs clean, and rejects a
  staged diff that carries a banned pattern.
- **Linux.** `tools/linux_gate.py` runs its twenty-three stages under WSL2 and passes all of them:
  1373 workspace tests and 52 tools tests, 0 failed, plus formatting, clippy, docs, arch,
  registry, the placeholder scans, both component generators, the bundler and the client
  suite — including the hygiene scan's trailing-newline check, which runs there against the
  same tree. The gate runs the default configuration; the release configuration compiles *and runs* there. Two stages cover it: one compiles
  `ncui-cli --features ncui-cli/wry-host`, and one runs the unit tests of the release
  configuration's crates plus the real-window integration tests — the host's 8 tests, the 8
  page-bridge tests and both comparison ports, all opening genuine windows through WSLg. That
  only became true after a real fix: `ncui-webview-wry` created its event loop on a worker
  thread and set tao's `with_any_thread(true)` on Windows only, so the Unix backend panicked and
  `ncui run` could not open a window on Linux at all. Earlier revisions of this audit said the
  GTK and WebKit libraries were unavailable, then that the session had no display; both were
  wrong. The islands test still needs a GPU device, which this session lacks.
- **The registry's per-feature Linux flags stay deliberately conservative.** 25 features are
  recorded `linux: unverified` even though the gate now runs real windows there. A flag is worth
  setting only when the feature's *whole* evidence set ran on that platform, and for most of those
  25 that is not established — several carry benchmark evidence this run does not measure, and a
  platform flag that over-claims is worse than one that under-claims. Three were directly observed
  on Linux this session — `webview.host` (8 host tests), `runtime.page_bridge` (8 page-bridge
  tests) and `process.bundling` (the bundler's 9 tests) — and the rest stay conservative until
  each one's evidence is checked individually.
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
| Change and removal audit | Removals and behaviour changes are recorded as changelog fragments under `changelog/unreleased/` (five at the time of writing), and `docs/sweep-adjudication.md` is the record for the marker sweep. The instruction's recommended `docs/engineering/change-removal-log.md` is not used; this repository's convention is the changelog plus git history, and the divergence is recorded here rather than left implicit. |
| Incomplete-work transparency | `docs/remaining-work.md` and `docs/known-gaps.md` state what is absent and what remains. |
| Documentation kept in step | This audit exists because a figure transcribed into a document drifts from the artifact it cites; the project has made that defect and corrected it twice. This pass found six more: `docs/tier1-plan.md` still called itself "design only" while CI runs its guards, the audit itself said eleven ADRs when there are sixteen, the README layout omitted `ncui-component`, ADR-011's layer table omitted the three layer-2 crates added since it was written, `arch.rs`'s diagram did the same, and `docs/sweep-adjudication.md` described one root fix twice under two headings while its table claimed two. That last one is now two distinct fixes described once each, with its stale line numbers removed and its `#[allow]` count stated as a snapshot beside the gate that actually holds the invariant. |
| A documented invariant is only as good as its gate | `docs/sweep-adjudication.md` claimed every `#[allow]` carries a written reason, "verified by reading the full attribute from source". The workspace already sets `clippy::allow_attributes_without_reason`, and a probe — an `#[allow(dead_code)]` with no reason — fails clippy with exit 101. The document now names the gate rather than the reading. `principle.md` states that every text file ends with exactly one trailing newline "and the staged-content check gates it"; nothing gated it and 23 tracked files had no final newline. The hygiene scanner now checks both halves in `--tree` and `--staged`, the files are fixed, and the corpus is excluded because a seed without a newline is a valid fuzzer input. |

## 8. Verification log

| Command | Result |
| --- | --- |
| `cargo test --workspace --all-targets` | exit 0 |
| `cargo test -p ncui-runtime --features wry-host --test ports --test islands --test page_bridge` | 21 passed, 0 failed |
| `python scripts/file-size.py` | 0 errors |
| `python scripts/changelog-fold.py --check` | passes |
| `python scripts/hygiene-scan.py --tree` | 0 errors |
| `./tools/target/release/ncui-arch .` | layering satisfied, 19 crates |
| `./tools/target/release/ncui-registry protocols/feature-registry.json` | 71 complete, 1 not implemented, every completed feature carries evidence |
| `python tools/correct_test_counts.py --check` | all 69 cited counts match |
| `python tools/make_greeter_component.py --check` | matches the generator |
| `python comparison/tools/make_word_signal_component.py --check` | matches the generator |
| `python tools/linux_gate.py` | 23 stages pass, 1373 + 52 tests, 0 failed |
| `cargo build --release -p ncui-cli --features ncui-cli/wry-host` | builds; `ncui run examples/counter` serves `/__ncui/island` with 200 and the expected pixels |
| the same tree with `gpu-islands` removed from `wry-host`, and with no window host | 4.880 MiB and 1.641 MiB against the release build's 11.269 MiB — the renderer's 6.4 MiB, measured by building it rather than asserted |
| `NCUI_SOAK_ROUNDS=2000 cargo test -p ncui-runtime --test soak` | 5 passed, 0 failed |
| `python tools/test_build_release.py` | 0 failures, one skip (no signing key on this machine) |
| `tools/build_release.py`'s reproducibility check, run directly | **fails on Windows**: two clean builds of one source differ by 403,978 bytes, because a dependency's build script embeds its `OUT_DIR`. Two builds into two different temp directories differ by 69 bytes, which is how the path was isolated. Recorded in `docs/known-gaps.md`; the claim is now "reproducible for a fixed build directory", not "reproducible". |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features` | clean |
| the staged index exported to an empty directory: `cargo metadata --no-deps` then `cargo check --workspace --all-targets` | both succeed — the tree a clone would take is a complete, buildable repository |
| `cargo test -p ncui-app --all-targets` | 18 passed, 0 failed, including every server stop |
| `cargo test -p ncui-gpu-renderer --features text-shaping` | 54 passed, 0 failed — six of them pin the island row wrapping, on Windows and again under WSL2 |
| the island route, over real HTTP from the release binary | a 64×96 row with `aaaa bbbb cccc dddd` at 16 px draws ink across 40 of the row's 96 pixel rows; the same row with a single letter draws 11; the accessible description carries the row's whole text |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | clean; it caught a collapsible-`if` warning in `crates/ncui-runtime/tests/ports.rs` that the default-feature lint never sees |
| `cargo clippy --workspace --all-targets --features ncui-cli/wry-host -- -D warnings` | clean; the release configuration had a `needless_pass_by_value` warning that only appears without the tray feature |
| `python comparison/bench/compare.py` | 5 repeats per configuration; idle CPU 0.00 % for all four, NC-UI IPC 0.375 ms against Electron's 0.188 ms |
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
4. **A Windows menu bar and file dialogs** — recorded absent with their reasons.
5. **A row taller than its text is clipped, not scrolled** — wrapping fits a row's text into
   the row's width, but a block of lines taller than the row is cut off at the bottom rather
   than scrolled or elided.
6. **The release binary is reproducible for one directory only** — a dependency embeds its
   build-output path, so the same source built into two directories differs by 403,978 bytes
   on Windows. A consumer rebuilding elsewhere cannot verify the published digest. Recorded
   in `docs/known-gaps.md` with the byte evidence; not fixable inside this tree.
