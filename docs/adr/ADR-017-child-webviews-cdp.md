# ADR-017: Child webviews and CDP-equivalent protocol

**Status:** Accepted — Phase 1 implemented (`ncui.window.create.child`), Phase 2 implemented on Windows with an explicit WebKitGTK refusal, Phase 3 implementation integrated with final Windows/Linux verification in progress
**Date:** 2026-11-20
**Context:** ZCode's Browser tool requires child webviews and a DevTools Protocol (CDP) equivalent. At the time of this decision NC-UI had neither. This ADR records the options, the phased implementation, and the platform limits that remain.

## 1. The problem

ZCode's Browser tool needs two capabilities that NC-UI does not have:

1. **Child webviews** — a page creates a second webview as a child of itself, with its own URL, navigation policy, and capability grant. Tauri 2 supports this via `WebviewBuilder::new(&webview_window)` and `add_child_webview`. Electron supports it via `<webview>` tags and `BrowserView`.

2. **CDP-equivalent protocol** — a way for the host to send DevTools Protocol commands to the embedded browser engine (navigate, evaluate JS, capture screenshots, inspect DOM). Tauri 2 on Windows uses WebView2's `CallDevToolsProtocolMethod` via COM. Electron exposes CDP natively.

### Why NC-UI needs this

- ZCode's Browser tool (`native/crates/nc-code-shell/src/browser_view.rs:477-615`) creates a child webview, injects a CDP port, and drives it via `CallDevToolsProtocolMethod`.
- Without child webviews, the Browser tool has no native port — the page disappears.
- Without CDP, the Browser tool cannot navigate, evaluate, or screenshot the child view.

### Constraints at the time of the decision

1. **ADR-001 §15-16**: "does not patch, fork, or inject into the browser engine" — rules out modifying the WebView engine itself.
2. **`unsafe_code = "forbid"`** — workspace-wide ban on `unsafe` Rust code, except ADR-012's narrowly scoped Windows native-menu attachment.
3. **`ncui-webview` is pure policy** — the platform binding lives in `ncui-webview-wry` (layer 3).
4. **wry's then-public API had no child-webview seam** — the pinned wry 0.57 line now provides `build_as_child` on Windows and `build_gtk` for an embedded GTK container on Linux.
5. **wry exposes no `PostWebMessageAsJson`** — the COM interface is not accessible from safe Rust; the page bridge uses wry's IPC handler instead.

## 2. Options evaluated

### Option A: wry child webviews via `WebviewBuilder::new`

**Approach:** Use wry's `WebViewBuilder` to create a second webview inside the same window, passing a `WebviewWindowBuilder` that specifies the child's geometry.

**Pros:**
- Uses wry's existing public API — no FFI, no `unsafe`.
- Child webview shares the parent's event loop, so no threading complexity.

**Cons (at the time):**
- The wry API available when this ADR was written did not expose an in-process child builder. The pinned wry 0.57 line now exposes `build_as_child` and the Unix `build_gtk` path used by Phase 3.
- Even with a child builder, the child needs its own capability grant and navigation policy rather than silently sharing the parent window's authority.
- **Verdict at the time:** not viable with the then-current wry API. **Update:** Phase 3 uses the newer child APIs without patching wry.

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

### Option D: Chrome DevTools Protocol via a loopback WebSocket transport

**Approach:** On Windows, launch WebView2 with `--remote-debugging-port=<port>` and connect to its published endpoint via a WebSocket CDP client. The host communicates with the named window's engine through standard CDP over WebSocket.

**Pros:**
- Uses the standard CDP protocol — no custom COM code, no `unsafe`.
- Cross-platform: works with any Chromium-based webview that supports `--remote-debugging-port`.
- Well-documented protocol with existing Rust libraries (`chromiumoxide`, `cdp`).
- The child webview is a real Chromium page with full DevTools access.

**Cons:**
- WebView2 must be launched with the remote debugging flag; the host owns and bounds the resulting loopback endpoint.
- The WebSocket CDP connection is a new transport and adds latency.
- The transport is Windows-only. WebKitGTK exposes a native inspector but publishes no Chromium `DevToolsActivePort` file and does not speak CDP.
- CDP drives a page's engine; it does not by itself create an embedded child surface.

**Verdict:** the practical CDP path on Windows, implemented without a sidecar browser process. Phase 3 supplies embedded surfaces separately.

### Option E: `<webview>` tag equivalent via a custom HTML element

**Approach:** Implement a custom HTML element `<ncui-webview>` that the page can use to embed a child webview. The host intercepts the element's creation, creates a child webview via the platform API, and positions it over the element's bounding box.

**Pros:**
- Most natural developer experience — a page writes `<ncui-webview src="...">` and gets an embedded browser.
- The child copies navigation policy but receives an empty capability grant rather than inheriting the parent's authority.
- Cross-platform: WKWebView supports `addSubview`, WebViewGTK supports `add_child`, WebView2 supports child HWNDs.

**Cons:**
- Requires platform-specific child containers and their lifetime rules.
- Automatic DOM geometry adds observation, resize and z-order work beyond the explicit bounds API.
- **Verdict:** the right long-term page API. Phase 3 implements the native child surface beneath it; the custom element remains future work.

