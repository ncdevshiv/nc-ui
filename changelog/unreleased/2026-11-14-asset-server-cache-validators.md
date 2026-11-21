- **The asset server sends cache validators and honours conditional `GET`.** Every file
response now carries a strong `ETag` (modification time + served length) and a
`Last-Modified` HTTP-date, and a request echoing either back is answered `304 Not Modified`
with no body, so a `--watch` reload stops re-sending assets that did not change. Not breaking:
clients that send no validators see the same `200` as before. `If-None-Match` outranks
`If-Modified-Since` (RFC 9110 §13.1.3), an unparseable date is treated as no condition rather
than as a match (a false `304` would hand back an empty body), and the date comparison runs
in whole seconds because that is the resolution an HTTP-date carries. Unit-tested (date
conversion round-trips, a leap day, the unparseable-date and precedence rules) and
integration-tested over a real socket; measured by `app.asset_get_200` and
`app.asset_conditional_304` in the latency harness.
