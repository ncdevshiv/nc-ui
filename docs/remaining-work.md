# Remaining work

The registry records 72 tracked features: 71 complete, 1 absent. This file is the plan for the
one that remains, and records what building it needs — and, below, what the ones that used to
remain needed, because the reasoning that moved them is worth keeping.

Every capability recorded here as absent and since delivered is in the table below. They were
marked blocked on things this machine turned out to have — a Linux environment, a signing key, a
component ABI that needs no runtime to be specified — so the list was worth re-checking rather
than inheriting. The one still absent is the one that genuinely needs hardware this project
cannot fake.

The verification split they settled is the part worth carrying forward: **the property half runs
in CI; the libFuzzer half needs a sanitizer runtime this machine does not have.**

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

### `cross_platform.verification.macos`

The full gate suite run and passing on macOS. **Nothing about the gate is missing**: it needs a
machine, and no macOS host is reachable from the one that produced this registry. Every feature
whose evidence includes a test that only a Mac can run is recorded `unverified` there rather than
`verified`, which is why so many entries carry that word for that platform.

## What `gpu.islands` needed, and no longer does

A rectangular page region rendered by the GPU rather than the DOM, embedded in ordinary HTML.

This one is no longer blocked, and the reason is worth stating precisely because every other
entry on the original list turned out not to be.

**The renderer now exists and is verified on a real GPU.** `ncui-gpu-renderer` owns the
device, the surface, the render pass and the text layout path, and
`crates/ncui-gpu-renderer/tests/render_offscreen.rs` creates a real device, draws a frame
and reads the pixels back — so the assertion is on the output, not on the absence of a
crash.

What is built:

- **The frame format** (`frame.rs`) — the typed binary representation ADR-002 requires for
  the hot path, with the length checked before anything is allocated.
- **The render pass** (`render.rs`) — a pure `RenderPlan` that can be tested without a GPU,
  and the `wgpu` binding that draws it.
- **The surface** (`surface.rs`) — a window or an offscreen target, and the readback that
  makes the offscreen one verifiable.
- **Device loss recovery** (`device.rs`) — a supervisor that rebuilds a lost device and
  counts the rebuilds. The recovery path is exercised by a test that hands it a device which
  reports itself lost, so it is a *tested path* and not a claim.
- **Text layout** (`text.rs`) — HarfBuzz through `rustybuzz`, as ADR-002 asks: integrated,
  not re-implemented.
- **The accessibility contract** — `ncui-gpu` now depends on `ncui-a11y`, builds a semantic
  tree from the islands' descriptors, and satisfies all eleven of ADR-002's input items. The
  `Accessibility` arm used to be a permanent `false`; it is now driven by a probe against a
  live host.

**What was missing, and what moved the capability to `complete`:** the renderer was not yet driving
a *window*. A GPU island is a region of a page, and the region has to be composited with the DOM
around it — the renderer's surface has to be the webview's window, the two have to agree on the pixel
geometry, and the DOM's copy of the page has to be told which pixels the GPU owns. That composition
boundary is ADR-002's "highest-risk area in the renderer", and the section below records how each
part of it was built.

So the position now is that both halves are done and verified, and the capability is recorded
`complete` because the renderer *can* be composited with the DOM and that composition is exercised
by a test against a live host.

- **ADR-002 requires the full input contract** — mouse, keyboard, focus, scrolling, clipboard,
  selection, resize, high-DPI, drag and drop, IME, and accessibility. An island that is a bare
  canvas and drops the rest "is not acceptable", so a partial implementation is a defect
  rather than progress. **All eleven are now demonstrated by a probe against a live host.**
- **The half that can be built without a GPU is now built.** `ncui-a11y` holds the semantic
  tree, focus order, keyboard routing and `IslandDescriptor`, which is what ADR-002 asks
  accessibility to be checked against. That was deliberately sequenced first.
