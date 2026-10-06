# ADR-002: GPU architecture

**Status:** Accepted · **Date:** 2026-10-01

## Context
DOM rendering does not scale to million-line editors, large tables, or high-frequency canvases.
A million logical records must not become a million DOM nodes.

## Decision
Rendering has **three strategies** — `Dom`, `Gpu`, `Native` — and a component declares or
negotiates one. A **GPU Island** is a rectangular region of the page whose content is rendered
by the GPU engine rather than the DOM, embedded in ordinary HTML.

GPU Islands must support the full input contract: mouse, keyboard, focus, scrolling, clipboard,
selection, resize, high-DPI, drag and drop, IME, and accessibility. An island that is a bare
canvas and drops the rest is not acceptable: accessibility for GPU content is a first-class
requirement, not a follow-up.

The GPU engine is a native Rust layer. The hot path uses an efficient typed binary
representation; JSON is reserved for the control plane.

## Consequences
- A DOM/GPU composition boundary becomes the highest-risk area in the renderer: focus,
  scrolling, and IME must behave identically whether content is DOM or GPU.
- Every GPU component needs a text layout path. We are not attempting to out-engineer
  HarfBuzz/CoreText/DirectWrite; we integrate them.
- GPU device loss is a normal event, not an exceptional one, and recovery is a tested path.
- The `render_mode` of every component is observable, so a bad DOM/GPU split is measurable
  rather than a matter of opinion.

## Implementation status
**Not implemented.** No GPU renderer exists. The half that can exist without one — the
accessibility strategy — is built; see below.
