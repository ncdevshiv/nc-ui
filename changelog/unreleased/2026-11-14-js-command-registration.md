- **A page can now register a command handler in JavaScript, and the runtime calls it.** Native
commands were Rust-only, and the any-language sidecar (the previous slice) answered a command with
another *program*. The last gap was a command answered by the page's own code. An application now
declares a page command in `ncui.app.json` — `page_commands: [{command, requires, summary}]` — and
the page registers the function that answers it with `window.ncui.registerCommand(name, fn)`.
Calling it through the usual `window.ncui.invoke` seam is gated exactly like a native command: the
command must be declared and the page must hold its `requires`, or the seam refuses it under the
page's own correlation id so its promise rejects rather than hangs. This closes the JavaScript half
of C3 with no new dependency.

**The round trip, and why it is shaped this way.** `wry` exposes no way to read a value back out of
`evaluate_script`, so the runtime cannot answer a page handler directly. Instead it holds the page's
original invoke open, evaluates `window.ncui.__runCommand(replyId, name, payload)` in the page, the
handler runs and posts its answer back over the same bridge under `replyId` (a
`command_result` message the seam parser now understands), and the runtime settles the original
invoke with it. A handler that is missing or throws is reported to the page as an error rather than
left to time out; a command declared but never handed a handler answers with an error rather than
running anything; and a component cannot reach a page command — only a page can, which is what
keeps page code from being driven by a component. `PageCommands` bounds the outstanding calls
(`MAX_PENDING_PAGE_CALLS`), and every refusal carries the original id.

**A bug the end-to-end test caught, not a unit test.** The first version sent a page command's
capability refusal without the correlation id, so the page's own invoke would have hung until its
30-second timeout instead of rejecting at once. The `page_bridge` end-to-end test — which drives the
real page pump — failed on exactly this, because it asserts the refusal carries the id of the call
being answered. Fixed by stamping the original id on every page-command refusal.

**Tested end to end, against a real engine and a real application.** `page_bridge.rs` drives the
real page pump: a page invokes a registered JS command, the runtime hands it over, the answer comes
back and settles the page's own invoke; a second test asserts an ungranted page command is refused
at the seam and never handed to the page. `ncui-webview-wry` runs the page half against a real
WebView — `registerCommand` stores a handler and `__runCommand` runs it and posts the answer. Unit
tests cover the seam's `command_result` parse and its `authorize_perform` gate, the pending-call map
(settle-once, distinct reply ids), and the manifest declaration (real JSON, including a page command
that shadows a sidecar's name). The counter example now declares `counter.summary` and registers a
handler for it, and a smoke run confirms the application opens and stays alive with the command
registered. Gate: `cargo fmt`, `cargo clippy --workspace --all-targets` (pedantic, no warnings,
`unsafe_code = "forbid"`), the full `cargo test --workspace` suite, and the layering gate all pass.
