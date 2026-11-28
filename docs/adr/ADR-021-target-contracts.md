# ADR-021: Target contracts — native windows, browser/WebGPU, mobile

**Status:** Accepted · **Date:** 2026-11-28

## Context

NC-UI's target matrix has been one target wide: web-authored pages inside native windows.
The goal prompt's B4/B5 items require broadening it, and require this ADR before any code,
because the word "target" hides three different contracts that share almost nothing except
the rendering model. Writing code before separating them is how a runtime ends up claiming
a browser target because it can host HTML — which it already does, and which is not the
same statement.

The three contracts:

1. **Web-authored pages in native windows.** What exists today: untrusted HTML/JS behind
   the seam (ADR-001), rendered by the platform WebView, optionally owning GPU island
   rectangles (ADR-002). The page is the application; the runtime is the host.
2. **Ncui-native content in a browser/WebGPU shell.** The rendering engine — the wgpu
   renderer and the island/input model around it — compiled to WASM and running inside a
   real browser, drawing through WebGPU. There is no WebView and no HTML application:
   the browser is the platform the way Win32 is the platform on Windows, and the shell
   owes the same platform contract a windowing system owes — input, clipboard, text
   services, lifecycle, accessibility, packaging.
3. **Ncui-native content on iOS/Android.** The same rendering engine behind the platform
   WebView (WKWebView / Android WebView) with the capability model intact — a layer-3
   adapter crate the way `ncui-webview-wry` is, per B4. This contract is recorded here
   for completeness; it needs hardware this project does not have, and per the project's
   own rule it stays `absent` rather than done until its gate runs on a real device.

The confusion this ADR exists to prevent: (1) and (2) both involve a browser engine
somewhere, so a sloppy account can pass one off as the other. They are opposites. In (1)
the web content is the application and the runtime wraps it; in (2) the Rust engine is
the application and the browser is merely the OS it runs on. A shell that renders the
page's DOM is contract (1) with extra steps, not contract (2).

## Decision

**The browser/WebGPU shell is one crate, `ncui-web-shell`, and its unit of content is the
island scene.** The GPU island renderer (`ncui-gpu-renderer`) is already platform-wrapped
wgpu; wgpu's `webgpu` backend makes the same rendering code run on a browser canvas. The
shell compiles the renderer and the island scene model to `wasm32-unknown-unknown` and
owns everything the browser does not provide for free:

- **Canvas and compositing.** One `<canvas>` per shell instance, sized by
  ResizeObserver, scaled by `devicePixelRatio` — high-DPI is the shell's problem,
  not the scene's. The renderer's surface is the canvas; there is no DOM
  compositing boundary because there is no DOM content.
- **Frame scheduling.** `requestAnimationFrame` drives the render loop; the loop is
  idle-stopped — a static scene draws nothing, the same rule the desktop renderer
  follows.
- **Input.** Mouse, keyboard, focus, scrolling, selection, resize, drag and drop —
  the ADR-002 input contract — arrive as DOM events and enter the same input model
  the native host uses. The contract items are identical; only their source differs.
- **Clipboard.** The async Clipboard API (`navigator.clipboard`), capability-gated
  exactly as the desktop clipboard is: a target's platform does not loosen the
  security model (ADR-007). Where the browser requires a user gesture, the shell
  defers to one rather than silently failing.
- **Text services and IME.** Browser IME arrives through composition events on a
  hidden focused element — the standard wasm editing shim — and is routed into the
  same composition state machine the native fields use. Hidden-element shims are
  platform reality on the web, the way `WM_IME_*` is platform reality on Windows;
  what is not acceptable is dropping composition entirely.
- **Accessibility.** The island scene carries the semantic tree `ncui-a11y` already
  models. In a browser the honest surface is the DOM accessibility tree: the shell
  maintains a parallel set of visually hidden ARIA nodes mirroring the scene's
  semantics — the same information the AccessKit bridge publishes on Windows,
  retargeted at the web's accessibility primitive.
- **Lifecycle and packaging.** The shell is a static artifact: one `.wasm` plus one
  thin `.js` loader plus one `index.html`, servable by any static file server. The
  runtime's asset-server model does not apply — there is no runtime process — so
  packaging is a build step (`ncui build --target web`), not a host.

**Evidence is a real-browser gate, and compile-only is nothing.** Per the goal prompt:
no target is supported because it compiles. The shell's gate runs the artifact in a
real browser — headless or headed Chromium-family on the development machine counts,
because WebGPU in Chromium is the real implementation, not a mock — and proves the
same class of evidence the native targets prove: pixels (a rendered scene read back
through the browser), input (synthesized DOM events reach the scene's input model),
and semantics (the ARIA mirror reflects the scene). A gate that only checks the
artifact loads is the compile-only fallacy wearing a browser costume.

**What the shell is not.** It is not a way to run pages (contract 1) in a browser —
there is nothing to run them in a browser with, they are already web content. It does
not share the runtime process, the RPC seam, or the agent protocol: those are host
concerns, and the browser shell has no host. If a future requirement is agent-driven
inspection of browser-hosted scenes, it will arrive as its own ADR rather than as a
silent re-expansion of this one.

## Consequences

- `wgpu` gains the `webgpu` feature — additive, and per ADR-012 it is the same
  dependency exception, not a new one.
- WASM-forbidden patterns in the renderer's dependency cone (OS threads, blocking IO,
  `std::time::Instant` misuse) surface at compile time, not in review: the wasm32
  compile check is the honest detector, and it runs in CI.
- The ARIA mirror duplicates information the AccessKit tree already carries — the
  cost of two accessibility platforms is paid once in `ncui-a11y`'s semantic model,
  not twice in the shells.
