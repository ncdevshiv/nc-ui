- **Windows CDP commands are bound to the window that requested them (ADR-017 Phase 2).**
  The process-wide WebView2 browser endpoint is no longer treated as a page identity. Each
  top-level `EngineWindow` records the exactly-one new `page` target observed during serialized
  creation; ambiguous or missing targets fail closed as `cdp_target_bind_failed`, without guessing
  from a title or URL. Page commands travel through `HostEvent::CdpRequest` with the engine's
  window identity, attach a flattened `Target.attachToTarget` session, replace any caller-supplied
  `sessionId`, and detach after the exchange on the same WebSocket connection because WebView2
  sessions are connection-scoped. `Browser.getVersion` remains browser-scoped.
- **CDP exchanges and failures are typed and correlated.** The dependency-free WebSocket client
  requires a positive integer command id, reads complete messages until the matching response,
  ignores events and unrelated response ids, handles ping/pong and fragmented messages, and
  enforces frame, aggregate-size, and end-to-end time bounds. Engine-neutral `CdpError` now crosses
  `WebViewBackend::cdp_send` directly; runtime dispatch, screenshot/PDF capture, JSON-RPC, and
  recognized IPC codecs preserve stable `cdp_*` codes instead of reporting every failure as a
  conflict. WebKitGTK still refuses with exactly `cdp_platform_unsupported`.
- **Coverage includes deterministic fake-WebSocket regressions and real two-window isolation.**
  Tests place events before responses, send mismatched ids, interleave ping/pong with fragments,
  verify session propagation and detach behavior, reject target-snapshot ambiguity, and drive
  distinct `Runtime.evaluate` values through two real Windows runtime hosts.
