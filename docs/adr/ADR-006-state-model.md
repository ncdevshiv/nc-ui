# ADR-006: State model

**Status:** Accepted · **Date:** 2026-10-01

## Context
Serialising the entire state, shipping it over IPC, and re-rendering the whole application on
every change is the default failure mode of this kind of runtime. It is the direct cause of
"inefficient state synchronisation" in existing web-desktop applications.

## Decision
State updates are **incremental deltas**. A mutation emits a `Patch` of ordered changes; a
subscriber receives only what is newer than the revision it last saw, filtered to what it
subscribed to.

Two rules protect correctness:

- changes to the same key within one patch **coalesce** (last write wins, order-stable);
- a subscriber that has fallen so far behind that changes were evicted is told to
  **resynchronise** rather than silently handed a gap. Silent gaps are worse than a full resync
  because they are invisible.

## Consequences
- Every store carries a monotonic revision; a revision is the resume point for any reader.
- The resync signal must be part of the protocol, not an optimisation, or correctness depends
  on timing.
- Virtualisation (a million rows without a million nodes) is a state concern as much as a
  rendering one.

## Implementation status
**Implemented.** `ncui-state` provides revisioned stores, coalescing patches, filtered
subscriptions, catch-up from an older revision, and the resync-required path, all tested.
