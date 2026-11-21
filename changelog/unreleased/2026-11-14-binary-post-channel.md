- **A page can move a large buffer to the runtime as raw bytes, not as a JSON string.** The
page bridge carries every payload as a JSON string, so an application moving a big buffer pays
JSON's cost twice — escaped on the way out, parsed on the way in. The asset server now answers
`POST`: it reads the request body (bounded by `MAX_REQUEST_BODY_BYTES`, refused with a 413
before a byte is read when a client declares more) and hands those bytes to a route. A route
that moves a buffer registers against the same `Route` table it already uses, and the responder
now receives the body alongside the target. This is A1's reframed target — a binary *data*
channel for large payloads, not the small-round-trip latency the WebView boundary owns.

**Bounded, and a `POST` cannot reach a file.** The body is read once, up to
`MAX_REQUEST_BODY_BYTES` (64 MiB); a `Content-Length` over that is refused before allocation, so
a client cannot make the server allocate without bound. A `POST` is answered only by a route
that claims its path — a `POST` to an ordinary file path is refused with a 405 rather than
falling through to the file server, because the file server serves `GET` and `HEAD` and must not
hand a file's bytes to a `POST`. A missing or malformed `Content-Length` is an empty body rather
than a guess.

**Two bugs the end-to-end tests caught, not the compiler.** The first version let a `POST` to a
file path fall through to the file server — the method guard had moved, so `POST /index.html`
would have served the file's bytes to a `POST`. The byte-for-byte test caught it. The second
omitted the response body for a `POST`: `write_response` was told "include the body" only for
`GET`, so a `POST` answer carried a `Content-Length` and no body, and the client hung reading.
Both are the kind of thing only a test that drives a real socket and checks the real bytes
finds; the byte-echo test reads the response body as raw bytes, because a lossy UTF-8 copy would
have hidden the corruption.

**Measured with a payload-size sweep, against a real listener over a real socket.**
`binary_channel/round_trip` in `ncui-bench` posts buffers of 1 KiB to 1 MiB at an echo route and
reads them back, so the number is a transport measurement rather than an encoding estimate. On
this Windows host the round trip is ~150 µs fixed (connect, head, read) and throughput climbs to
~176 MiB/s at a 64 KiB payload — the channel is not the bottleneck at application buffer sizes.
The sweep exists to show where the binary channel wins over JSON-in-`postMessage`; the
small-payload latency row (A1's original target) is owned by the WebView boundary, not the
encoding, and is measured separately.

**Tested end to end over real TCP connections.** The `ncui-app` asset tests open a real socket to
a real listener and speak HTTP to it; four new tests cover the `POST` path — a body of arbitrary
bytes (0–255, not valid UTF-8) echoed byte for byte, an empty body served as empty, an
over-limit body refused with a 413 before it is read, and a `POST` to a non-route path refused
rather than leaking a file. The existing island and reload routes were updated to ignore the
body a `POST` would carry. Gate: `cargo fmt`, `cargo clippy --workspace --all-targets`
(pedantic, no warnings), and the full `cargo test --workspace` suite all pass.
