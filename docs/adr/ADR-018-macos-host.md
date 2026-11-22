# ADR-018: macOS host — main-thread event loop redesign

**Status:** Proposed  
**Date:** 2026-11-20  
**Context:** NC-UI's window host creates the tao event loop on a worker thread. On Windows and Linux, `tao` provides `with_any_thread(true)` to allow this. On macOS, `tao` panics: `"On macOS, 'EventLoop' must be created on the main thread!"`, and `PlatformSpecificEventLoopAttributes` is an empty struct — there is no any-thread switch. This blocks all macOS support.

## 1. The problem

### Current architecture

`ncui-webview-wry/src/host.rs` creates the tao event loop on a **worker thread**:

```rust
// Current (Windows/Linux):
let event_loop = EventLoopBuilder::with_user_event()
    .with_any_thread(true)  // Windows/Linux only
    .build();
```

The runtime's pump thread must not be the one blocking in `event_loop.run()`, so the loop runs on a background thread. On macOS, this panics.

### Why this matters

- macOS is the second-largest desktop platform after Windows.
- ZCode ships on macOS via Tauri 2. Without a macOS host, NC-UI cannot be a drop-in replacement.
- The registry records `cross_platform.verification.macos` as `not_implemented` — every feature's macOS platform state is `unverified`.

## 2. Root cause

`tao` 0.33's macOS backend requires `EventLoop::new()` to be called on the main thread because:
1. Cocoa's `NSApplication` must be initialized on the main thread.
2. The `CGEventSource` and display link APIs are main-thread-only.
3. `PlatformSpecificEventLoopAttributes` has no `any_thread` field on macOS.

The NC-UI host creates the loop on a worker thread because the runtime's pump thread (which calls `event_loop.run()`) must not be the same thread that the application's main logic runs on. On Windows/Linux, `with_any_thread(true)` bridges this gap. On macOS, there is no bridge.

## 3. Options evaluated

### Option A: Main-thread event loop with a separate pump thread

**Approach:** On macOS, create the event loop on the main thread. The runtime's pump logic runs on a separate thread and communicates with the event loop via `EventLoopProxy`. This is the standard macOS pattern.

**How:**
1. Split `run_engine` into two phases:
   - **Main thread:** Create `EventLoop`, set up `NSApplication`, call `event_loop.run()`.
   - **Worker thread:** The runtime's pump logic, communicating via `EventLoopProxy<HostEvent>`.
2. On Windows/Linux, keep the existing worker-thread model (`with_any_thread(true)`).
3. Use `#[cfg(target_os = "macos")]` to select the right startup path.

**Pros:**
- Uses tao's intended macOS pattern — no hacks, no FFI.
- No `unsafe` code.
- The event loop runs on the main thread as macOS requires.
- The pump thread is separate, so the runtime's logic doesn't block the event loop.

**Cons:**
- Two code paths (macOS vs. Windows/Linux) — more complexity.
- The main thread must be dedicated to the event loop — no other main-thread work is possible.
- The `EventLoopProxy` adds a small serialization cost per event.

**Verdict: this is the correct approach.**

### Option B: `#[allow(unsafe_code)]` FFI to call Cocoa APIs from the worker thread

**Approach:** Use `objc` or `cocoa` crates to call `NSApplication::sharedApplication()` and related APIs from the worker thread, bypassing tao's main-thread check.

**Pros:**
- Single code path.

**Cons:**
- Requires `unsafe_code` — violates the workspace-wide ban.
- `NSApplication` is not thread-safe — undefined behavior.
- The tao event loop itself would still panic.
- **Verdict: not viable.**

### Option C: Use `winit` instead of `tao` on macOS

**Approach:** Replace `tao` with `winit`, which supports any-thread event loops on macOS.

**Pros:**
- Single code path.

**Cons:**
- `winit` does not support webviews — it's a windowing library only.
- `wry` depends on `tao` — replacing `tao` means replacing `wry`.
- ADR-001 forbids patching or forking the browser engine.
- **Verdict: not viable.**

