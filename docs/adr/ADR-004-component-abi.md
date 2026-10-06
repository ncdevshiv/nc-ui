# ADR-004: Component ABI

**Status:** Accepted · **Date:** 2026-10-01

## Context
The component model must not be fundamentally tied to React, TypeScript, Rust, or any single
language, or "one application model with multiple execution strategies" is not real.

## Decision
A **language-neutral NC-UI Component Protocol** is the fundamental abstraction. It defines
component identity, version, properties, events, children, commands, lifecycle, state, rendering
capabilities, resources, permissions, accessibility, serialization, and the error model.

WASM via the **WebAssembly Component Model** is the primary cross-language execution target, with
native bindings where WASM is inappropriate.

Versions negotiate explicitly through `SemVer::is_compatible_with`, which is caret-range and
directional: a pre-release peer only satisfies a requirement sharing its exact
`major.minor.patch`, so a consumer can never silently bind to an unreleased build.

## Consequences
- The ABI is a public compatibility surface, versioned independently of the runtime.
- A component written in another language must be observable through the same introspection API
  as a Rust one; anything less makes the abstraction cosmetic.
- Capability declarations in the ABI are the enforcement point, not documentation.

## Implementation status
**Not implemented.** No ABI crate exists. `ncui_core::SemVer` implements the negotiation rules
this ABI will depend on, and that part is tested.
