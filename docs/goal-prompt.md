# NC-UI Goal Prompt — best-in-class against Electron, Tauri 2, and Zero Native

**Status:** Draft goal specification.
**Baseline:** the tree as measured in `docs/completion-audit.md` (2026-11-24) and `docs/known-gaps.md`.
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

From `comparison/bench/results/windows.json`, Windows x86_64, 2026-11-24, 5 repeats and
200 IPC rounds per repeat, median:

| Metric | Electron | NC-UI (Electron port) | Tauri 2 | NC-UI (Tauri port) | Zero Native |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cold start to window | 276.5 ms | **60.4 ms** | 64.0 ms | **60.7 ms** | [E-claimed] tiny/instant |
| Idle working set | **304.6 MiB** | 361.9 MiB | **349.9 MiB** | 362.5 MiB | [E-claimed] |
| Idle CPU | **0.00 %** | **0.00 %** | **0.00 %** | **0.00 %** | — |
| IPC round trip | **0.196 ms** | 0.351 ms | 1.418 ms | **0.410 ms** | — |
| Distributed size | 356.8 MiB | **12.3 MiB** | **8.5 MiB** | 12.3 MiB | [E-claimed] tiny |

**Structural gaps that are not numbers:**

- The macOS host path is written but **unverified**; ADR-018 records the main-thread event-loop
  blocker, and macOS is deliberately deferred while Windows and Linux are completed.