## 4. Design

### 4.1 Platform-specific event loop creation

```rust
#[cfg(target_os = "macos")]
fn create_event_loop() -> EventLoop<HostEvent> {
    // macOS: must be called on the main thread.
    // The caller is responsible for ensuring this.
    EventLoopBuilder::with_user_event().build()
}

#[cfg(not(target_os = "macos"))]
fn create_event_loop() -> EventLoop<HostEvent> {
    // Windows/Linux: any thread is fine.
    EventLoopBuilder::with_user_event()
        .with_any_thread(true)
        .build()
}
```

### 4.2 Platform-specific engine startup

```rust
#[cfg(target_os = "macos")]
fn run_engine_macos(event_loop: EventLoop<HostEvent>, proxy: EventLoopProxy<HostEvent>) {
    // macOS: the event loop runs on the main thread.
    // The proxy is used by the worker thread to send messages.
    event_loop.run(move |event, target, control_flow| {
        // ... same event handling as the cross-platform path ...
    });
}

#[cfg(not(target_os = "macos"))]
fn run_engine(event_loop: EventLoop<HostEvent>, proxy: EventLoopProxy<HostEvent>) {
    // Windows/Linux: the event loop runs on a worker thread.
    std::thread::spawn(move || {
        event_loop.run(move |event, target, control_flow| {
            // ... same event handling ...
        });
    });
}
```

### 4.3 Main-thread entry point on macOS

On macOS, `main.rs` must create the event loop on the main thread:

```rust
#[cfg(target_os = "macos")]
fn main() {
    let event_loop = create_event_loop();  // main thread
    let proxy = event_loop.create_proxy();
    
    // Spawn the runtime on a worker thread.
    std::thread::spawn(move || {
        // runtime logic, using proxy to communicate with the event loop
    });
    
    // Run the event loop on the main thread.
    run_engine_macos(event_loop, proxy);
}
```

### 4.4 Feature gating

The macOS path is behind `#[cfg(target_os = "macos")]` — it only compiles on macOS. The existing Windows/Linux path is unchanged.

## 5. Security model

No changes to the security model:
- The event loop still obeys the `HostSeam` for navigation and capability decisions.
- The pump thread still runs the runtime's dispatch pipeline.
- No new capabilities or grants are introduced.

## 6. Testing

- **Unit tests:** The event handling logic is platform-independent and tested as before.
- **macOS CI:** A `macos-latest` runner builds and runs the full gate suite.
- **Smoke test:** `ncui run examples/counter` opens a real window on macOS.
- **Parity test:** The same test suite that passes on Windows and Linux passes on macOS.

## 7. Consequences

- **Positive:** NC-UI becomes a true cross-platform runtime (Windows, Linux, macOS).
- **Negative:** Two code paths (macOS vs. Windows/Linux) — more maintenance.
- **Risk:** The macOS path cannot be tested from the current Windows/Linux machine. A macOS CI runner is required before the `cross_platform.verification.macos` registry entry can be marked `complete`.

## 8. Implementation checklist

- [ ] Add `#[cfg(target_os = "macos")]` event loop creation in `host.rs`
- [ ] Add `run_engine_macos` variant that runs on the main thread
- [ ] Update `main.rs` to create the event loop on the main thread on macOS
- [ ] Add `macos-latest` to CI matrix
- [ ] Run full gate on macOS
- [ ] Flip `cross_platform.verification.macos` from `not_implemented` to `complete`
- [ ] Update `docs/known-gaps.md` to remove the macOS blocker entry

## 9. References

- ADR-001: WebView strategy (no engine patching)
- ADR-012: Dependency policy (unsafe_code ban)
- `G:/nc-ui/docs/known-gaps.md:153-176` — tao macOS main-thread panic
- `G:/nc-ui/protocols/feature-registry.json:1913-1915` — macos not_implemented
