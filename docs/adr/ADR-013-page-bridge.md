# ADR-013: The page bridge and the application command surface

**Status:** Accepted · **Date:** 2026-10-22

## Context

ADR-001 splits NC-UI in two: the WebView owns the page, and NC-UI owns the runtime around
it. That split answers *who renders*; it does not answer *how a page reaches the runtime*.

The first implementation of that answer was a page that posted a JSON object and a host that
answered by evaluating a script. It had three defects that only a real page could have
found:

* the host's answer called a function the injected script never defined, so every answer
  threw in the page and no page ever received one;
* nothing correlated a request with its answer, so two calls in flight could not be told
  apart;
* a page could only call the six read-only actions the runtime hard-coded, so an
  application had no way to give a page anything of its own.

A framework whose page cannot call the application is not a framework. An application
command surface is therefore part of the runtime, not a convenience layered over it.

## Decision

**The bridge is a correlated request/response protocol.** Every request carries an opaque
id minted by the page's bridge, and the host echoes it unchanged. The id is data: it never
selects a handler. A page may have any number of calls in flight, and each answer reaches
the call that asked for it.

**One action table decides what a page may call.** The table is shared by every window's
seam and by the runtime's command registry, so an action declared in one place is visible
in the other without a second source of truth. A page names an action, never a runtime
method.

**An application registers its own commands through that same table.** A command declares
the capabilities it requires. Registering a command whose capabilities the page grant does
not cover is refused, so a page is never handed authority the host did not grant it. A
command runs through the same rate limit, authorization, consent gate and audit trail an
agent's request takes.

**The page's half of the contract is one file.** `packages/ncui-client/src/bridge.js` is
the script the host injects, and it is the file the package's own suite exercises. There is
no second copy to drift.

## Consequences

- A page's request is now a protocol with a version, and the page can discover what it may
  call: `describe` reports the protocol version, the page's grant and every action with
  its requirements.
- The bridge refuses work it cannot carry: a request id is bounded and restricted to
  characters the transport can hold, so a page cannot put an unbounded string into a log
  or an audit record.
- The bridge bounds its own queue and times out a call that is never answered, so a
  stalled host cannot leave a page waiting forever.
- A command's consent is still a boolean the page supplies, which the same class of gap
  already recorded against the agent API. `docs/known-gaps.md` records it, and it is
  closed by the confirmation surface the application story has to add.
- The page bridge carries JSON. The runtime's binary framing is not on this path, and
  `docs/known-gaps.md` records what that costs and what would change it.

## Implementation status

**Implemented.** The protocol is in `crates/ncui-webview/src/seam.rs`, the runtime's
dispatch of it is in `crates/ncui-runtime/src/window.rs`, the injected script and the typed
client are in `packages/ncui-client/`, and the command registry is in
`crates/ncui-runtime/src/commands.rs`.

It is exercised end to end on Windows: `crates/ncui-webview-wry/tests/page_bridge.rs`
opens a real window, loads a real page, has the page make two calls, answers them in the
opposite order, and asserts that each call received its own answer.
