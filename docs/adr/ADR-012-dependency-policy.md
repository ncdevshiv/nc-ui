# ADR-012: Dependency policy

**Status:** Accepted · **Date:** 2026-10-01

## Context
NC-UI links a WebView and drives a GPU driver. A compromised transitive dependency in that
position is a compromised application, running with the user's privileges. The ecosystem also
makes it trivially easy to accumulate hundreds of transitive dependencies by typing `cargo add`.

## Decision
**Minimise dependencies as a design goal, not an accident.** `deny.toml` makes the policy
machine-checked:

- wildcards and unregistered sources are **denied**; crates.io is the only registry, so the
  runtime is buildable without reaching a private index;
- known vulnerabilities and yanked versions are **denied**;
- the licence allow-list is explicit, so a copyleft or unknown licence surfaces during CI rather
  than at release;
- `unsafe_code` is **forbidden** in the runtime workspace.

The count and shape of dependencies is a reviewable property. When a subsystem can be built on
`std`, it is built on `std`.

## The one exception: `getrandom`

`ncui-security` depends on `getrandom`, and nothing else in the workspace does.

The reason is specific. `ncui serve` mints a bearer token, and `std` exposes no API for
`BCryptGenRandom` on Windows. Calling it directly requires `unsafe`, which
`unsafe_code = "forbid"` rejects. The alternatives were all worse than adding a dependency:

* return an error and leave `ncui serve` unable to start on Windows — which is what the
  code did before this decision, and is defensible but leaves a headline feature unusable on
  one of the three target platforms;
* fall back to `RandomState`, `SystemTime` or `SplitMix64` — all predictable, and a
  predictable token that *looks* random is worse than no token because it destroys the
  reader's trust in every token the system mints;
* hand-roll the FFI behind a `#[allow(unsafe_code)]` — rejected, since the crate-wide ban
  exists precisely so that this kind of local exception does not accumulate.

`getrandom` performs the FFI internally, so this project's own code remains free of `unsafe`
and the ban holds where it matters. Unix keeps using `/dev/urandom` directly and needs no
dependency at all; `getrandom` is only the Windows path.

This is recorded as a deliberate exception rather than a slip. It is the difference between
"this platform cannot start" and "this platform works", and it is confined to one module for
one purpose.

## Consequences
- Some conveniences are reimplemented rather than imported, which is a real cost paid for a
  smaller and more auditable attack surface. This is a deliberate trade, not an oversight.
- Supply-chain checks (audit, licence, SBOM) are CI gates, not release-week chores.
- The decision is revisited if a dependency's maintenance status or licence changes.

## Implementation status
**Implemented.** `deny.toml` is in place, the CI supply-chain job runs `cargo deny check` and
`rustsec/audit-check`, and `unsafe_code = "forbid"` is set workspace-wide.
