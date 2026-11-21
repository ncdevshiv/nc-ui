- **Drag regions reach the engine.** A mousedown on a `[data-ncui-drag-region]` element now
actually moves an undecorated window. Two defects had left the feature dead since it landed:
the injected script called the action `window.drag_region` while the seam declares
`drag_region`, so the page's request was refused before it reached anything, and the
`ncui.window.drag.region` handler answered `{"dragging": true}` without contacting the engine
(`let _ = runtime;`) — a success nothing felt. The script now calls the declared name, and the
handler asks the calling window's own engine (`Windows::drag_window` → `WebViewBackend::drag_window`
→ `tao::window::Window::drag_window`), reporting a refusal as a refusal: a window that is not
open is `not_found`, an engine that cannot drag is `conflict`.

Tested: the page-bridge e2e drives a real `perform` message for `drag_region` through the seam
and asserts the engine recorded the drag; the registry-level test asserts an unknown window is
refused rather than answered with a phantom drag.

- **Application events are honest, broadcast, and callable from Rust.** `ncui.app.emit` sat on
a silent drop: a failed delivery was swallowed into `emitted: false`, there was no way to reach
every window, and an embedding application had no Rust API at all — only the JSON-RPC method.
Now `Runtime::emit(window, event, payload)` and `Runtime::emit_all(event, payload)` are the one
delivery path (the JSON-RPC method wraps them), a failed delivery is an error naming the
window rather than a success the page never received, and omitting `window` (or passing
`"all"`) broadcasts to every open window, returning the count that was reached. Event names
keep the 256-byte bound in one place shared by both callers.

Tested: `Runtime::emit` delivers a `__dispatch` script a mock page receives (page-bridge e2e);
an emit to an unknown window is refused; a broadcast with no windows answers `emitted: 0`.

- **`ncui.cdp.send` tells the truth.** The handler evaluated
`window.ncui.__cdpSend(...)` in the target page while `bridge.js` defined no `__cdpSend`, and
answered `{"sent": true}` regardless — a delivery report for a script that could never do
anything. The bridge now defines `__cdpSend`: it records the command in a bounded
`window.__ncuiCdpLog` ring and re-dispatches it to `ncui.on("ncui:cdp", ...)` listeners, so a
page that wants a CDP relay implements one where its own debugging endpoint lives. The handler
now requires `window` and `method` (no silent defaults to window `…0001` and `Page.navigate`),
bounds the method name, and reports a window that is not open as `not_found` and an engine
that refuses the script as `conflict`. The host-side CDP transport itself remains ADR-017
Phase 2 and is recorded as such in `docs/known-gaps.md`.

Tested: e2e refusals for an unknown window and for missing `window`/`method`; the dispatch
sweep exercises the consent-gated call against a live window.

- **The macOS host runs the engine loop on the main thread (ADR-018).** `tao`'s AppKit backend
panics unless the event loop is created on the main thread and offers no any-thread switch, so
NC-UI could not launch on macOS at all. On macOS the host now exchanges the two threads the
other platforms use: `run_on_main_thread` builds the loop on the main thread, installs its
proxy for every caller, and runs the application — the runtime pump that opens windows and
serves pages — on a worker that reaches the loop through that proxy. When the application
finishes, the loop is asked to quit with the code the application returned, via a new
`HostEvent::Quit` handled with `ControlFlow::ExitWithCode`. `ncui`'s `main` wires this on macOS
(`wry-host` builds); Windows and Linux paths are unchanged.

Recorded rather than claimed: the macOS path is written and reviewed, but this machine is a
Windows machine — cross-compiling the check for the macOS target stops in
`objc2-exception-helper`'s build script, which needs a C compiler for the Apple target that
does not exist here, so even target compilation was **not** verified, and no macOS machine ran
the gate. A platform is not "supported" until its gate has run on it, so
`cross_platform.verification.macos` stays `not_implemented` until a macOS run says otherwise.

- **The capability name table includes `cdp`.** The workspace gate caught `Capability::Cdp`
having been added to the enum, the wire mapping and the scope predicate without the static
diagnostics table (`CAPABILITY_NAMES`) being extended — `string_mapping_is_total_and_injective`
failed at 22 names against 23 variants. Fixed by adding `"cdp"`; the invariant test now passes.

