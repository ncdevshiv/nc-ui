# ADR-016: The component host

**Status:** Accepted · **Date:** 2026-10-23

## Context

`ncui-abi` (ADR-004) specified a component ABI: a wire format for a manifest, version
negotiation, and capability narrowing. It specified all of that and executed none of it, and it
said so in its own module documentation. The registry recorded the capability as complete, which
was true of the gate and false of the thing the gate was for: a component could be described,
negotiated and refused, but never *run*.

That is the shape of claim this project treats as a defect. A specification with no
implementation is not a partial feature; it is a document that reads like one.

## Decision

`ncui-component` executes components. It is the only crate that may interpret guest code, and it
holds no policy of its own: it reads a manifest, calls `ncui_abi::negotiate`, and instantiates
with exactly what the negotiation returned.

**The interpreter is `wasmi`.** It is a pure-Rust WebAssembly interpreter, so this project's own
code stays free of `unsafe`, which `unsafe_code = "forbid"` requires. ADR-012 records it as the
third dependency exception; the tree it adds is 18 crates, and it is confined to
`ncui-component`.

**The contract is written down once**, in `ncui-component/src/contract.rs`: the exports a module
must provide (`memory`, `ncui_abi_version`, `ncui_manifest`, `ncui_scratch`, `ncui_invoke`), the
two imports it may use (`ncui::capability_check`, `ncui::host_call`), the packed
`(pointer << 32) | length` convention that carries a document across the boundary, and the
negative refusal codes a host call returns.

**Nothing runs at instantiation.** The engine is configured with `allow_start_fn(false)`, so a
module carrying a start section is refused at compile time rather than executed at load. Loading
a component is not running one.

**The manifest is read through a store that cannot reach the host.** This is the part that took
the longest to get right. A manifest is produced by the component's own code — the host calls
`ncui_manifest()` and reads the bytes back out of guest memory — so the read is itself an
execution of untrusted code, and it happens *before* any negotiation exists. The read therefore
runs in a store with an empty grant, no declared services and no gateway: every `host_call`
during declaration is refused by construction. `a_component_cannot_reach_the_host_while_it_is_declaring_itself`
asserts both halves — the gateway was never called, and the same service *is* served after the
component has been negotiated with.

**Execution is bounded four ways**, because each bound catches what the others do not:

* **Fuel**, refilled per call, so a component that does not return is stopped rather than
  allowed to hold the process. Exhaustion is a catchable trap.
* **A memory limiter** with `trap_on_grow_failure(true)`. The specification has `memory.grow`
  return `-1` when it cannot grow, and a component that ignores that answer would go on writing
  into memory it does not have; a trap is the honest reading of "you may not have that".
* **A copy budget** on every guest-memory read, so a component handing the host a length of four
  billion gets a refusal rather than a four-gigabyte allocation.
* **A response bound** on every result, checked before the bytes are read out of guest memory.

**A service is reachable only if the manifest declared it and the negotiation granted the
capability behind it**, and the check happens in that order. A component cannot ask for a
service it did not publish, and cannot use a capability it was not given, by asking at call
time.

**A component reaches application commands and nothing else.** `PageCommands::component_call`
serves a `Native` handler whose requirements the component's narrowed grant covers; a built-in
runtime method is refused with the reason, because those are reached through the agent pipeline
with the agent's authority and its audit trail. Giving a component a shorter route would make
every "who is asking" decision the agent surface makes decorative.

**`run.components` is a capability of its own, and it is destructive.** It is not an authority
over a resource but permission to execute code the runtime did not write, so the existing
consent gate and audit trail apply to every call that loads or drives a component. A page that
may write a file may not thereby run one.

## Consequences

* The ABI's wire format is now load-bearing rather than descriptive: `ncui_abi::wire` parses the
  manifest out of the artifact, refusing unknown fields and unknown capability names, and the
  negotiation runs on what it parsed.
* A component is one artifact, not an artifact plus a sidecar that can disagree with it.
* Calls into one component are serialised behind a mutex, because a store is single-threaded by
  construction. A component that needs concurrency gets one instance per caller, which the
  runtime's `component.open` supports by name.
* Not implemented, and recorded as such in `docs/known-gaps.md`: WASI, threads, SIMD, and the
  component-model binary format. A component here is a core WebAssembly module speaking this
  contract.
* The `component.*` commands are registered only when the page grant covers `run.components`, so
  an application that does not ask for it has no component surface at all rather than a surface
  that refuses.

## Alternatives rejected

* **Hand-write an interpreter.** Rejected for the reason the GPU and clipboard work rejected
  hand-rolled FFI: an interpreter is a large, adversarial surface, and a home-grown one would be
  a second implementation of a specification with its own bugs to find. `wasmi` is pure Rust and
  already the subject of far more scrutiny than a new one would receive.
* **Load a native library instead.** A `.dll`/`.so` component would need `libloading`, which
  needs `unsafe`, which the workspace forbids — and it would hand a component the host's own
  address space rather than one linear memory the host can bound.
* **Use a component-model runtime.** Larger dependency tree, and the binary format is still
  moving; the properties this runtime needs (a bounded store, a fuel meter, a memory limiter)
  are the ones the core specification already provides.
* **Ship precompiled artifacts.** A committed `.wasm` blob is a binary whose provenance no
  reviewer can check. Every fixture in this repository is WAT text assembled at test time, so
  the artifact under review is the artifact in the tree.
