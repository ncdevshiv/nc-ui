# ADR-017 Phase 3: embedded child webviews

**Status:** Accepted — implemented and verified on Windows and Linux; the DOM custom element remains future work
**Date:** 2026-11-22
**Builds on:** ADR-017 Phase 1 (logical child windows) and Phase 2 (Windows CDP transport)

## 1. What Phase 3 adds

Phase 1 creates a second top-level window. Phase 3 creates a real browser surface
inside an existing parent window's client area. The caller supplies the starting
URL and an explicit rectangle in logical pixels; the runtime does not infer a
layout.

The agent-facing methods are:

- `ncui.window.child.create` — validate and embed a child, returning its canonical
  `child-` plus sixteen-hex-digit identity.
- `ncui.window.child.bounds` — move a live child without changing its URL.
- `ncui.window.child.close` — close the engine surface and then remove its policy
  record.
- `ncui.window.child.list` — enumerate live children with their parent and current
  rectangle.

These are embedded surfaces, not the logical child windows created by
`ncui.window.create.child`.

## 2. Ownership and identities

The parent `Window` owns each child's lifecycle, isolated seam, current request,
IPC receiver and pending page messages. `WindowRegistry` delegates to that parent.
There is no process-global live-child registry in the production path.

The runtime parent `WindowId` is not an engine window handle. The live
`WindowHost` retains the platform's internal `u64` identity and routes child
operations through its own engine event loop. A child identity is issued by that
engine, remains scoped to its parent, and is never derived from the runtime
parent identity.

Creation commits policy state only after the engine reports a running child.
Duplicate engine identities, failed lifecycle transitions and parent engine loss
close the newly built surface. Parent engine death marks its live children
crashed; closing the parent cascades to its engine children.

`ncui-webview-child` supplies the engine-neutral request, identity, entry and
error types plus validation. Its standalone registry API is not the live
authority used by the runtime.

## 3. Platform binding and geometry

Windows uses wry's `WebViewBuilder::build_as_child` and `set_bounds` against the
parent's WebView2 child-HWND path.

Linux uses `WebViewBuilderExtUnix::build_gtk` with a `gtk::Fixed` container. The
host obtains the underlying WebKitGTK webview and explicitly moves it inside that
container: WebKitGTK's `set_bounds` changes the requested rectangle but does not
perform the required `GtkFixed` movement.

Bounds are relative to the parent's client area and use logical pixels. Geometry
is explicit: after a parent resize, the caller re-issues the rectangle it wants.
A bounds-only update validates geometry and preserves the original URL.

## 4. Security model

- All four methods require `Capability::Window` and pass through the normal
  authentication, rate-limit, authorization and audit pipeline.
- Creation validates the URL and rectangle before asking the engine to build.
- The initial URL must be readable and allowed by the parent's navigation policy.
- A child copies navigation, action and locale policy but receives an empty
  capability grant. It cannot exercise its parent's authority.
- Every bounds, close, evaluation and message operation is parent-scoped. A
  different parent cannot reach the child.
- Scripts are delivered only while the child is running; suspended, crashed and
  closed children refuse evaluation.
- A child cannot settle another page's deferred command. Replies are bound to
  `PageTarget::Child { parent, child }`.

## 5. IPC and lifecycle

The caller reserves the child identity before sending the build event so the IPC handler and a
timeout cleanup both name the same surface. If the reply budget expires, an ordered close event
follows the build and removes a child that completed after the caller stopped waiting.

The creation channel is the primary delivery path. If that receiver has disconnected, the same
message is sent to the parent's retained fallback receiver rather than duplicated. Both channels
and the policy-owned pending queue are bounded by the page bridge's 1,024-call budget. A wait
alternates between the two sources in short slices, so a primary IPC message wakes it promptly;
messages naming another child are refused. Closing a child removes its engine receiver before
removing its policy record.

## 6. Validation and failure rules

- A creation URL is non-empty and at most `MAX_CHILD_URL_BYTES` bytes.
- Coordinates are non-negative.
- Width and height are non-zero and at most `MAX_CHILD_EXTENT`.
- Invalid requests never reach the engine.
- Engine and ownership failures are reported as refusals; an unavailable parent
  engine does not invent a runtime window identity.
- A headless backend refuses child creation by name rather than claiming a
  surface exists.

## 7. Verification and observability

`crates/ncui-webview-child/src/lib.rs` tests DTO validation, canonical identities,
engine-issued identity preservation, registry operations and named headless
refusals. `crates/ncui-webview/src/windows.rs` and
`crates/ncui-runtime/tests/page_bridge.rs` cover parent ownership, initial-URL
vetting, empty grants, cross-parent refusal, bounds preservation, targeted replies
and lifecycle-gated evaluation.

`crates/ncui-webview-wry/src/host.rs` exercises a real child on Windows and Linux:
embed, move, post bridge IPC, receive it through the primary channel, receive it
through the fallback after disconnect, reject another parent and close. It also
proves that a timed-out build is closed before a later request and that primary IPC
wakes a wait before its deadline. The engine-neutral suite pins the bounded queue,
and the real page bridge runs on both engines.

The dispatch pipeline emits a `window.child_operations` counter with only four
fixed method labels and two outcome labels, plus a correlated completion trace.
Window and child identities are not metric labels. The latency harness measures
the real request-validation path as `webview.child_request_validate`; native
creation remains display- and user-driven rather than being presented as a
headless microbenchmark.

## 8. Consequences

- **Positive:** applications have a real embedded browser surface on Windows and
  Linux, with parent-owned lifetime, isolated authority and targeted IPC.
- **Negative:** callers own geometry updates; there is no automatic DOM layout or
  resize synchronization yet.
- **Platform limit:** macOS is not verified. Linux has no Chromium CDP endpoint;
  its native inspector remains available separately.
- **Future work:** a `<ncui-webview>` custom element can sit above this native API
  to observe DOM geometry and z-order. GPU islands inside children are not part
  of this phase.