- **The first CI run on Linux and macOS surfaced what a Windows-only gate cannot see.** Four
defect classes, all real, none visible from this Windows machine:

  - `ncui-webview-wry`'s macOS `run_on_main_thread` moved the loop's proxy into the
  quit-joiner and then handed the same proxy to `run_engine` — a use-after-move (E0382) only
  a macOS compile sees. The joiner now clones its own handle.
  - `ncui-native`'s deep-link helpers (`run_command` import, `tool_error`, `first_line`) are
  used only by the Windows/Linux tool paths, and the macOS notifier arm uses none of the
  notifier's state; on macOS they were dead code, and `-D warnings` makes dead code a compile
  error. The helpers are now compiled only where a tool runs, and the macOS arm carries the
  reason it does not use `self`.
  - `ncui-gpu-renderer`'s window-surface test imports tao's Windows-only event-loop extension
  unconditionally, so it did not compile on Linux. It is now declared the Windows composition
  probe it always was and compiled out elsewhere.
  - The `rust` and `release` jobs never installed the Linux GTK/WebKit libraries the
  workspace's default features link, so `glib-sys`'s build script failed before any gate could
  run. Both jobs now install the same set the `adapters` job does.

Tested: the gate itself is the test — these are the failures it reported, each fixed at the
site that owns it; the Windows gates (fmt, clippy `-D warnings`, workspace suite) re-run green
locally, and the Linux/macOS legs are exercised by CI on this push.

- **Three more first-run CI findings, fixed at their owners.** The first run also reached the
Windows legs, which failed in ways a local Windows machine does not reproduce:

  - `notify::tests::a_real_notification_is_accepted_by_the_platform` failed on the runner with
  `SQLITE_CONSTRAINT_UNIQUE`: the toast tag was `ncui-{pid}-{count}`, and Windows' notification
  store persists per user across processes while process identifiers are reused — a tag a
  previous job raised was still in the store. The tag now carries a wall-clock component
  (`ncui-{nanos:x}-{count}`), which another run cannot collide with.
  - The comparison harness's Tauri leg floated both sides (`"2"` in Cargo.toml, `^2.0.0` in
  package.json), so npm and cargo resolved different minors and the Tauri CLI refused the build
  ("version mismatched Tauri packages"). Both sides are now pinned to the same minor: the CLI
  and API at `2.12.4`, the crates at `=2.12.5` / `=2.7.4`.
  - The release job's reproducibility check reported different digests for two builds —
  diagnosed and fixed below in this fragment (the check now rebuilds honestly in a cleaned
  tree).

Tested: `cargo test -p ncui-native --lib` on this Windows machine (the notification test raises
a real toast through PowerShell); the Tauri pins are the versions the committed comparison
report was measured with.

- **The doc steps surfaced two rustdoc lint classes of their own.** `AppKit` in the
macOS-only doc comments trips the missing-backticks lint (the engine proxy, the macOS host
entry point, and the runtime's re-export), and `devtools.rs` linked `[tree]`, `[events]` and
`[profile]` — names that resolve to nothing. The events and profile links now point at the
`DevTools` methods that exist, and the snapshot link points at `DevTools::summary`.

Tested: `cargo doc --workspace --no-deps --all-features` with `RUSTDOCFLAGS=-D warnings`
exits 0 on Windows; the macOS-only doc comments were audited by grep.

- **The second run's failures were the next layer down, and each fix is at its owner:**
  the macOS-only `run_application() as i32` cast is now `i32::from(...)` (clippy's lossless-cast
  lint fires only where the macOS block compiles); the Tauri comparison leg now overlays a
  pinned `Cargo.lock` alongside the pinned manifest — the CLI's pre-flight version check reads
  the *lock*, which still held the upstream 1.x pins, so the build was refused before Cargo
  ever resolved the pinned `=2.12.5`; and the runtime's application tests now skip on machines
  that cannot open a real window (a headless Linux runner has no X11/Wayland server, and
  AppKit requires the event loop on the main thread a test harness does not own) instead of
  panicking inside the platform's event loop — the same decision the GPU surface probe makes.

Tested: `cargo doc --workspace --no-deps --all-features` (RUSTDOCFLAGS=-D warnings) exits 0;
the workspace suite and clippy re-run green on Windows, where the application tests still run
for real with the guard in place.