- **A GPU device is present** on this machine (an RTX 3070) and a window can be created and
  shown, which is more than CI can offer — but a device being present is not the same as a
  renderer existing.

**Delivered.** The three things this section listed as missing are all built and tested: the
island route serves frames on the page's own origin (`crates/ncui-runtime/src/islands.rs`), the
content of a region is rows of type rather than a colour (`ncui-gpu-renderer`'s glyph rasteriser
and text path, measured in `crates/ncui-bench/benches/latency.rs`), and an island's contents reach
a screen reader through a description the runtime serves from the same model it draws from, which
the page mirrors into the DOM (`examples/counter`). The registry records the feature `complete`,
with its evidence citing the ten tests in `crates/ncui-runtime/tests/islands.rs`.

### The window host

`ncui-webview-wry` now implements `HostFactory` over `wry`/`tao`, and the engine obeys
the policy rather than keeping a copy of it: a navigation is offered to the window's seam
and the seam's verdict is what the engine does, and a message from the page is forwarded
verbatim so that only the seam — which holds the grant — can act on it.

`crates/ncui-runtime/tests/page_bridge.rs` drives the whole path: a page posts a message,
the runtime parses it, checks it against the window's grant, dispatches it through the same
pipeline an agent's request goes through, and answers the page. A page that names an action
the host did not declare is refused; a malformed message is answered rather than dropped;
and the request appears in the audit trail under `page:<window-id>`.

**What was missing, and is now done:** the `wry-engine` feature used to be off by default, so
the binding was compiled and tested but not part of a release build. `ncui-cli`'s `wry-host`
feature now enables it — with `gpu-islands`, because the release build serves the island route
the counter example's canvas is — and the `adapters` CI job runs the workspace with
`--all-features` on `windows-latest`, which opens a real window on a real host. The binding is
therefore on the path a user's `ncui run` takes, not only on the path a contributor's build
takes.

### Why the contract report has a consumer, and what that left

`crates/ncui-gpu` answers "which ADR-002 items can a host actually demonstrate?" by driving a
host and observing each item's effect — not by returning a list. That replaced a check which
compared a cloned constant's length to itself and so passed on a host whose router was a no-op.

Its consumer is `ncui.gpu.inspect`, which reports the contract items the running runtime's host
demonstrates. The report is a property of a *live host*: one with islands, in a page, being
routed to.

Both things that used to block the wiring further are now true: a real `Islands` reaches the
runtime (`crates/ncui-runtime/src/islands.rs`), so the report is about the *running* page rather
than about whatever the runtime was given; and the GPU surface is drawn into a window
(`ncui-webview-wry`'s `gpu-surface`), so the capability is exercised end to end rather than
asserted. What is left is the ordinary work of a running system, not a missing component.

## The sequencing that follows from the blockers

```text
done                mcp_adapter · fuzzing · soak_and_leaks · webview.host
                    webview.engine · a11y.support · abi.component_abi
                    supply_chain_releases · cross_platform.verification.linux
                    gpu.islands · every capability the gaps document lists as
                    implemented: menus and file dialogs are the two it still
                    lists as absent, for the reasons it gives there
needs a Mac host    cross_platform.verification.macos
```

`cross_platform.verification` was one entry covering two platforms and is now two, because
Linux was genuinely run and macOS was not — and a single entry would have let the completed
half hide the unrun one. macOS needs a host; nothing about the gate is missing.

`process.fuzzing` was recorded as unverified because the x64 AddressSanitizer runtime is
absent on the Windows host. It ran under WSL2, where Rust ships its own, and the numbers are
in `docs/known-gaps.md`.

`gpu.islands` and `a11y.support` were deliberately coupled: the accessibility work is
cheaper to do first, and doing islands first would mean retrofitting a semantic tree into a
rendering path that has none. That ordering is now paid off — the semantic tree is wired
into the compositing boundary and the accessibility arm of the input contract is probed
rather than deferred.
