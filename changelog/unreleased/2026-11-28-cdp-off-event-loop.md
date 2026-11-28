# CDP exchanges no longer block the engine loop; editor-grade latency proven end to end

- A real architectural defect, found by measurement: `HostEvent::CdpRequest` was
  handled on the engine event loop, and the blocking websocket connect+exchange
  (up to the 30 s timeout) dammed every queued `HostEvent::Eval` behind it — any
  CDP consumer froze page responses for the length of its exchange. In-page
  timestamps proved the shape: every evaluated script flushing in the same
  millisecond at t=30 s, one second after the bridge's own timeout had deleted
  the pending entry. The arm now hands the clonable endpoint binding to a worker
  thread (`send` already connects fresh per command), so the loop that serves
  pages never blocks on the transport that debugs them.
- `command_roundtrip_live.rs` closes the `ipc.editor_grade` latency question end
  to end: 200 sequential `window.ncui.invoke` round trips in a real WebView2
  window, timed by the page's own clock with the production-shape pump (a
  dedicated thread paced by `wait_page_message`, because every interleaved pump
  design starves behind blocking CDP calls) — **p50 0.5 ms, p90 0.7 ms,
  p99 3.6 ms, max 3.9 ms**, inside the register's ≤ 8 ms interaction budget,
  transport included. The harness itself carries the lessons: measurement in
  the served page not in CDP payloads (large evaluated scripts hang the
  endpoint), idempotent triggers (CDP evaluates are at-least-once), transient
  transport tolerance with honest deadlines. Supersede semantics for stale
  scrub commands remains the item's one open piece, named in the register.