- Text services through a hidden element are lossy compared to native IME (candidate
  window positioning is the browser's, not ours). Recorded here rather than
  discovered later; the composition state machine itself is exact.

## Implementation status

**Partially implemented — the pipeline is proven in pixels; the contract items are not.**

Proven on 2026-11-28, on this machine, by `crates/ncui-web-shell/gate/run-gate.ps1`:

- `ncui-gpu-renderer` and its dependency cone compile to `wasm32-unknown-unknown` with
  wgpu's `webgpu` backend enabled; native compilation is unaffected. The cost was honest
  and small: thread-safety bounds in `ncui-gpu-renderer` follow the target, because
  browser objects are single-threaded by platform design.
- `ncui-web-shell` exists: it creates the instance/adapter/device on a real canvas,
  configures the surface, and presents frames through a requestAnimationFrame loop.
- The real-browser gate passes in headless Edge: `webgpu_available=true`,
  `attached=true`, zero console errors, and the compositor readback shows the shell's
  exact clear color rgb(28, 41, 82) at the canvas center — pixel-level evidence that
  instance → device → surface → render pass → presentation works in a real browser,
  not merely that the crate compiles.

Additionally proven on the same day, in the same gate run:

- **Scene rendering in pixels**: the shell builds a deterministic `Frame` of coloured
  islands, plans it through the shared `RenderPlan`, and draws it through the shared
  `QuadPipeline` — the same rendering path the native targets use. The gate verifies
  three independent pixel truths: navy clear at the canvas centre, the red island's
  exact colour inside its rectangle, the green island's inside its own.
- **Input arrival**: DOM pointer/keyboard listeners translate events into the
  `ncui-gpu` input model (`PointerEvent`, `PointerPhase`, `MouseButton`). The gate
  synthesizes a click at (70, 50) plus a keypress through CDP and reads the shell's
  input accounting back: move/down/up counted, coordinates exact, key counted.

Further proven on the same day:

- **IslandHost routing**: the shell owns an `IslandHost` over the scene's islands —
  the same rectangles the renderer draws, since a hit-test disagreeing with the paint
  would be lying about one of them. The gate proves the input contract's outcomes, not
  event arrival: a synthesized click inside the red island selects the correct row at
  exact geometry (`red_selection=1..2`), Tab moves focus into the host (`focused=0`),
  and subsequent keypresses route into the focused island (`last_route=island:red`).
  Focus entry follows the gpui shape: unfocused Tab is the shell's responsibility;
  once an island holds focus the host arbitrates, and an island can never trap the user.

Further proven on the same day, same gate:

- **Scrolling**: wheel events enter the input model as scroll-phase pointer events;
  the red island carries content taller than its viewport, and a synthesized wheel
  of 120px moved its scroll exactly 120. The probe is temporal because the host's
  documented semantics clear focus on scroll — the gate reads focus before wheeling.
- **Clipboard**: Ctrl+C copies the focused island's selection through `host.copy()`
  into the browser clipboard; the gate reads `"second"` back through
  `navigator.clipboard.readText()` — the host's truth and the platform's truth agreeing.
  Paste runs the other way through the async Clipboard API into `host.paste()`.

Further proven, same gate:

- **The ARIA accessibility mirror**: the scene's `SemanticTree` renders into visually
  hidden ARIA DOM — clipped rather than `display:none`, which browsers drop from the
  accessibility tree. The gate verifies both islands as labeled regions, all three rows
  as focusable button nodes, hidden visually but present to AT — the same information
  the AccessKit bridge publishes on Windows, on the web's accessibility primitive.

- **ResizeObserver/devicePixelRatio sizing**: backing store follows CSS size times
  devicePixelRatio through a real ResizeObserver; the gate resizes the canvas by CSS
  and verifies the backing store follows exactly. The probe caught a real latent bug —
  a `RefCell` double-borrow in the surface-reconfigure path that was unreachable until
  resizing existed — which is what evidence gates are for.

Completed the same day, each with gate evidence:

- **The IME composition shim**: a visually hidden textarea is the composition surface —
  the web's platform reality the way `WM_IME` is Windows'. `compositionupdate` carries
  the whole in-progress string into `IslandHost::set_composition` — a replace entry
  point added to the shared input model because keystroke semantics cannot represent
  platform composition — and `compositionend` routes the host's commit. The gate
  dispatches real composition events: `nih` visible in composition state during,
  `nihon` committed after.
- **Drag-and-drop**: browser drops hit-test through `route_drag` and text payloads
  land in the island's clipboard through `drop_text`; the gate drops synthetic text
  over the red island and reads it back.
- **Idle-stopped scheduling**: input, resize and startup dirty the loop; a static
  scene presents nothing. The gate measures zero frame growth across an idle window.
- **Packaging**: `package.ps1` produces the static artifact — release wasm, loader,
  html — and the *entire gate passes against the packaged artifact itself*
  (`node verify.mjs dist`), because a package is only proven when it runs.

**The browser/WebGPU shell (contract 2) is implemented and evidence-complete** as of
2026-11-28: canvas, rAF scheduling, ResizeObserver/high-DPI, clipboard, text services,
lifecycle, accessibility, packaging, input (mouse, keyboard, focus, scrolling,
selection, drag-and-drop), scene rendering — every item verified in a real browser,
in pixels or in platform state, never by compilation. The honest caveats: the scene
is the shell's deterministic gate scene rather than hosted application content —
wiring real island content in is runtime integration work, not shell work — and live
OS-IME verification with a human belongs with the kit's own IME pass. The mobile
contract (3) remains unstarted and hardware-blocked.
