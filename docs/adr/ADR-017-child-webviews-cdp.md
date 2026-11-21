# ADR-017: Child webviews and CDP-equivalent protocol

**Status:** Accepted — Phase 1 implemented (`ncui.window.create.child`), Phase 2 implemented (the CDP WebSocket transport, 2026-11-22), Phase 3 open  
**Date:** 2026-11-20  
**Context:** ZCode's Browser tool requires child webviews and a DevTools Protocol (CDP) equivalent. NC-UI currently has neither. This ADR evaluates the design options, their trade-offs, and recommends an approach.

## 1. The problem

ZCode's Browser tool needs two capabilities that NC-UI does not have:

1. **Child webviews** — a page creates a second webview as a child of itself, with its own URL, navigation policy, and capability grant. Tauri 2 supports this via `WebviewBuilder::new(&webview_window)` and `add_child_webview`. Electron supports it via `<webview>` tags and `BrowserView`.

2. **CDP-equivalent protocol** — a way for the host to send DevTools Protocol commands to the embedded browser engine (navigate, evaluate JS, capture screenshots, inspect DOM). Tauri 2 on Windows uses WebView2's `CallDevToolsProtocolMethod` via COM. Electron exposes CDP natively.

### Why NC-UI needs this

- ZCode's Browser tool (`native/crates/nc-code-shell/src/browser_view.rs:477-615`) creates a child webview, injects a CDP port, and drives it via `CallDevToolsProtocolMethod`.
- Without child webviews, the Browser tool has no native port — the page disappears.
- Without CDP, the Browser tool cannot navigate, evaluate, or screenshot the child view.

### Current constraints

1. **ADR-001 §15-16**: "does not patch, fork, or inject into the browser engine" — rules out modifying the WebView engine itself.
2. **`unsafe_code = "forbid"`** — workspace-wide ban on `unsafe` Rust code.
3. **`ncui-webview` is pure policy** — the platform binding lives in `ncui-webview-wry` (layer 3).
4. **wry's public API has no child-webview seam** — `wry::WebViewBuilder` creates top-level windows, not children.
5. **`wry` exposes no `PostWebMessageAsJson`** — the COM interface is not accessible from safe Rust.

## 2. Options evaluated

### Option A: wry child webviews via `WebviewBuilder::new`

**Approach:** Use wry's `WebViewBuilder` to create a second webview inside the same window, passing a `WebviewWindowBuilder` that specifies the child's geometry.

**Pros:**
- Uses wry's existing public API — no FFI, no `unsafe`.
- Child webview shares the parent's event loop, so no threading complexity.

**Cons:**
- wry 0.57 does not expose `add_child_webview` or `WebviewBuilder::new` for in-process children. The `wry` crate creates one webview per `WebView` instance; multiple webviews require multiple windows.
- Even if wry supported it, the child would not have its own capability grant or navigation policy — those are per-window in NC-UI's model.
- **Verdict: not viable with current wry.**

### Option B: Multiple top-level windows with parent-child relationship

**Approach:** The host creates a second top-level window (via the existing `WindowHost` / `HostFactory` seam) that is logically a "child" of the first. The parent page can communicate with the child via `ncui.app.emit` events.

**Pros:**
- Uses existing infrastructure — no new platform binding needed.
- Each window has its own capability grant, navigation policy, and lifecycle — consistent with NC-UI's security model.
- Works on all platforms (Windows, Linux, macOS) with the existing wry host.

**Cons:**
- The child is a separate top-level window, not an embedded child webview. It appears in the taskbar and has its own decorations.
- No CDP-equivalent protocol — the parent can only communicate via `ncui.app.emit` events and `window.post`, not via CDP commands like `Runtime.evaluate` or `Page.captureScreenshot`.
- The ZCode Browser tool expects an embedded view, not a separate window.

**Verdict: viable as a first step, but does not fully satisfy the Browser tool's needs.**

### Option C: Windows WebView2 CDP via a scoped `#[allow(unsafe_code)]` module

**Approach:** Add a single, audited module in `ncui-webview-wry` that calls WebView2's `CallDevToolsProtocolMethod` via the COM interface, behind an ADR-012 exception.

**Pros:**
- Gives ZCode the exact CDP protocol it expects.
- The COM call is a well-defined interface — not a patch or fork of the browser engine.
- Can be scoped to Windows only, with feature gates.

**Cons:**
- Requires lifting `unsafe_code = "forbid"` for one module — a significant policy change.
- The COM interface requires raw pointer manipulation (`ICoreWebView2` → `ICoreWebView2_2` → `CallDevToolsProtocolMethod`).
- ADR-001 §15-16 forbids "patching, forking, or injecting into the browser engine" — a CDP call is arguably not injection, but it's close enough to require an explicit ADR-012 exception.
- Does not work on Linux (WebKitGTK has no CDP) or macOS (WKWebView has no CDP).
- **Verdict: high-risk, Windows-only, requires policy exception.**

### Option D: Chrome DevTools Protocol via a WebSocket sidecar

