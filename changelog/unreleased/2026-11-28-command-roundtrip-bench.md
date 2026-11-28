# Editor-grade command latency measured

- `runtime.page_command_roundtrip` joins the latency bench: the whole native-side
  cost of one scrub-shaped page command — request JSON text parsed through the
  seam's page-facing `SeamRequest::parse`, capability checked, handler run,
  response serialized back to JSON text. On Windows dev hardware it measures
  p50 1.5 µs / p99 3.6 µs against the register's ≤ 8 ms interaction budget —
  roughly three orders of magnitude of headroom, with serialization the larger
  half of the cost (the constructed-message `page_command_dispatch` path alone
  is 800 ns, which is why measuring it was never the editor-grade evidence).
  What the number honestly does not include is the webview transport crossing,
  which no in-process bench can measure; that and supersede semantics for stale
  scrub commands remain the register's open pieces, named in `futureplan/what
  to add.md` §7. The platform baseline is re-recorded with the scenario included.
