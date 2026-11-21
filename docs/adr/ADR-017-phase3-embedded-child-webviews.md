# ADR-017 Phase 3: embedded child webviews (`ncui-webview-child`)

**Status:** Accepted — scaffold implemented (policy surface, registry, geometry contract, headless refusals); the wry engine binding and agent-facing dispatch are the next slices
**Date:** 2026-11-22
**Builds on:** ADR-017 Phase 1 (logical child windows) and Phase 2 (CDP WebSocket transport)

## 1. What Phase 3 adds

Phase 1 gives a page a second *window*; Phase 2 gives it the engine's DevTools
endpoint. Phase 3 is the piece the Browser tool ultimately wants: a webview
that lives **inside** the parent window's frame, positioned over a rectangle
the page chooses, with its own URL and its own seam-gated policy.

The scaffold ships as a new layer-3 crate, `ncui-webview-child`:

- `ChildWebviewRequest` — the value a caller provides: the starting URL, and
  the rectangle of logical pixels (relative to the parent window's client
  area) the child should occupy.
- `ChildWebviewHandle` — what the caller gets back: the child's identity and
  parent, plus the operations the surface supports (`set_bounds`, `close`).
- `ChildWebviewRegistry` — the process-wide record of live children, keyed by
  the child's identity, with the engine that builds them behind the
  `ChildWebviewEngine` seam.

## 2. Geometry synchronization

The child's rectangle is logical pixels relative to the parent's client area.
The engine applies it with wry's `WebView::set_bounds`, which on Windows moves
and sizes the child's container HWND inside the parent. Geometry is explicit:
when the parent resizes, the page re-issues the bounds it wants — the page
owns the layout, and the runtime does not invent one while the window changes
under it.

## 3. Security model

- Creating a child requires the `window` capability, the same one every other
  window operation requires.
- The child's page is governed by the engine's seam: navigation policy and
  capability grants are checked by the policy crate, not by the engine. A
  child cannot reach an origin its grant does not allow.
- The child is recorded with its parent's identity, so enumeration can show
  the tree, and a closed child is removed rather than leaked.

## 4. Validation rules

- The URL is bounded (`MAX_CHILD_URL_BYTES`) and non-empty.
- The rectangle sits inside the parent's client area: no negative origin.
- The extent is non-zero and bounded (`MAX_CHILD_EXTENT`).
- A request that fails validation is refused **before** the engine is asked,
  so a malformed request never reaches the platform.

## 5. What the scaffold deliberately does not do yet

- **No engine binding yet.** The wry half — `build_as_child` and
  `set_bounds` against a real parent window — is the next slice, together
  with live-window tests.
- **No agent-facing JSON-RPC methods yet.** The dispatch methods
  (`ncui.window.child.create`, `ncui.window.child.bounds`,
  `ncui.window.child.close`, `ncui.window.child.list`) come after the engine
  binding has been exercised on a real window.
- **No input routing.** The child is a real WebView2 surface; mouse and
  keyboard land in it natively. Nothing in the runtime needs to forward them.
- **No GPU islands inside children.** The island surface remains a
  parent-window feature; children get the plain webview.

## 6. Why a separate crate

ADR-011's layering keeps the pure policy (`ncui-webview`) away from the
platform binding (`ncui-webview-wry`). An embedded child is mostly policy
(what may exist, where it may point, who may create it) with a thin engine
half (build it, move it). A separate crate keeps that split honest: the
policy types and the registry live here, the wry implementation lives behind
the existing `wry-engine` feature in `ncui-webview-wry`, and a future second
engine only has to implement the same three operations.

## 7. Consequences

- **Positive:** the Browser tool's target shape (an embedded view the page
  positions) now has a designed, scaffolded home; the policy surface is
  tested headlessly (creation, validation, refusals, lifecycle), and the
  engine half is confined to the existing wry binding.
- **Negative:** the geometry model is "page re-issues bounds" rather than
  automatic resize, until a caller asks for something richer.
- **Risk:** WebView2's child-HWND composition has its own z-order and focus
  quirks; they will surface in the live-window tests of the next slice, which
  is exactly where the scaffold's tests are designed to go.
