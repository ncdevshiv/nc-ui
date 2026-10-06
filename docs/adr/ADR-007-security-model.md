# ADR-007: Security model

**Status:** Accepted · **Date:** 2026-10-01

## Context
NC-UI runs untrusted web content and accepts commands from autonomous agents over a local
socket. Both are attack surfaces, and the second is easy to under-protect because "loopback"
feels safe.

## Decision
**No ambient authority.** Every filesystem read, process spawn, socket open, and diagnostic read
is gated on an explicitly granted `CapabilitySet`, checked at the point of use, not at the
boundary.

Specific commitments:

- **Loopback is not trusted.** The agent API requires a bearer token, validates every parameter,
  enforces a rate limit, and records an audit event for every action.
- **Destructive capabilities require per-call authorization**, even when held. Holding
  `delete.data` authorises the *possibility* of deletion, not the next call.
- **Secrets never reach** source, logs, traces, metrics, or crash reports. `SecretToken` redacts
  itself in `Debug` and `Display`, and comparison is constant-time.
- **Path containment is decided by path component, not string prefix.** `C:\rootevil` is not
  inside `C:\root`, and a traversal payload must never escape its root.
- **Agent roles are explicit:** Observer, Debugger, Tester, Developer, Administrator. Observer
  holds no workspace-scope and no destructive capability.

## Consequences
- Security is a crate with an adversarial test suite, not a review checklist.
- `SafePath` is tested with randomly generated malicious inputs, asserting the invariant that it
  returns either a contained path or an error — never an escaping path.
- Where std alone cannot provide a secure primitive, the API requires the caller to inject it
  rather than pretending to provide security. A non-random `generate` is worse than no
  generator, because it looks safe.

## Implementation status
**Implemented** for authorization, roles, destructive-action gating, rate limiting, token
redaction and constant-time comparison, path containment, and the audit trail.