- **No mobile story at all.** Tauri 2 ships iOS/Android; Zero Native claims mobile.
- Native menus are **implemented and verified on Windows and Linux** through `muda` (ADR-012's
  ninth exception); macOS is unverified. DevTools open/close are implemented, with Windows open
  state honestly reported as unknown. File/save/folder dialogs are implemented behind `dialogs`
  (`rfd`, ADR-012's eighth exception), except the single real modal open, which needs a one-time
  human verification; the colour dialog is implemented on Windows through the platform tool.
- The page bridge has a JSON control path **and a bounded raw `POST` data channel** for large
  payloads. The WebView reply still crosses as evaluated script, so the small-call IPC row is
  unchanged by the binary data channel.
- Dispatched destructive methods and native page commands require exact host approval. The
  runtime validates subject-, method-, capability- and parameter-bound consume-once tokens, and
  the desktop CLI installs a native confirmation dialog before the first window opens. The
  legacy page `consent: true` path is gone; real dialog acceptance remains platform evidence.
- **No bundler / transform layer.** The asset server serves files from disk and transforms
  nothing, so a TypeScript/JSX app must compile its own output first. What *is* built around the
  dev loop: `ncui new` scaffolds a runnable app in one command; `ncui run --watch` reloads the page
  **with its designated state kept** (the page opts in via one bridge script); the asset server
  sends `ETag`/`Last-Modified` and answers conditional `GET` with `304`; and `ncui build` bundles
  the built-output directory the manifest's `assets` field names, not raw source. The remaining
  gap is the transform itself (built-in Rust compiler vs. a dev-server proxy), which trades against
  the no-Node moat and is recorded as an open decision.
- Native commands are **Rust-only**; a JS-only app cannot register one.
- Release binary is reproducible for a fixed build directory; the cross-directory `OUT_DIR`
  limitation is recorded, and no signing key is configured on this machine.

---

## 2. Goal A — close every measured performance gap

NC-UI must win or tie on **every** row of the comparison table, on **every** platform it
claims, not just cold-start.

### A1. IPC round trip — beat 0.196 ms (Electron)
- **Baseline [E-measured]:** 0.351 ms for the Electron port and 0.410 ms for the Tauri port in
  the 2026-11-24 comparison. The cause is measured, not inferred: the in-process
  half (capability check, handler, response serialisation) is a p99 of **900 ns**, so **99.7 %
  of the round trip is the WebView boundary** — the page's `postMessage` in, the host's
  `evaluate_script` out, and a thread hop because the host's event loop runs on its own thread
  (`comparison/README.md`). `wry` exposes no `PostWebMessageAsJson` seam, so the reply can only
  be delivered by evaluating a script string.
- **Correction to an earlier draft of this goal (measure-don't-trust):** a *binary* bridge
  does **not** close the small-round-trip gap. A binary transport can carry the page→host
  *request*, but the host→page *reply* must still cross as `evaluate_script(&str)`, so the
  boundary hop — about 99.8 % of the current cost — remains. A binary path is worth building for
  **large-payload throughput** (an app moving a big buffer pays JSON's cost twice today), not
  for the 200-call small-payload latency this row measures. Chasing ≤ 0.196 ms here with a
  binary bridge would be optimising the 0.3 %.
- **What would actually move this row [E-target]:** remove a boundary crossing or the thread
  hop, not the encoding. Concretely: (a) a `PostWebMessageAsJson`-equivalent seam in the port
  (likely means a `wry` upstream change or a different port), or (b) co-locate the host's
  reply path with the page's event loop to remove the thread hop, or (c) accept the boundary
  cost and stop counting it as a defect — Electron's direct C++ IPC is a different architecture,
  and Tauri 2 (1.452 ms in the current reproduction) pays the *same* WebView2 boundary. Against
  Tauri 2, NC-UI's 0.436 ms already wins; Electron remains the faster row at 0.278 ms against
  NC-UI's 0.363 ms.
- **Reframed target:** keep the binary bridge as a *throughput* goal (large payloads), measured
  separately with a payload-size sweep; do not book it against the ≤ 0.19 ms small-round-trip
  target, which the boundary — not the encoding — owns.
- **Measured status [E-measured] — the binary *data* channel is shipped.** The asset server now
  answers `POST`: it reads the request body (bounded, 413 above `MAX_REQUEST_BODY_BYTES`) and
  hands the bytes to a route, so a page moves a large buffer raw instead of JSON-encoding it.
  `binary_channel/round_trip` in `ncui-bench` sweeps 1 KiB–1 MiB against a real listener over a
  real socket: ~150 µs fixed per round trip, throughput climbing to ~176 MiB/s at 64 KiB. The
  WebView *boundary* hop (the 99.7 % that owns the ≤ 0.196 ms row) is untouched by design — this
  channel is the throughput half, measured on its own. Evidence: `changelog/unreleased/` binary
  POST channel fragment; `ncui-app` asset tests (byte-for-byte POST echo over a real socket).
- **Evidence:** `comparison/README.md` IPC analysis; `comparison/bench/compare.py` row; the
  900 ns in-process p99 in the latency harness (`runtime.page_command_dispatch`).

### A2. Idle working set — beat 304.6 MiB (Electron)
- **Baseline [E-measured]:** 361.9 MiB for the Electron port and 362.5 MiB for the Tauri port in
  the current comparison. An earlier minimal scaffolded-app decomposition measured 317.4 MiB.
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

### A3. Distributed size — approach 8.5 MiB (Tauri 2)
- **Baseline [E-measured]:** 12.3 MiB for both ports in the current comparison. An earlier
  isolated build measured the GPU island renderer at about 6.4 MiB.
- **Target [E-target]:** a `ncui build --profile minimal` that ships a window host **without**
  the GPU renderer for apps that do not use islands, landing ≤ 5 MiB; keep the full build for
  apps that do.
- **Done [E-measured].** `ncui build --profile {standard,minimal}` compiles the named profile's
  own runtime and bundles it. `standard` (the default) is the full build — window host plus the
  GPU-island renderer; `minimal` is the window host alone, which links out `wgpu` and the text
  shaper. The renderer became genuinely optional at link time: `wry-host` no longer implies
  `gpu-islands` (that implication was what forced 6.4 MiB into every window host), so a profile
  can choose. Measured earlier on the counter example: standard **11.292 MiB**, minimal
  **4.902 MiB** (binary 4.872 MiB) — **≤ 5 MiB, target met then**. The current standard build is
  12.3 MiB after native-menu and other runtime work; the minimal profile needs a fresh measurement.
  The minimal bundle was launched and opened a
  real window (E2E). A page that asks for an island on `minimal` gets the asset server's 404 —
  the island route is `#[cfg(feature = "gpu-islands")]`-gated — which the counter example already
  renders as "No island on this machine" rather than a failure.
- **Constraint honoured:** the standard build is byte-for-byte the size it was (11.292 MiB), so
  nothing was removed — the heavy subsystem was made *optional at link time*, as the constraint
  requires. The remaining gap to Tauri 2's 8.5 MiB is the WebView2 host itself (a window host
  without the renderer cannot go below ~4.9 MiB on this toolchain), recorded rather than
  overclaimed.

### A4. Cold start — hold the win
- **Baseline [E-measured]:** 55.1 ms for the Tauri port (51.9 ms for the Electron port).
  **Target:** stay ≤ 55 ms after A1–A3 land; a regression
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

### B1. Close the former native-surface absences

1. **Native menus — done on Windows and Linux.** Manifest-declared nested menus attach through
   `muda`, route by per-window command id and deliver selections to the page. The single Windows
   `init_for_hwnd` call is confined to the `native-menu` module under ADR-012's ninth exception;
   the workspace ban remains in force everywhere else. macOS's safe adapter path is unverified.
2. **File / colour / directory dialogs — implemented.** File, save and folder dialogs use `rfd`
   behind `dialogs`; the colour dialog uses the Windows platform tool. Request validation and
   path policy are tested headlessly. The one open evidence item is a person completing a real
   modal selection, recorded in `docs/known-gaps.md` rather than claimed by a test.
3. **Devtools — partial.** Open and close are implemented; Windows cannot report the open state
   and answers `unknown` honestly. A future host-side state tracker may close that observation
   gap without inventing a platform answer.

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
  before any code. iOS is an explicit evidence target, not a compile-only target.

### B5. Browser/WebGPU target breadth — distinct from hosting web pages
- NC-UI already hosts web-authored pages in native windows; that is **not** a browser target.
  The additional goal is a shell where nc-ui-native rendering and input run in a real browser
  through WebGPU, with canvas/rAF/ResizeObserver, clipboard, text services, lifecycle,
  accessibility and packaging treated as platform work.
- **Required:** a target-contract ADR separating (1) web-authored pages in native windows,
  (2) nc-ui-native content in a browser/WebGPU shell, and (3) nc-ui-native content on iOS.
  No target is supported because it compiles; it is supported when its real-browser or
  real-device gate runs. This broadens the target matrix; it does not remove B4's existing
  Android requirement.

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
  - Native text editing is part of language support: nc-ui-kit/native text fields must implement
    gpui's `InputHandler` contract rather than key-character-only editing — marked text,
    candidate placement, commit/cancel, selection, UTF-8↔UTF-16 range mapping, and rich-text
    spans that survive editing.
  - Links inside native text runs must be keyboard-operable focus targets. gpui 0.2.2
    `InteractiveText` hit-tests click/hover ranges but has no focus API, so the runtime/kit
    text contract must add per-link Tab/Shift+Tab focus, Enter/Space activation, Menu/Shift+F10
    secondary actions, and accessibility bounds/state.
  - Native surfaces need an AccessKit-backed accessibility tree (or an explicitly justified
    equivalent) rather than only the island JSON mirror; platform assistive technology must see
    controls, text, links and focus state.
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

NC-UI's verified safety posture (`unsafe_code="forbid"` except one audited package, clippy
pedantic, fuzzing, property tests, capability model) is its **moat**. Zero Native being Zig means
(by design) no borrow checker — do not trade the moat away for build speed.

- **E1. Unforgeable consent — enforcement shipped, native acceptance evidence remains.**
  Dispatched destructive methods and native page commands require runtime-minted tokens bound
  to subject, method, capability, correlation and exact parameters, and spend them once. The
  desktop CLI installs a real native confirmation surface before the first window opens; only
  explicit approval mints and immediately spends a token. The page boolean is removed, and
  `known-gaps.md` records the remaining per-platform dialog acceptance evidence.
- **E2. Signed, integrity-checked bundles** (see B2). An installed app should be a signed
  artefact, not just a digest in a release doc.
- **E3. Keep** `unsafe_code="forbid"` workspace-wide except ADR-012's single package-scoped
  Windows menu attachment. Any new FFI (Job Objects, dialogs, menus) is a scoped, audited,
  ADR-012-exceptioned module with a written `reason` — never a global lift.
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

**Measured status on Windows:** the Electron / Tauri 2 / NC-UI rows are measured by the harness.
The **Tauri side is Tauri 2**, the current major, ported from the 1.2 the app was originally cloned
at (the menu API, the capabilities model, and the page `invoke` module all changed, so it is a port
of the comparison app, not a version bump; the port is carried by
`bench/instrumentation/tauri/` and reproduces from a fresh clone). The 2026-11-24 fail-closed
five-repeat, 200-round run gives cold start p50 **61.2 ms** for the NC-UI Electron port against
Electron's **268.5 ms**, and **59.8 ms** for the NC-UI Tauri port against Tauri 2's **140.0 ms**.
Idle framework-process CPU is **0.00 %** throughout. Distributed size is **12.3 MiB** against
Electron's **356.8 MiB**, and **12.3 MiB** against Tauri 2's **8.5 MiB**. Tauri 2's IPC is
**1.452 ms** against NC-UI's **0.436 ms**; Electron remains faster at **0.278 ms** against
NC-UI's **0.363 ms**. Idle memory is **332.5 MiB** against Electron's **298.0 MiB**, and
**332.6 MiB** against Tauri 2's **320.5 MiB**. The current tables are at the top of
`comparison/README.md`. `build_ncui()` builds the renderer-carrying default, not the `minimal`
profile A3 split off, so the size row cannot silently drift to an older ~4.9 MiB figure. Still
open on F1: the **Zero Native** fourth side (no verified upstream in this repo, so its figures
stay `[E-claimed]`) and the **Linux/macOS** legs (B3).

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

1. **Phase 1 — the former native-surface absences** (B1: menus, dialogs, devtools). *Native
   menus and file/save/folder/colour dialogs are delivered on their verified platforms*; the
   remaining evidence item is a human completing a real modal selection, and DevTools open-state
   observation on Windows is partial.
2. **Phase 2 — the dev loop** (C2 bundler + ETag, D1/D2 reload-with-state). Removes the biggest
   "can't build a real app with this" blocker.
3. **Phase 3 — performance** (A1 binary bridge, A2 memory, A3 size profiles). Attack IPC first;
   it's the most tractable measured gap.
4. **Phase 4 — macOS** (B3). Unlocks the third desktop platform and the macOS leg of F1.
5. **Phase 5 — command-language breadth** (C3, *done*: sidecar + JS commands) + **consent enforcement**
   (E1, *implemented*; native dialog acceptance evidence remains).
6. **Phase 6 — mobile + browser/WebGPU target shells** (B4+B5). Largest, last, own ADR.
7. **Phase 7 — native interaction correctness** (AccessKit, IME/rich text, per-link keyboard
   focus). Parallel-safe after the native host proof, but blocking any claim that native views
   are first-class application UI.
8. **Phase 8 — enforcement** (F2) once dedicated runners exist.

---

## 10. Definition of done (for the whole prompt)

- [ ] Every row of the four-way comparison table: NC-UI best or tied, on Windows, Linux, macOS.
- [ ] Registry reports **0 `not_implemented`** — macOS verified, and every capability present.
- [ ] No "absent" left in `known-gaps.md` without a reason. Native menus and file/save/folder/
      colour dialogs are delivered on their verified platforms; the dialogs' one open item is the
      human-verified modal open recorded in `known-gaps.md`, and Windows DevTools open-state
      observation remains partial.
- [ ] Mobile host exists and its gate runs on real iOS + Android hardware.
- [ ] Browser/WebGPU target shell exists and its gate runs in real browsers.
- [ ] Native text/a11y correctness exists: AccessKit-backed native trees, live IME composition
      in a rich-text field, and keyboard-focusable links inside text runs.
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
      *(Consent shipped: runtime-minted tokens bound to subject, method, capability and exact
      parameters, spent once; prompts are asked with no runtime lock held. The signing pipeline
      exists — `bundle.json` digests verified on install/update, detached release signing in the
      supply-chain leg — but no signing key is configured on this machine, so the signature step
      is built, not exercised.)*
- [ ] Internal benchmark gate is **enforcing** on dedicated runners.
- [ ] Full gate green on Windows, Linux, macOS (+ mobile where applicable); changelog appended.
