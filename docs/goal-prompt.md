# NC-UI Goal Prompt — best-in-class against Electron, Tauri 2, and Zero Native

**Status:** Draft goal specification.
**Baseline:** the tree as measured in `docs/completion-audit.md` (2026-11-08) and `docs/known-gaps.md`.
**Bar:** not "passes the gates." The gates already pass. The bar is *wins or ties on every axis a
user or a benchmark can observe*, on every platform the competitors ship, with no capability
silently absent.

This document is written in the project's own idiom: every requirement is reduced to a claim a
reader can re-run and see. Anything that cannot yet be measured is labelled, and the honest
baseline is stated before the target so the gap is visible rather than implied.

---

## 0. Evidence tiers used below

- **[E-measured]** — already measured in this repo; the number is the baseline.
- **[E-claimed]** — a competitor figure we have *not* independently measured. Treated as a target
  to beat, never quoted as fact. Zero Native figures are entirely in this tier until we clone and
  run them through `comparison/bench/compare.py`.
- **[E-target]** — the number we commit to reaching, to be proven by a command in §7.

---

## 1. Where NC-UI stands today (the honest baseline)

From `docs/completion-audit.md` §3 and §9, Windows x86_64, 5 repeats, median:

| Metric | Electron | NC-UI now | Tauri | Zero Native |
| --- | ---: | ---: | ---: | ---: |
| Cold start to window | 308.8 ms | **51.9 ms** | 52.4 ms | [E-claimed] tiny/instant |
| Idle working set | 301.3 MiB | 349.1 MiB | 335.9 MiB | [E-claimed] |
| Idle CPU | 0.00 % | **0.00 %** | 0.00 % | — |
| IPC round trip | **0.188 ms** | 0.375 ms | 0.281 ms | — |
| Distributed size | 356.8 MiB | 11.3 MiB | **3.2 MiB** | [E-claimed] tiny |

**Structural gaps that are not numbers:**

