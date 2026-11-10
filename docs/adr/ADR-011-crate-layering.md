# ADR-011: Crate layering

**Status:** Accepted · **Date:** 2026-10-01

## Context
The failure mode of a large Rust project is not slow code; it is a dependency graph in which
every module can see every other module, so nothing is independently testable and regressions
have unbounded blast radius.

## Decision
A **strict total order of layers**. A crate may depend only on crates at *strictly lower*
layers:

```text
0  ncui-core
1  ncui-observability  ncui-security  ncui-ipc  ncui-state  ncui-scheduler
1  ncui-a11y  ncui-abi
2  ncui-workers  ncui-webview  ncui-gpu
2  ncui-app  ncui-native  ncui-component
3  ncui-webview-wry  ncui-gpu-renderer
4  ncui-runtime
5  ncui-cli  ncui-bench
```

The table above is a reading of the authoritative one, `LAYERS` in
`tools/ncui-tools/src/arch.rs`, which is what the checker enforces; when the two disagree, the
code is right and this table is the bug.

`ncui-gpu` is the compositing boundary's decision logic — hit testing, routing, selection and
input state. It currently has **no internal dependencies**, so the layer rule does not force its
position; layer 2 is a semantic grouping that keeps it beside `ncui-workers` and `ncui-webview`,
the other crates that exist to serve a host rather than to be depended on.

Layer 3 holds the **platform adapters**: the crates that implement a layer-2 port against a
real platform library. `ncui-webview-wry` implements `ncui-webview`'s `WebViewBackend` and
`HostFactory` over `wry`/`tao`; `ncui-gpu-renderer` implements the compositing boundary's
rendering half over `wgpu`. They sit *between* the port and the runtime, which is what lets
the runtime depend on a real host without depending on the policy that describes one — and
what keeps `wry` and `wgpu` out of every crate that does not actually talk to a platform.

The rule is enforced mechanically by `ncui-arch`, which reads the real graph from
`cargo metadata` rather than parsing manifests, so it reflects what Cargo will build. Adding a
crate without assigning it a layer is an **error**, not a warning: an unlayered crate would
silently bypass every boundary rule.

An explicit cycle search runs in addition to the layer check, so a hand-introduced cycle is
reported as a cycle rather than as a confusing layer complaint.

## Consequences
- Modularity means "one cohesive responsibility per module, explicit boundaries, independently
  testable capabilities" — not one function per file.
- The tooling lives in a separate workspace so a tooling bug cannot affect a runtime build and
  the runtime's dependency tree stays small.
- Layer 1 crates are mutually independent by construction, which is what makes parallel work
  and isolated testing possible.

## Implementation status
**Implemented and enforced.** `tools/ncui-tools/src/arch.rs` implements the layer table, the
loader, the boundary check, and cycle detection, with tests covering an inverted dependency, a
same-layer dependency, an unlayered crate, and a genuine cycle.
