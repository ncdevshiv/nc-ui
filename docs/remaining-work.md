# Remaining work

The registry records 35 tracked capabilities: 34 complete, 1 absent. This file is the plan
for the one that remains, and records what building it needs.

Six capabilities were recorded here as absent and are now delivered. They were marked blocked
on things this machine turned out to have — a Linux environment, a signing key, a component ABI
that needs no runtime to be specified — so the list was worth re-checking rather than
inheriting. The one capability still absent is the one that genuinely needs hardware this
project cannot fake.

The three that were buildable here — `runtime.mcp_adapter`, `process.fuzzing` and
`process.soak_and_leaks` — are done and registered. The sections that described them are
retained below as a record of what each required, because the verification split they settled
is the part worth carrying forward: **the property half runs in CI; the libFuzzer half needs a
sanitizer runtime this machine does not have.**

The ordering rule is the project's own: a capability is complete when it is implemented,
tested, integrated, observable, benchmarked and documented — so **a feature that cannot be
verified on this machine stays `absent` rather than being marked done.**

## What was delivered

| Capability | Where it lives |
| --- | --- |
| `runtime.mcp_adapter` | `crates/ncui-runtime/src/mcp.rs` |
| `process.fuzzing` | `fuzz/fuzz_targets/`, plus property tests in three crates |
| `process.soak_and_leaks` | `crates/ncui-runtime/tests/soak.rs` |
| `webview.host` | `crates/ncui-webview/` |
| `a11y.support` | `crates/ncui-a11y/` |
| `abi.component_abi` | `crates/ncui-abi/` |
| `process.supply_chain_releases` | `tools/build_release.py`, `.cargo/config.toml` |
| `cross_platform.verification.linux` | `tools/linux_gate.py` |

## What remains

### `gpu.islands`

A rectangular page region rendered by the GPU rather than the DOM, embedded in ordinary HTML.

This one is genuinely blocked, and the reason is worth stating precisely because every other
entry on the original list turned out not to be:

- **ADR-002 requires the full input contract** — mouse, keyboard, focus, scrolling, clipboard,
  selection, resize, high-DPI, drag and drop, IME, and accessibility. An island that is a bare
  canvas and drops the rest "is not acceptable", so a partial implementation is a defect
  rather than progress.
- **The half that can be built without a GPU is now built.** `ncui-a11y` holds the semantic
  tree, focus order, keyboard routing and `IslandDescriptor`, which is what ADR-002 asks
  accessibility to be checked against. That was deliberately sequenced first.
- **A GPU device is present** on this machine (an RTX 3070) and a window can be created and
  shown, which is more than CI can offer — but a device being present is not the same as a
  renderer existing, and nothing here renders anything yet.
- **What is missing is the renderer**, not the device: a typed binary representation for the
  hot path, device-loss recovery as a tested path, and the composition boundary ADR-002 calls
  "the highest-risk area in the renderer".

Recording it as complete on the strength of `IslandDescriptor` would be exactly the error this
project's own rules exist to prevent.

## The sequencing that follows from the blockers

```text
done                mcp_adapter · fuzzing · soak_and_leaks · webview.host
                    a11y.support · abi.component_abi · supply_chain_releases
                    cross_platform.verification.linux
remaining           gpu.islands
needs a Mac host    cross_platform.verification.macos
```

`cross_platform.verification` was one entry covering two platforms and is now two, because
Linux was genuinely run and macOS was not — and a single entry would have let the completed
half hide the unrun one. macOS needs a host; nothing about the gate is missing.

`process.fuzzing` was recorded as unverified because the x64 AddressSanitizer runtime is
absent on the Windows host. It ran under WSL2, where Rust ships its own, and the numbers are
in `docs/known-gaps.md`.

`gpu.islands` and `a11y.support` are deliberately coupled: the accessibility work is
cheaper to do first, and doing islands first would mean retrofitting a semantic tree into a
rendering path that has none.