- macOS does not work (`tao` main-thread event-loop panic) — desktop is Win + Linux only.
- **No mobile story at all.** Tauri 2 ships iOS/Android; Zero Native claims mobile.
- Native menus (Windows) and devtools are **absent**. File/save/folder dialogs are
  **implemented** behind the `dialogs` feature (`rfd`, ADR-012's eighth exception), except the
  single real modal open, which needs a one-time human verification a test cannot drive and the
  feature registry does not model; a **colour** dialog is still absent (`rfd` does not provide
  one).
- The page bridge is **JSON-only**; no binary path.
- Consent is a **plain `bool`**; no unforgeable token.
- **No bundler / transform layer.** The asset server serves files from disk and transforms
  nothing, so a TypeScript/JSX app must compile its own output first. What *is* built around the
  dev loop: `ncui new` scaffolds a runnable app in one command; `ncui run --watch` reloads the page
  **with its designated state kept** (the page opts in via one bridge script); the asset server
  sends `ETag`/`Last-Modified` and answers conditional `GET` with `304`; and `ncui build` bundles
  the built-output directory the manifest's `assets` field names, not raw source. The remaining
  gap is the transform itself (built-in Rust compiler vs. a dev-server proxy), which trades against
  the no-Node moat and is recorded as an open decision.
- Native commands are **Rust-only**; a JS-only app cannot register one.
- Release binary is reproducible for one build directory only (embedded `OUT_DIR`).

---

## 2. Goal A — close every measured performance gap

NC-UI must win or tie on **every** row of the comparison table, on **every** platform it
claims, not just cold-start.

### A1. IPC round trip — beat 0.188 ms (Electron)
- **Baseline [E-measured]:** 0.375 ms. The cause is measured, not inferred: the in-process
  half (capability check, handler, response serialisation) is a p99 of **900 ns**, so **99.8 %
  of the round trip is the WebView boundary** — the page's `postMessage` in, the host's
  `evaluate_script` out, and a thread hop because the host's event loop runs on its own thread
  (`comparison/README.md`). `wry` exposes no `PostWebMessageAsJson` seam, so the reply can only
  be delivered by evaluating a script string.
- **Correction to an earlier draft of this goal (measure-don't-trust):** a *binary* bridge
  does **not** close the small-round-trip gap. A binary transport can carry the page→host
  *request*, but the host→page *reply* must still cross as `evaluate_script(&str)`, so the
  boundary hop — 99.8 % of the cost — remains. A binary path is worth building for
  **large-payload throughput** (an app moving a big buffer pays JSON's cost twice today), not
  for the 200-call small-payload latency this row measures. Chasing ≤ 0.19 ms here with a
  binary bridge would be optimising the 0.2 %.
- **What would actually move this row [E-target]:** remove a boundary crossing or the thread
  hop, not the encoding. Concretely: (a) a `PostWebMessageAsJson`-equivalent seam in the port
  (likely means a `wry` upstream change or a different port), or (b) co-locate the host's
  reply path with the page's event loop to remove the thread hop, or (c) accept the boundary
  cost and stop counting it as a defect — Electron's direct C++ IPC is a different architecture,
  and Tauri (0.281 ms) pays the *same* WebView2 boundary, which is why it is only 1.2× faster
  where Electron is 2.0×.
- **Reframed target:** keep the binary bridge as a *throughput* goal (large payloads), measured
  separately with a payload-size sweep; do not book it against the ≤ 0.19 ms small-round-trip
  target, which the boundary — not the encoding — owns.
- **Measured status [E-measured] — the binary *data* channel is shipped.** The asset server now
  answers `POST`: it reads the request body (bounded, 413 above `MAX_REQUEST_BODY_BYTES`) and
  hands the bytes to a route, so a page moves a large buffer raw instead of JSON-encoding it.
  `binary_channel/round_trip` in `ncui-bench` sweeps 1 KiB–1 MiB against a real listener over a
  real socket: ~150 µs fixed per round trip, throughput climbing to ~176 MiB/s at 64 KiB. The
  WebView *boundary* hop (the 99.8 % that owns the ≤ 0.19 ms row) is untouched by design — this
  channel is the throughput half, measured on its own. Evidence: `changelog/unreleased/` binary
  POST channel fragment; `ncui-app` asset tests (byte-for-byte POST echo over a real socket).
- **Evidence:** `comparison/README.md` IPC analysis; `comparison/bench/compare.py` row; the
  900 ns in-process p99 in the latency harness (`runtime.page_command_dispatch`).

### A2. Idle working set — beat 301.3 MiB (Electron)
- **Baseline [E-measured]:** 349.1 MiB, of which ~129 MiB is WebView2's own browser process.
- **Target [E-target]:** ≤ 300 MiB tree total, Windows and Linux.
- **Required work:** lazy-init the subsystems a minimal app does not touch (GPU renderer,
  worker pool, metrics ring) so a window-with-a-page does not pay for them; measure the
  decomposition per process as §3 of the audit does.
- **Lazy-init — done [E-measured].** All three named subsystems are now lazy or bounded: the
  worker pool spawns its threads on first submitted task rather than at startup (it used to
  spawn `cpu_count.clamp(1,64)` — twelve on this host — into every window-with-a-page that
  never submits background work), the GPU renderer's `host` is `Option` and stays `None`
  until islands install, and the metrics/log rings bound their pre-allocation
  (`.min(64)`/`.min(1024)`). `scripts/measure_a2_working_set.py` measures the result with the
  comparison harness's own Win32 `process_tree_memory` walk.
- **Measured decomposition [E-measured], minimal scaffolded app (window + counter, only
  `read.metrics` + `window`), 5 repeats, median:** tree total **317.4 MiB**;
  `msedgewebview2.exe` **301.6 MiB**; `ncui.exe` **23.1 MiB**. The host is lean (the audit's
  ~26 MiB) and the worker pool spawns zero threads — **but WebView2's own processes alone
  exceed the 300 MiB target.**
- **Verdict — blocked on the engine, not on host allocations [E-measured].** The ≤ 300 MiB
  target cannot be reached by lazy-initialising host subsystems while the system WebView2 is
  the engine: the gap is the web engine's multi-process footprint (a browser process, a
  renderer, a GPU process and utility/crashpad handlers), which is an architectural property
  of the port and not an allocation the host controls. Reaching ≤ 300 MiB would require
  changing the engine or its process model — a **project decision** (a different WebView port,
  a single-process mode, or a lighter engine for minimal apps), not a code task. Recorded here
  and in `known-gaps.md` rather than marked done.

### A3. Distributed size — approach 3.2 MiB (Tauri)
- **Baseline [E-measured]:** 11.3 MiB; the GPU island renderer is 6.4 MiB of it.
- **Target [E-target]:** a `ncui build --profile minimal` that ships a window host **without**
  the GPU renderer for apps that do not use islands, landing ≤ 5 MiB; keep the full build for
  apps that do.
- **Done [E-measured].** `ncui build --profile {standard,minimal}` compiles the named profile's
  own runtime and bundles it. `standard` (the default) is the full build — window host plus the
  GPU-island renderer; `minimal` is the window host alone, which links out `wgpu` and the text
  shaper. The renderer became genuinely optional at link time: `wry-host` no longer implies
  `gpu-islands` (that implication was what forced 6.4 MiB into every window host), so a profile
  can choose. Measured on the counter example: standard **11.292 MiB**, minimal **4.902 MiB**
  (binary 4.872 MiB) — **≤ 5 MiB, target met**. The minimal bundle was launched and opened a
  real window (E2E). A page that asks for an island on `minimal` gets the asset server's 404 —
  the island route is `#[cfg(feature = "gpu-islands")]`-gated — which the counter example already
  renders as "No island on this machine" rather than a failure.
- **Constraint honoured:** the standard build is byte-for-byte the size it was (11.292 MiB), so
  nothing was removed — the heavy subsystem was made *optional at link time*, as the constraint
  requires. The remaining gap to Tauri's 3.2 MiB is the WebView2 host itself (a window host
  without the renderer cannot go below ~4.9 MiB on this toolchain), recorded rather than
  overclaimed.

### A4. Cold start — hold the win
- **Baseline [E-measured]:** 51.9 ms. **Target:** stay ≤ 55 ms after A1–A3 land; a regression
  here to pay for elsewhere is a defect.

### A5. Zero-copy GPU composition
- **Baseline:** pixels cross the CPU (~1 MB/frame at 640×480; not viable at 60 fps). `wry`'s
  public API has no shared-texture seam.
- **Target [E-target]:** a composition path where a GPU island's frame reaches the screen without
  a per-frame CPU readback — via a child surface, a shared texture, or a different port that
  exposes one. If the port cannot express it, say so in `known-gaps.md` and pick a port that can.
- **Evidence:** a benchmark asserting island frame *time* is independent of island *area*
  (the analogue of the existing `a_window_ignores_the_size_of_the_collection` test).

---

## 3. Goal B — full native capability parity

A page must be able to do everything Electron and Tauri 2 pages can, behind the same
capability-gated seam, with nothing recorded "absent."

### B1. Fix the three absences first
1. **Native menus (Windows).** Today blocked by `muda`'s `pub unsafe fn init_for_hwnd` vs the
   workspace-wide `unsafe_code="forbid"`. Resolution is a **project decision**, not a code task:
   either (a) a windowing crate with a safe menu seam, or (b) a single, audited, `#[allow(unsafe_code)]`
   module with a written ADR-012 exception and a `reason`. Do not lift the ban globally.
2. **File / colour / directory dialogs.** *File, save and folder dialogs are now done*: an
   `rfd`-based implementation behind the `dialogs` feature plus the `DialogBackend` seam, so the
   permission/option/path policy is tested headlessly and the platform adapter is thin
   (ADR-012's eighth exception). The one step still open is not code: the single real modal open
   needs a one-time human verification the registry does not model, recorded in
   `docs/known-gaps.md`. A **colour** dialog remains absent — `rfd` does not provide one, so it
   needs a separate dependency decision.
3. **Devtools on every platform.** `wry`'s Windows backend reports `is_devtools_open()==false`
   unconditionally and makes `close_devtools()` a no-op. Required: a devtools capability that
   (a) opens, (b) can be *observed* open, (c) closes — on Windows, Linux, macOS. If the engine
   cannot report state on a platform, implement the state tracking in the host so the host can
   always answer, and gate the whole thing behind a `devtools` feature that is **on in debug and
   off in release** by default.

### B2. Complete the native surface to Tauri-2/Electron parity
Everything below must exist as a capability-gated command with tests, or be recorded absent with
a reason in `known-gaps.md`. Each new dependency needs an ADR-012 exception.

- **Clipboard:** direct binding (e.g. `arboard`) so a call is microseconds, not a process spawn.
  Fallback to platform tools stays as a reported-unavailable path.
- **Notifications:** rich (actions, buttons, inline reply) where the platform allows, not just a
  title+body toast.
- **Auto-update, end to end:** check → notify → download → verify signature → apply → relaunch,
  with the user in the loop. Today `ncui update --from` exists but nothing is automatic and
  nothing is signed at the application level.
- **Signed application bundles.** Today only the `ncui` binary is signed; a bundle carries no
  signature of its own. Required: sign `bundle.json` + the installer; verify on install/update.
- **Per-machine install + MSI/package** on Windows; `.deb`/`.rpm`/AppImage on Linux; `.app`/`.dmg`
  + `Info.plist` scheme registration on macOS.
- **Process containment for detached children:** Windows Job Objects / Linux cgroups so a program
  that breaks away is still stopped at the time limit. Requires an FFI/dependency decision under
  ADR-012.
- **Global shortcuts, tray, deep links, single-instance, window state:** already present — keep
  them and carry them to macOS once B3 lands.
- **System integration:** default protocol/URL-scheme registration verified live, file-type
  associations, `open_with`, and a real `app.set_badge`/`set_progress` where the platform has one.

### B3. Make macOS a first-class platform
- **Blocker:** `tao`'s macOS backend requires the event loop on the main thread and offers no
  any-thread switch; this host creates it on a worker thread by design.
- **Required:** restructure the host so the engine loop owns the main thread on macOS (and the
  runtime's pump stays off it), then run the **full gate** on a real macOS host and flip
  `cross_platform.verification.macos` from `not_implemented` to `complete`. A platform is not
  "supported" until its gate has run on it.

### B4. Mobile (the axis NC-UI currently forfeits)
- Tauri 2 ships iOS/Android; Zero Native claims mobile. NC-UI has nothing.
- **Required:** a mobile host that reuses the same `ncui-webview` policy seam and the same page
  bridge over the platform WebView (WKWebView / Android WebView), with the capability model
  intact. Scope it as a new layer-3 adapter crate (`ncui-webview-mobile`) behind a feature, the
  way `ncui-webview-wry` is. This is the single largest new subsystem and should be its own ADR
  before any code.

---

## 4. Goal C — "all language support"

This goal is ambiguous, so it is stated as four separable requirements. All four are in scope.

### C1. Frontend languages/frameworks — already satisfied, keep it
A page is an ordinary web page; any framework, any language that compiles to JS/WASM. **Do not
regress this.** Add a `--template` generator (`ncui new`) that scaffolds a verified starter for
React, Svelte, Vue, Solid, vanilla-TS, and a WASM/Rust frontend, each a working `ncui run` app in
`examples/`.

### C2. A real bundler + dev server (the biggest day-to-day gap)
- **Done:** the asset server sends `ETag`/`Last-Modified` and answers `If-None-Match` with `304`;
  `ncui build` bundles the *built* output the manifest's `assets` field names, not raw source
  (verified by test); `ncui run --watch` reloads with the page's designated state kept; and `ncui
  new` scaffolds a runnable app in one command.
- **Remaining — the transform itself.** `ncui run --watch` still reloads what is on disk, so a
  TypeScript/JSX file is not compiled by it. Required: a first-class bundler integration
  (Vite/esbuild) with an official template and a documented `ncui run` ⇄ dev-server proxy, **or**
  a built-in transform layer. **This is an open project decision, not a code task:** a built-in
  Rust compiler (SWC — pure Rust, Apache-2.0, no Node) keeps the no-Node moat but adds a large
  (~40-crate) dependency that grows the audited surface ADR-012 protects; a dev-server proxy keeps
  the Rust surface minimal but makes TS/JSX apps require Node at dev time. The trade is recorded in
  `known-gaps.md` rather than made unilaterally.

### C3. Backend command languages — not Rust-only
- **Today: done — both halves.** A command a page calls can be answered by native Rust, by any
  executable (sidecar), or by the page's own JavaScript.
  - **any-language sidecar — done.** An application declares a sidecar in `ncui.app.json`
    (`sidecars: [{command, program, args, requires, timeout_ms, summary}]`). The command is
    answered by *any* executable over a one-line-JSON-each-way contract: the runtime writes the
    call payload to the program's stdin and reads one JSON answer from its stdout, so
    Go/Python/Zig/whatever backends work. It is capability-gated twice — the application must
    hold `process.spawn` for any sidecar to register at all, and each sidecar's `requires` must
    sit inside the page grant and gates the call at the seam (`describe` advertises it). It runs
    inside the `process` runner's containment (root-resolved program, secret-scrubbed
    environment, no shell, timeout, bounded output, process-tree stop, cwd pinned to the app
    root so a relative script path resolves next to the manifest). A non-zero exit or a
    non-JSON answer is surfaced to the page as an error rather than silently swallowed. The
    **component ABI** (already built) stays the first-class path for precompiled native modules
    in any AOT language that can emit WASM.
  - **JS/TS commands — done, via the webview-callback round trip (no new dependency).** An
    application declares a page command in `ncui.app.json`
    (`page_commands: [{command, requires, summary}]`); the page registers the function that
    answers it with `window.ncui.registerCommand(name, fn)`. Invoking the command through the
    usual `window.ncui.invoke` seam is gated exactly like a native one — the command must be
    declared and the page must hold `requires`, or the seam refuses it and the page's own
    promise rejects. Because `wry` exposes no way to read a value back out of
    `evaluate_script`, the runtime does not answer the call directly: it holds the page's
    original invoke open, evaluates `window.ncui.__runCommand(replyId, name, payload)` in the
    page, the handler runs and posts its answer back over the same bridge under `replyId`, and
    the runtime settles the original invoke with it. A handler that is missing or throws is
    reported to the page as an error rather than left to time out. A command declared but never
    handed a handler answers with an error rather than running anything. A component cannot
    reach a page command — only a page can, which is what keeps page code from being driven by
    a component.

### C4. UI languages / i18n / accessibility of text
- **Required:** the runtime must not assume Latin text or LTR.
  - GPU island text: shaping goes through `rustybuzz` (HarfBuzz), and the row/text path is extended
    to **RTL, bidi runs, and scripts with joining**, verified by tests (see the measured status
    below). The "a row taller than its text is clipped, not scrolled/elided" gap —
    `completion-audit.md` §9.5 — is **fixed**: overflowing rows are elided with a shaped `…`.
  - Bridge/`IslandDescriptor` must carry locale + text direction so a screen reader is told the
    language and direction.
  - App manifests declare a default locale; the runtime exposes the platform locale to the page.
- **Measured status [E-measured] — the non-GPU half is shipped.** `IslandDescriptor` carries
  `locale` + `direction`; manifests declare a validated `default_locale`; and `describe` answers
  with the platform `locale` and a `text_direction` derived from it (so the two cannot disagree),
  delivered to the page on the window seam. Direction is derived by `TextDirection::for_locale`
  against a fixed right-to-left script set; **~33 ns** per derivation (`locale/direction_for_locale`
  in `ncui-bench`), negligible on the describe/island path. The platform locale is read from
  `LC_ALL`/`LANG`; Windows' `GetUserDefaultLocaleName` needs an FFI reader that does not exist, so
  such a platform reports `"und"` rather than a wrong locale (an ADR-012-scoped addition when
  built). The **GPU clipped-row gap is fixed.** A block of wrapped lines taller than its row is
  now *elided, not clipped*: `fit_lines` draws only the lines that fit and `elide_line` truncates
  the last visible one to make room for a shaped `…` (`U+2026`), so a reader sees the text
  continues rather than a line sliced mid-glyph; a row too short for even one line still draws its
  first (clipped, as always) rather than going blank. The decision lives in
  `ncui-gpu-renderer`'s `text` module next to the wrapping it sits on, is unit-tested over
  synthetic lines (no font or GPU needed), and is measured — `fit_lines` **~3.2–3.4 ns** either way
  (a block that fits and one that overflows cost the same: the decision is O(1)) and `elide_line`
  **~99 ns** on the overflowing rows only (`island/*` in `ncui-bench`) — negligible against the
  GPU render and the megabyte readback a frame already costs. An end-to-end test drives a real
  overflowing row through the island route on a real GPU and asserts the frame stays well formed
  with nothing drawn below the row. **The GPU RTL/bidi/joining half is now shipped too.** The
  shaping core (`rustybuzz`) always reordered a run that is all right-to-left, but a *mixed*-direction
  line was shaped in its first strong direction with the other runs left in logical order — so a
  Hebrew word inside an English sentence drew backwards. Every line is now split into
  single-direction runs first (`bidi_runs`) and each run shaped with its own direction, then the runs
  concatenated in reading order with clusters rebased onto the whole line. Pure right-to-left runs,
  an embedded right-to-left word, two right-to-left words keeping their phrase order, and Arabic
  letters taking their joined (not isolated) forms are all unit-tested on the system font; a real
  Hebrew and a real Arabic row are driven through the island route on a real GPU end to end. Measured:
  the split costs **~672 ns** for a plain left-to-right line and **~818 ns** for a mixed one
  (`bidi/*` in `ncui-bench`) — sub-microsecond against the shaping and the GPU render it precedes.
  The splitter is a *bounded* bidirectional algorithm, not all of UAX #9 — no directional marks, no
  paired-bracket resolution, and a number follows its run's direction — and `docs/known-gaps.md`
  records those limits. What is *not* done: Latin ligature substitution is the font's own default
  feature, applied by the shaper when the font has it, not something this code adds.

---

## 5. Goal D — developer experience that rivals "instant rebuild"

Zero Native's headline is instant rebuilds. NC-UI must match the *experience*, not the marketing.

- **D1.** *Done.* Sub-second page reload with **state preserved** where the app asks for it: the
  page designates its state on `window.__ncui_state`, the host snapshots it to `sessionStorage`
  before reloading, and an opt-in bridge script restores it after. State the page does not
  designate is not preserved — a reload tears down the JS heap, which is what a reload is — so
  this is one well-defined slot the app opts into, stated rather than overstated.
- **D2.** *Done.* The asset server sends `ETag`/`Last-Modified` and answers conditional `GET` with
  `304`, so a `--watch` reload re-sends nothing unchanged (`app.asset_conditional_304` in the
  latency harness).
- **D3.** Fast native-side iteration: document and template the fastest Rust-rebuild path
  (`cargo-watch` + a shared/fast linker), and, where a native command changes, hot-reload the
  *page* without rebuilding the host.
- **D4.** Hot-reload of WASM components without an app restart (the component host already loads
  them; add reload-on-change).
- **D5.** *Done.* `ncui new <directory>` scaffolds a runnable app in one command — manifest, entry
  page and script — granting the smallest capability set that opens a window, including the reload
  bridge so the dev loop the runtime ships is the one the template demonstrates. It refuses to
  scaffold over an existing non-empty directory.
- **Evidence [E-target]:** a benchmark measuring **edit → visible change** (page-only) and
  **edit → visible change** (native command) in milliseconds, committed beside the latency suite.

---

## 6. Goal E — security: keep the lead, close the two soft spots

NC-UI's verified safety posture (`unsafe_code="forbid"`, clippy pedantic, fuzzing, property
tests, capability model) is its **moat**. Zero Native being Zig means (by design) no borrow
checker — do not trade the moat away for build speed.

- **E1. Unforgeable consent.** Replace the plain `bool` consent with a token the runtime mints at
  the point of user confirmation. Requires the runtime UI surface that does not exist yet (see
  `known-gaps.md`). Until it exists, the gate must keep failing closed.
- **E2. Signed, integrity-checked bundles** (see B2). An installed app should be a signed
  artefact, not just a digest in a release doc.
- **E3. Keep** `unsafe_code="forbid"` workspace-wide. Any new FFI (Job Objects, dialogs, menus) is
  a scoped, audited, ADR-012-exceptioned module with a written `reason` — never a global lift.
- **E4.** Every new untrusted parser (bundle, update document, dialog results, sidecar framing)
  gets a property test + a fuzz target, as the existing ones do.

---

## 7. Goal F — the benchmark gate

"Passes every benchmark" is made concrete here. Two things must be true:

### F1. NC-UI wins or ties every comparison row, every platform
Extend `comparison/bench/compare.py` to build and measure **four** sides: Electron, Tauri 2
(current, not 1.2), NC-UI, and Zero Native (clone + port, same as the others). Run on **Windows,
Linux, and macOS** once B3 lands. Report the full table. NC-UI must be **best or tied** on:
cold start, idle CPU, IPC round trip, and — target after A2/A3 — memory and size. Where it is
not, `comparison/README.md` carries the cause and the plan, exactly as it does now.

**Measured status on Windows (the one host reproducible now):** the Electron / Tauri / NC-UI rows
are measured by the harness. The **Tauri side is now Tauri 2**, the current major, ported from the
1.2 the app was originally cloned at (the menu API, the capabilities model, and the page `invoke`
module all changed, so it is a port of the comparison app, not a version bump; the port is carried
by `bench/instrumentation/tauri/` and reproduces from a fresh clone). The **Electron leg and the
Tauri 2 leg were each re-run end to end on 2026-11-14** (`python comparison/bench/compare.py`,
repeats 5, rounds 200). It reproduces: cold start p50 **51.6 ms** for the NC-UI Electron port
against Electron's **261.1 ms**; the Tauri legs are level (Tauri 2 **52.9 ms**, NC-UI **55.1 ms**);
idle CPU **0.00 %** throughout; distributed size **11.4 MiB** against Electron's **356.8 MiB**
(Tauri 2 **8.5 MiB**, smaller). One verdict changed when the Tauri side moved to the current major,
and re-running is what surfaced it: **Tauri 2's own IPC is ~6× slower than Tauri 1.2's**
(**1.398 ms** against the 0.252 ms measured before the upgrade, reproduced across two independent
runs), which moves the IPC row against Tauri from a Tauri win to an NC-UI win (NC-UI **0.348 ms**).
NC-UI's own IPC figure did not move — the same **0.31–0.37 ms** it measured against Electron and
against Tauri 1.2 — which is the check that the row moved because Tauri's bridge changed, not
because the measurement did. The rows where NC-UI is *not* best reproduce too, with their causes
in `comparison/README.md`: IPC against **Electron** (**0.210 ms**, the double webview-boundary
crossing plus thread hop) and idle memory (NC-UI **330–331 MiB** against Electron's **291.8 MiB**,
the WebView2 runtime's own browser process). The fresh tables are recorded in
`comparison/README.md` under "Reproducing these numbers". `build_ncui()` builds the
renderer-carrying `wry-host-gpu` default, not the `minimal` profile A3 split off, so the size row
cannot silently drift to ~4.9 MiB. Still open on F1: the **Zero Native** fourth side (no verified
upstream in this repo, so its figures stay `[E-claimed]`) and the **Linux/macOS** legs (B3).

### F2. The internal regression gate becomes enforcing
- **Baseline [E-measured]:** the latency gate reports but does not fail by default, because the
  minimum-of-9 spread on a workstation is too noisy (`known-gaps.md`).
- **Required:** record baselines on **dedicated, quiet CI runners**, re-measure the spread, and
  if it stays under ~1.5×, flip `--enforce` on in `.github/workflows/ci.yml`. A dedicated runner
  is the precondition; do not enable enforcement on a noisy workstation.
- Add to the enforced set: **edit→visible-change** (D), **island frame time vs area** (A5), and
  **bundle/install/update time** (B2).

---

## 8. Non-negotiable constraints (unchanged)

These are why the project is trustworthy; none of the goals above may violate them.

1. **No mock / stub / TODO / placeholder / MVP.** Every capability is real, run, and evidenced.
2. **A feature is not "supported" on a platform until its gate has run on that platform.**
   macOS and mobile count only when their gates are green on real hardware.
3. **No blind deletion;** every removal has a reason, evidence, replacement and rollback.
4. **Append-only changelog;** every change logged with why/how/evidence.
5. **Warnings are defects;** `unsafe_code="forbid"` holds; clippy pedantic `-D warnings`.
6. **Generated artifacts come from checked-in generators** that a policy gate verifies.

---

## 9. Suggested sequencing (each phase is independently shippable)

1. **Phase 1 — the three absences** (B1: menus, dialogs, devtools). Smallest scope, highest
   user-visible parity gain, all Windows-first. *Dialogs (file/save/folder) are now closed*; the
   open remainder is menus (Windows) and devtools (every platform), plus the colour dialog.
2. **Phase 2 — the dev loop** (C2 bundler + ETag, D1/D2 reload-with-state). Removes the biggest
   "can't build a real app with this" blocker.
3. **Phase 3 — performance** (A1 binary bridge, A2 memory, A3 size profiles). Attack IPC first;
   it's the most tractable measured gap.
4. **Phase 4 — macOS** (B3). Unlocks the third desktop platform and the macOS leg of F1.
5. **Phase 5 — command-language breadth** (C3, *done*: sidecar + JS commands) + **consent token** (E1).
6. **Phase 6 — mobile** (B4). Largest, last, own ADR.
7. **Phase 7 — enforcement** (F2) once dedicated runners exist.

---

## 10. Definition of done (for the whole prompt)

- [ ] Every row of the four-way comparison table: NC-UI best or tied, on Windows, Linux, macOS.
- [ ] Registry reports **0 `not_implemented`** — macOS verified, and every capability present.
- [ ] No "absent" left in `known-gaps.md` without a reason; the absences (menus, devtools,
      colour dialog) closed. File/save/folder dialogs are done; their one open item is the
      human-verified modal open recorded in `known-gaps.md`.
- [ ] Mobile host exists and its gate runs on real iOS + Android hardware.
- [ ] Bundler + `ncui new` templates land; a TS app builds, runs and reloads with state.
- [x] Sidecar command registration works, capability-gated (done — any-language stdin/stdout
      JSON contract, `process.spawn`-gated, contained by the `process` runner).
- [x] JS/TS page-registered command handlers work, capability-gated (done — the page declares a
      command and registers a handler with `registerCommand`; the runtime gates it at the seam and
      answers the page's own invoke over the webview-callback round trip, no new dependency).
- [x] i18n/RTL/bidi verified by tests in the GPU text path.
      *(All three parts shipped. Non-GPU: `IslandDescriptor` locale+direction, manifest
      `default_locale`, and `describe` exposing the platform locale + derived text direction to the
      page. GPU clipped-row: overflowing island rows are elided with a shaped `…`, unit-tested over
      synthetic lines and end-to-end on a real GPU, measured at ~3 ns/row for the decision and
      ~99 ns to mark an overflowing row. GPU RTL/bidi/joining: every line is split into
      single-direction runs and each shaped with its own direction, so a pure right-to-left run, an
      embedded right-to-left word, two right-to-left words keeping their phrase order, and Arabic
      letters taking their joined forms are unit-tested, and real Hebrew and Arabic rows are driven
      through the island route on a real GPU — measured at ~672 ns to split a left-to-right line and
      ~818 ns a mixed one. The splitter is a bounded bidirectional algorithm, not all of UAX #9; its
      limits (no directional marks, no bracket pairing, numbers follow their run) are recorded in
      `docs/known-gaps.md`. Latin ligature substitution is the font's own feature, not this code's.)*
- [ ] Consent is an unforgeable token; bundles are signed.
- [ ] Internal benchmark gate is **enforcing** on dedicated runners.
- [ ] Full gate green on Windows, Linux, macOS (+ mobile where applicable); changelog appended.