## 3. Recommendation

**Phased approach: Option B first, Option D for Windows CDP, and Option E's native child surface before its DOM custom element.**

### Phase 1: Logical child windows (Option B) — implemented

1. Add a `ncui.window.create_child` method that creates a second top-level window with a narrowed grant and a parent-child relationship.
2. The child window is tracked in the parent's `WindowSummary` with `parent: Some(parent_id)`.
3. The parent communicates with the child via `ncui.app.emit` events and `ncui.window.post`.
4. The child can navigate itself via `ncui.window.post` with `window.location = ...`.

This gives ZCode's Browser tool a working native port immediately — the browser view is a real NC-UI window with its own URL bar and content, even if it's not visually embedded.

### Phase 2: CDP bridge via WebSocket (Option D) — implemented 2026-11-22, hardened 2026-11-24

1. The engine is always created with `--remote-debugging-port=0` on Windows (an ephemeral port, so two instances never collide).
2. `ncui-webview-wry/src/cdp.rs` reads the port and bearer path from the user-data directory's `DevToolsActivePort` file and performs a bounded WebSocket exchange over a loopback `TcpStream` — no new dependency. A command must carry a positive integer id; the client reads complete messages until that id answers, ignores events and unrelated responses, and safely handles ping/pong, fragmentation, close frames, and size limits.
3. The browser endpoint is process-wide, not a window identity. During serialized top-level window creation the engine snapshots `Target.getTargets` before and after the build and stores the exactly-one new `page` target on that `EngineWindow`. A missing first-start endpoint supplies an empty baseline; zero or multiple new page targets fail closed as `cdp_target_bind_failed`, without title or URL guesses.
4. Every page command is carried by `HostEvent::CdpRequest` with the host's engine `u64` identity and a bounded reply wait. The engine attaches `Target.attachToTarget { targetId, flatten: true }`, adds the returned `sessionId` (replacing any caller-supplied session), sends the command, and detaches afterward. Only browser-domain discovery such as `Browser.getVersion` uses the browser endpoint directly.
5. `ncui.cdp.send` (requiring `Capability::Cdp`) carries the command through the window's backend and returns the endpoint's `result`; the page-side `__cdpSend` relay remains for pages that want to observe the traffic.
6. `CdpError` is engine-neutral and `WebViewBackend::cdp_send` returns it directly. Runtime dispatch, screenshot/PDF capture, JSON-RPC, and recognized IPC codecs preserve the stable `cdp_*` code instead of collapsing every failure to a conflict.
7. WebKitGTK has no Chromium CDP endpoint. Non-Windows engines return `cdp_platform_unsupported` without waiting for a port file that cannot exist; the real Linux inspector remains available through the separate devtools methods.

This gives ZCode the CDP protocol it expects on Windows, with no `unsafe` code and no policy exception. Linux's refusal is explicit rather than presented as CDP support, and one window's page commands cannot silently execute in another window.

### Phase 3: True embedded child webviews (Option E) — implementation integrated

1. The parent `Window` owns each child's lifecycle, isolated seam, current bounds and pending page messages; the live `WindowHost` routes operations with its internal engine window identity.
2. Windows uses wry's `build_as_child`; Linux uses `build_gtk` inside a `gtk::Fixed`, with explicit widget movement because WebKitGTK's `set_bounds` does not move a child within that container.
3. `ncui.window.child.create`, `ncui.window.child.bounds`, `ncui.window.child.close` and `ncui.window.child.list` expose the surface through the normal dispatch and consent pipeline.
4. An embedded child copies navigation, action and locale policy but starts with an empty capability grant. It cannot act on its parent window or settle another page's deferred command.
5. A `<ncui-webview>` custom element with automatic DOM geometry remains future work; the current API positions the native surface explicitly.

## 4. Security model

- Child windows get a **narrowed grant** — a subset of the parent's capabilities. A page cannot grant its child more than it holds.
- Embedded child surfaces copy the parent's navigation, action and locale policy but receive an **empty capability grant**; parent-window actions and cross-page deferred-reply settlement are refused.
- CDP access requires a new `Capability::Cdp` — not in the default grant.
- All CDP commands are audited and rate-limited.
- The CDP WebSocket is loopback-only with a bearer token.

## 5. Consequences

- **Positive:** Phase 1 supplies logical child windows; Phase 2 supplies real Windows CDP without `unsafe`; Phase 3 supplies parent-owned embedded surfaces on Windows and Linux with isolated policy and targeted IPC.
- **Negative:** Phase 1 remains a separate window. CDP is Windows-only, and embedded geometry is explicit rather than DOM-synchronized.
- **Risk:** native child composition and focus remain platform-specific; the parent owns engine cleanup, and every child operation is checked against that parent.
- **Alternative:** a caller that does not need embedding can continue to use Phase 1 without the child-surface API.

## 6. References

- ADR-001: WebView strategy (no engine patching)
- ADR-012: Dependency policy (unsafe_code ban)
- ADR-013: Page bridge (JSON-only seam)
- `native/crates/nc-code-shell/src/browser_view.rs:477-615` — ZCode's current child webview usage
- `docs/known-gaps.md`, "The native surface, capability by capability" — current platform evidence and limits
