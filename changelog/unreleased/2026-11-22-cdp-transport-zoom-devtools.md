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

Tested: `cdp.rs` unit tests (endpoint parsing with and without a bearer path, a missing or
junk port file refused by name, a refused upgrade reported as `cdp_connect_failed`); the
dispatch parity test covers the new methods; the full `cargo test --workspace` suite passes
(1497 tests) and `cargo clippy --workspace --all-targets` is clean. Recorded rather than
claimed: a live WebView2 exchange is not exercised by the suite — the transport is proven to
the endpoint boundary, and the one real engine conversation is the next human verification.