**Approach:** On Windows, launch the WebView2 with `--remote-debugging-port=<port>` and connect to it via a WebSocket CDP client. The host communicates with the child webview through standard CDP over WebSocket.

**Pros:**
- Uses the standard CDP protocol — no custom COM code, no `unsafe`.
- Cross-platform: works with any Chromium-based webview that supports `--remote-debugging-port`.
- Well-documented protocol with existing Rust libraries (`chromiumoxide`, `cdp`).
- The child webview is a real Chromium page with full DevTools access.

**Cons:**
- Requires launching a separate browser process (not embedded in the NC-UI window).
- The WebSocket CDP connection is a new transport — adds latency.
- WebView2 must be launched with the remote debugging flag, which may conflict with the wry host's own WebView2 instance.
- The child is not visually embedded in the parent window — it's a separate browser process.

**Verdict: the most practical CDP path, but does not deliver an embedded child webview.**

### Option E: `<webview>` tag equivalent via a custom HTML element

**Approach:** Implement a custom HTML element `<ncui-webview>` that the page can use to embed a child webview. The host intercepts the element's creation, creates a child webview via the platform API, and positions it over the element's bounding box.

**Pros:**
- Most natural developer experience — a page writes `<ncui-webview src="...">` and gets an embedded browser.
- The child inherits the parent's capability grant (narrowed).
- Cross-platform: WKWebView supports `addSubview`, WebViewGTK supports `add_child`, WebView2 supports child HWNDs.

**Cons:**
- Requires platform-specific native code for each OS (WebView2 child HWND, WKWebView subview, WebKitGTK child widget).
- The wry crate does not expose these APIs — would need direct platform bindings.
- Complex geometry synchronization between DOM and native child surface.
- **Verdict: the right long-term design, but a large project.**

## 3. Recommendation

**Phased approach: Option B now, Option E later, Option D as a CDP bridge.**

### Phase 1: Logical child windows (Option B) — immediate

1. Add a `ncui.window.create_child` method that creates a second top-level window with a narrowed grant and a parent-child relationship.
2. The child window is tracked in the parent's `WindowSummary` with `parent: Some(parent_id)`.
3. The parent communicates with the child via `ncui.app.emit` events and `ncui.window.post`.
4. The child can navigate itself via `ncui.window.post` with `window.location = ...`.

This gives ZCode's Browser tool a working native port immediately — the browser view is a real NC-UI window with its own URL bar and content, even if it's not visually embedded.

### Phase 2: CDP bridge via WebSocket (Option D) — implemented 2026-11-22

1. The engine is always created with `--remote-debugging-port=0` on Windows (an ephemeral port, so two instances never collide).
2. `ncui-webview-wry/src/cdp.rs` reads the port and bearer path from the user-data directory's `DevToolsActivePort` file and speaks the WebSocket handshake and one framed exchange over a loopback `TcpStream` — no new dependency.
3. `ncui.cdp.send` (requiring `Capability::Cdp`) carries the command through the window's backend to the endpoint and returns the endpoint's `result`; the page-side `__cdpSend` relay remains for pages that want to observe the traffic.
4. The engine-side call is a `WebViewBackend` method that a host without the transport refuses by name.

This gives ZCode the CDP protocol it expects, with no `unsafe` code and no policy exception.

### Phase 3: True embedded child webviews (Option E) — long-term

1. Design a `<ncui-webview>` custom element.
2. Implement platform-specific child surface creation (WebView2 child HWND, WKWebView subview, WebKitGTK child).
3. Synchronize geometry between DOM and native surface.
4. This requires a new ADR and likely a new layer-3 crate (`ncui-webview-child`).

## 4. Security model

- Child windows get a **narrowed grant** — a subset of the parent's capabilities. A page cannot grant its child more than it holds.
- CDP access requires a new `Capability::Cdp` — not in the default grant.
- All CDP commands are audited and rate-limited.
- The CDP WebSocket is loopback-only with a bearer token.

## 5. Consequences

- **Positive:** Unblocks ZCode's Browser tool immediately (Phase 1). Provides a path to CDP without `unsafe` (Phase 2). Long-term embedded child webviews (Phase 3).
- **Negative:** The child is a separate window in Phase 1, not embedded. Full CDP is Phase 2. True embedding is a large project (Phase 3).
- **Risk:** Phase 2 depends on WebView2's `--remote-debugging-port` support, which is Windows-only. Linux/macOS CDP is not addressed.
- **Alternative:** If ZCode's Browser tool can be adapted to use a separate window instead of an embedded view, Phase 1 alone may be sufficient.

## 6. References

- ADR-001: WebView strategy (no engine patching)
- ADR-012: Dependency policy (unsafe_code ban)
- ADR-013: Page bridge (JSON-only seam)
- `native/crates/nc-code-shell/src/browser_view.rs:477-615` — ZCode's current child webview usage
- `G:/nc-ui/docs/known-gaps.md:538-541` — wry safe message seam limitation
