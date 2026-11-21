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
