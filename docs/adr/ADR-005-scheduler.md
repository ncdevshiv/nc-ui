# ADR-005: Scheduler

**Status:** Accepted · **Date:** 2026-10-01

## Context
A desktop runtime must never trade responsiveness for throughput. Background work — indexing,
search, parsing — must not starve input or rendering.

## Decision
A **priority-aware cooperative scheduler** with these classes, highest first: `Input`,
`VisibleUi`, `GpuRendering`, `Animation`, `Interactive`, `Background`, `Maintenance`.

The scheduled priority is the *static class plus an ageing bonus* that grows with time spent
waiting. Ageing is what makes the no-starvation guarantee achievable rather than aspirational.

Scheduling is **cooperative**: a frame dispatches a bounded time budget, runs until it is
exhausted, and returns a report. One long task therefore cannot monopolise the frame.

## Invariants
- Interactive work cannot be permanently starved.
- A cancelled task cannot execute after `cancel()` returns.

Both are tested with a deterministic, clock-injected harness — no sleeping, no wall-clock
assertions in gate tests.

## Consequences
- The clock is injected rather than read internally, so ageing and deadlines are testable and
  reproducible. This is a design constraint, not a test convenience.
- Budgets must be tuned against measured frame times, not guessed.

## Rejected alternatives

**"A long-waiting background task eventually outranks newer interactive work."** This was
requested and was not implemented, deliberately.

Ageing that can lift a task above the interactive classes would make the no-starvation
guarantee false: under a saturated background backlog, a promoted maintenance task would
displace interactive work indefinitely, which is precisely the failure the invariant
exists to prevent. The invariant was kept and the promotion ceiling was lowered instead.

The enforced bound is explicit and asserted:

```text
score(Maintenance, wait = ∞)  = 2047
score(Background,  wait = ∞)  = 2046
score(Interactive, wait = 0)  = 2048
```

Ageing therefore promotes work *within* its own class, never across the protected
boundary. `unprotected_ageing_is_bounded_below_the_protected_floor` asserts the arithmetic
directly at an extreme wait, and
`a_saturated_background_stream_never_starves_input` runs 10,000 rounds against a background
backlog at maximum ageing and requires input to be dispatched on the first pop.

### Why `Background` saturates two units below the floor

An earlier revision gave both unprotected classes the same margin, so at full ageing
`Maintenance` and `Background` both scored 2047 — *level* with each other. That satisfied
the protected-boundary invariant above while defeating its purpose: an equal score is a
tie, a tie is broken FIFO, and a maintenance task behind a permanent background backlog then
waited for all of it. The anti-starvation argument that produced this ceiling assumed
"level with" was enough; it is not, because a tie is not a promotion.

`Background` therefore gives up the single unit that lets `Maintenance` saturate strictly
above it. The protected boundary is untouched: both remain below `Interactive` at 2048, so
the bound this ADR defends still holds, and it is asserted for every class rather than for
`Background` alone.

If the no-starvation invariant is ever relaxed, this decision should be revisited — it is a
trade between two good properties, not a bug.

## Implementation status
**Implemented.** `ncui-scheduler` provides the priority model, ageing, a bounded ready queue,
the cooperative budget, cancellation, and deadline expiry, with both invariants tested.
