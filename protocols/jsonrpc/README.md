# NC-UI agent protocol (JSON-RPC 2.0)

**Status:** Specification · **Date:** 2026-10-01

This document defines the machine-readable interface an agent uses to inspect, test, profile and
operate an NC-UI runtime. It is the specification the implementation is written against; where
the two disagree, that is a defect in one of them and the specification is the tie-breaker.

## Design rules

Three constraints shape everything below.

1. **Discovery before use.** An agent must be able to learn what the runtime supports and what
   *it* is permitted to do by asking, not by reading source or guessing. `ncui.describe` is
   the entry point.
2. **MCP is an adapter, never the internal architecture.** This JSON-RPC surface is the
   runtime's own interface. MCP is layered on top of it (ADR-008). If MCP were the internal
   shape, an external agent protocol's constraints would become core design.
3. **Errors are structured and stable.** Every failure carries a machine-readable `code` from a
   closed set, plus the operation and the correlation identifier. Error codes are a public API
   surface: renaming one is a breaking change.

## Transport and authentication

- **Loopback is not trusted.** The server binds to loopback only, and still requires
  authentication. An agent on the same machine is not automatically authorised.
- Every request carries a bearer token. Tokens are compared in constant time.
- Requests are rate limited per token. Limits come from `Limits::agent_rate_per_second` and
  `Limits::agent_rate_burst`.
- Every request that performs work produces an audit event (see *Audit* below).

## Request shape

Standard JSON-RPC 2.0. NC-UI reserves the `ncui.` prefix for its own methods.

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "method": "ncui.health",
  "params": { "deep": true }
}
```

`params` is always an object. Positional parameters are not accepted, because they make a
method's signature unreadable and are incompatible with adding optional fields.

## Response shape

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "result": { "status": "ok", "checks": [ ... ] }
}
```

or

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "error": {
    "code": -32001,
    "message": "capability `read.logs` is not granted to agent",
    "data": {
      "ncui_code": "capability_denied",
      "operation": "agent::authorize",
      "correlation": "0f3a...",
      "retryable": false
    }
  }
}
```

`error.data.ncui_code` is the stable machine-readable code. `error.data.correlation` joins the
failure to its trace.

### Error codes

| JSON-RPC code | `ncui_code` | Meaning |
| --- | --- | --- |
| -32600 | `validation` | Malformed request or invalid parameter |
| -32601 | `protocol` | Unknown method |
| -32602 | `validation` | Invalid params |
| -32603 | `internal` | Runtime defect — a defect, not bad input |
| -32001 | `capability_denied` | The agent lacks a required capability |
| -32002 | `limit_exceeded` | Rate or resource limit exceeded |
| -32003 | `timeout` | The operation exceeded its deadline |
| -32004 | `cancelled` | The operation was cancelled |
| -32005 | `not_found` | The referenced entity does not exist |
| -32006 | `conflict` | Conflicts with current runtime state |
| -32007 | `unauthorized` | Missing or invalid token |
| -32008 | `backpressure` | A bounded queue is saturated |

## Methods

Every method declares the capability it requires. A method whose required capability is not
held returns `-32001` and performs no work. A method marked **destructive** additionally
requires explicit `consent: true` in its `params`; the default is refusal.

### Implemented

These are dispatched today. Each is covered by
`crates/ncui-runtime/tests/agent_surface.rs`, which asserts that the advertised requirement
is the one actually enforced.

| Method | Required capability | Destructive | Description |
| --- | --- | --- | --- |
| `ncui.describe` | none | no | Runtime identity, methods, roles, capabilities and the caller's own grants |
| `ncui.health` | none | no | Liveness, readiness and per-subsystem detail (`deep`) |
| `ncui.feature.status` | none | no | The feature registry, when the host loaded one |
| `ncui.workers.inspect` | `inspect.runtime` | no | Worker lifecycle, queue depth and task counters |
| `ncui.scheduler.inspect` | `inspect.runtime` | no | Ready-queue depth against capacity |
| `ncui.logs.query` | `read.logs` | no | Bounded log ring, filtered by level and correlation |
| `ncui.logs.purge` | `read.logs` | **yes** | Discards the log ring; requires `consent: true` |
| `ncui.metrics.query` | `read.metrics` | no | Metric snapshot with percentiles |

### Planned, not implemented

These are specified so the interface is stable, but **they do not exist**. Each is recorded
`not_implemented` in `protocols/feature-registry.json`, so an agent reading either document
agrees with the other.

| Method | Required capability | Blocked on |
| --- | --- | --- |
| `ncui.capabilities.describe` | none | — |
| `ncui.components.list`, `ncui.component.inspect` | `inspect.runtime` | `webview.host` — there is no component tree to inspect yet |
| `ncui.window.list` | `inspect.runtime` | `webview.host` |
| `ncui.state.inspect` | `inspect.runtime` | — |
| `ncui.gpu.inspect` | `inspect.gpu` | `gpu.islands` — no GPU surface exists yet |
| `ncui.ipc.inspect` | `inspect.runtime` | — |
| `ncui.trace.start` / `.stop`, `ncui.profile.start` / `.stop` | `inspect.runtime` | — |
| `ncui.test.run`, `ncui.benchmark.run` | `run.tests`, `run.benchmarks` | — |

The "Blocked on" column is deliberate: `components.list` and `window.list` are not merely
unwritten, they have nothing to report until a WebView host exists, and `gpu.inspect` until
a GPU surface does. Publishing them as merely "planned" would hide that dependency.

## Pagination and limits

Every method that returns a collection accepts `limit` (default 100, maximum 10 000) and an
opaque `cursor`. Exceeding the maximum is a `validation` error, not a silent clamp — a silently
clamped result reads as "this is everything" and is worse than a refusal.

## Cancellation

Long-running methods accept an optional `deadline_ms`. On expiry the operation is cancelled and
the request returns `-32003`. A timeout must generate observable diagnostics; a timeout is never
used to conceal a correctness problem.

## Audit

Every method invocation produces an audit event containing `agent_id`, `request_id`,
`correlation`, `method`, `target`, `result`, and the capabilities used. Destructive actions
require an explicit `authorized: true` field in `params`; holding the capability is necessary
but not sufficient (ADR-007).

## Versioning

The interface is versioned independently of the runtime via `SemVer::is_compatible_with`. A
pre-release agent client will not bind to a stable runtime, and a stable client will not bind to
a pre-release runtime.