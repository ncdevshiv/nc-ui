# ADR-003: IPC architecture

**Status:** Accepted · **Date:** 2026-10-01

## Context
IPC is a core subsystem, not plumbing. Serialise to JSON, copy, send, parse, copy again is the
default outcome of naive designs and is unacceptable on hot paths.

## Decision
A **binary framed protocol** is the transport for the hot path; JSON-RPC is the control plane.
Every frame carries a correlation identifier so one logical operation is traceable across every
boundary.

Every queue is **bounded** with an explicit backpressure policy (`Block`, `DropOldest`,
`Reject`). No unbounded queue exists anywhere in the runtime.

The protocol's invariants are load-bearing and tested as such:

- messages are never silently dropped;
- request identifiers are unique, and re-use is a conflict rather than a silent overwrite;
- a response maps to exactly one request;
- malformed input is rejected without allocating unboundedly and without crashing the runtime.

## Consequences
- The length field is validated *before* allocation, so a hostile frame cannot induce a large
  allocation. This is the single most important property of the decoder.
- Binary framing needs a version byte and a fuzz-style test that never panics; both are
  required, not optional.
- The JSON control plane and the binary hot path must not drift; both are covered by round-trip
  tests over the same logical messages.

## Implementation status
**Partially implemented.** The framing, envelope, correlation, cancellation, and backpressure
primitives are implemented in `ncui-ipc` and covered by tests. Transport binding to the
WebView, workers, and the agent socket is not yet implemented.
