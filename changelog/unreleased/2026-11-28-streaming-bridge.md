# 2026-11-28 — streaming bridge primitive

- **The runtime grows a streaming bridge: one open, many events, honest
  cancellation.** `Runtime::open_stream(window, event)` answers a subscription
  id and a cloneable `StreamSink`; the producing thread owns the sink outright
  and posts events into the page's `ncui.on` handlers through the window
  registry — which is now Arc-shared inside the runtime so hot paths never
  convoy behind the runtime lock, the same lesson the video producer handle
  taught, applied here at design time. Cancellation is three truths: explicit
  close, the window dying, the runtime ending; the sink reads closure
  pull-shaped, a post into a dead window marks the stream closed rather than
  pretending, and the inspect truth counts delivered and failed posts exactly.
  Live evidence: twenty events at interaction rate arrived in a real page's
  handler in order with payloads intact, counts exact throughout
  (`crates/ncui-runtime/tests/stream_bridge_live.rs`).
