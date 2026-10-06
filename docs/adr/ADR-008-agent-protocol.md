# ADR-008: Agent protocol

**Status:** Accepted · **Date:** 2026-10-01

## Context
An agent that can only *run* an application cannot help fix it. Diagnosis requires reading logs,
metrics, traces, component state, GPU state, IPC state, and worker state — the question is "why
is this slow", not "does it start".

## Decision
Two layers over one capability API:

- **JSON-RPC 2.0** is the structured interface for introspection, testing, profiling, and runtime
  control.
- **MCP** is the external interface for AI clients.

MCP is an *adapter*, never the internal architecture. If MCP were the internal shape, the runtime
would inherit an agent protocol's constraints as core design.

Every agent action is **capability-gated, rate-limited, and audited** with agent id, request id,
action, target, and result, so agent behaviour is reproducible after the fact.

## Consequences
- Method names and error codes are a public API surface and are versioned explicitly.
- `ncui describe` is the discovery entry point: an agent learns what it may do by asking, not by
  reading source.
- Local diagnostics must work with no external service, so the truth layer is queryable offline
  (spec 100, 101).

## Implementation status
**JSON-RPC: implemented. MCP: implemented as an adapter.**

`ncui-runtime` serves the eight-method surface over a loopback socket (`transport.rs`), on the
in-process path (`dispatch.rs`), and through MCP (`mcp.rs`). The adapter dispatches through the
same `dispatch_with` call as every other path, so the capability check, the consent gate, the
rate limit and the audit trail are inherited rather than reimplemented — which is what
"adapter, never the internal architecture" has to mean in practice.

`ncui.feature.status` is served when the caller supplies a registry (`ncui serve --registry
<path>`); without one it reports that none is loaded, which is a truthful refusal rather than
the unconditional `NotFound` it used to return.
