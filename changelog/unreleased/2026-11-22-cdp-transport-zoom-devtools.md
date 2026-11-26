- **The CDP transport exists (ADR-017 Phase 2).** `ncui.cdp.send` used to forward a command
to a page-side relay, which was honest about being a relay but left a page without its own
debugging endpoint nowhere to send a command. The engine is now created with
`--remote-debugging-port=0` (Windows/WebView2), the port and bearer path it publishes are
read from the user-data directory's `DevToolsActivePort` file, and the command is delivered
over a loopback WebSocket to the page's own DevTools endpoint — no `unsafe`, no engine
patching, one JSON answer back. `WebViewBackend` gained `set_zoom`, `open_devtools`,
`close_devtools`, `is_devtools_open`, `cdp_send` and `as_any` (defaulting to refusal, so a
headless backend says so by name); the wry host implements them; the runtime gained
`ncui.window.zoom` (factor 0.25–5.0), `ncui.window.devtools.open/close/state` (state answers
`open`/`closed`/`unknown`, because wry's Windows backend cannot report the inspector's
state), and `ncui.cdp.send` now returns the endpoint's `result` instead of a relay receipt.
WebKitGTK does not publish Chromium's `DevToolsActivePort` file or speak CDP, so non-Windows
engines return `cdp_platform_unsupported` by name rather than waiting for an endpoint that cannot
exist; Linux's native inspector remains available through the devtools methods.

Tested: `cdp.rs` unit tests (endpoint parsing with and without a bearer path, a missing or
junk port file refused by name, a refused upgrade reported as `cdp_connect_failed`); dispatch
parity covers the methods; the live Windows host exchanges `Browser.getVersion` with WebView2;
the live Linux host opens, observes and closes the real WebKitGTK inspector and verifies that
the named CDP refusal leaves its engine alive.
