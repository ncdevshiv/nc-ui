# ADR-009: Observability

**Status:** Accepted · **Date:** 2026-10-01

## Context
A system with GPU surfaces, worker threads, and a web engine is opaque without deliberate
instrumentation. Retrofitting correlation across boundaries after the fact is expensive and
usually incomplete.

## Decision
Use the **`tracing` ecosystem** for structured logging and spans, with an
**OpenTelemetry-compatible** model for metrics and traces. Crash reporting is optional and
complements rather than replaces local diagnostics.

**Correlation is the spine.** One `CorrelationId` accompanies a logical operation across input,
WebView event, DOM component, IPC, Rust command, worker, filesystem, state update, and GPU
render. An error that cannot be joined to its trace is not a useful error.

Production code never uses `println!`, `console.log`, or `dbg!`; the lints deny them.

Two anti-silencing rules:

- a metric series evicted by the cap **increments an eviction counter**, so bounded memory is
  visible rather than lossy in silence;
- percentiles are always reported (p50/p90/p95/p99/max) — averages alone are explicitly
  forbidden, because an average hides exactly the tail that hurts.

## Consequences
- Instrumentation is a definition-of-done item, not a follow-up.
- Bounded retention (log rings, metric series, histogram samples) is explicit and reported.
- Telemetry must work with no external service; open protocols only, no vendor lock-in.

## Implementation status
**Implemented** for tracing setup, correlation, the bounded metrics registry with real
percentiles, the bounded log ring, and the JSON Lines exporter.
