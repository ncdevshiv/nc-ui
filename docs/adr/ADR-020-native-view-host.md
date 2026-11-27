# ADR-020: Native view host

**Status:** Accepted · **Date:** 2026-11-24

## Context

NC-UI hosts two kinds of content. Pages are untrusted web content behind the seam
(ADR-001), and GPU islands are rectangular regions of a page rendered by the GPU engine
(ADR-002). Neither is a native widget layer: there is no first-party way to put a tree of
Rust-drawn controls — a settings panel, a picker, a palette — in front of a user.

nc-ui-kit, the sibling project, is a component library built on GPUI, the Rust UI
framework extracted from Zed. GPUI is compiled Rust in the host process. That single fact
decides the shape of everything else: a WASM component artifact is sandboxed precisely
because it is not native code (ADR-004), and GPUI cannot be one, because no WASM artifact
is Rust in the host process. Native views are therefore a **second trust class** of
content. A page is sandboxed; a native view is trusted by the act of host-linking, the
way `ncui-native` is trusted when it registers a hotkey or shows a file dialog. A
decision that pretended the two were the same class would either sandbox nothing or
forbid the capability outright, and both of those are ways to be wrong rather than
positions.

The questions that remain are the ones the trust class does not answer by itself: how a
window whose content is not a page is described, which engine creates it, what a page may
ask of it, and what its events cost the runtime.

## Decision

**A window's content is one of two kinds, carried inside the request.** `WindowContent`
is `Page` or `Native { provider, props }`, and the kind travels inside `WindowRequest`
rather than beside it, so every validation and every host sees the whole of what was
asked for in one place. A request defaults to `Page`, because every request before this
decision was one. Validation refuses what the kind cannot mean: a native request that
carries a URL is rejected rather than silently ignored, because a native window has no
page to navigate to and a caller's mistake should surface where it was made. The provider
identifier is namespaced lowercase ASCII (`nc-ui-kit::sonner`) — letters, digits and the
`.`, `_`, `-`, `:` separators, at most 128 bytes — because an identifier is printed into
diagnostics and compared against a registry, and a character set that cannot carry
whitespace or control characters cannot smuggle either into a log line. The props are
bounded at 64 KiB serialized: a page names them and host-linked code parses them, so they
cross the same trust boundary the URL does, and host memory is not the page's to spend.

**The contract lives in `ncui-webview`, beside the page contract.** Layer 2 gains the
types both sides share — `NativeViewEvent`, `NativeViewCommand`, `NativeViewError`, the
`NativeViewBackend` trait — and `RoutingFactory`, the factory that reads a request's
content kind and asks the host installed for that kind. Nothing in the module knows gpui
exists, which is why the contract is testable on a machine with no display. Native
windows skip the navigation policy entirely: there is no navigation, so there is nothing
for the policy to decide, and an exemption that exists because the subject is absent is
honest in a way that an exemption granted as a favour is not.

**The platform adapter is `ncui-gpui`, a layer-3 sibling to `ncui-webview-wry`**
(ADR-011). Its threading contract is dictated by the dependency, not chosen: gpui's
Windows platform captures its constructing thread as its main thread and pumps a
per-thread `GetMessageW` loop on it. The crate therefore owns one dedicated thread,
named `ncui-gpui-loop`, on which the `gpui::Application` is both constructed and run, and
every operation a host performs — opening a window, resizing it, reading its geometry
back — is a command sent into that thread with a reply channel. That is the same shape
the wry host uses for its own engine, and the two engines coexist because per-thread
Win32 message pumps are legal Win32: wry's tao loop already runs on a worker thread under
`with_any_thread`, so one process holds two independent UI event loops, each confined to
its own thread, neither ever touching the other's objects.

**A native window is always a top-level window, never composited inside a webview
window.** This is a deliberate simplification, not a missing feature. gpui 0.2.2 renders
only into its own platform window and swapchain — DirectX on Windows — and exposes no
offscreen-to-texture target; NC-UI's GPU islands are wgpu surfaces, and the two render
stacks share nothing that could carry a frame from one into the other. Compositing would
mean forking gpui *and* inventing a surface bridge, and what it would buy is precisely
what Win32 already provides for free once the native view is its own window: focus,
input, activation, z-order and multi-monitor behaviour come from the platform instead of
from a routing layer this project would have to write and then be wrong in.

**Providers are registered by the embedder, never by content.** Registration happens at
the composition root — the CLI, or the application embedding the runtime — before the
runtime serves; `ncui-kit-providers` bridges the kit's widgets and is wired in by the
CLI, not by the runtime and certainly not by a page. A page whose grant includes the
`window` capability may *open* a native window, but what it controls is exactly two
things: which registered provider it names, and the bounded JSON props it passes. The
provider's code is fixed at registration time, so opening a window injects no behaviour
the embedder did not link. That is why `Capability::Window` suffices and no new
capability exists: a capability answers "may this content ask", and the question "what
will happen when it asks" was answered at composition time, by a human, in code review.

**The event and command bridge mirrors the page bridge** (ADR-013). A view emits
`NativeViewEvent`s — a source, an action and a JSON payload in the provider's own
vocabulary, because the runtime carrying events without interpreting them is what keeps
the runtime from becoming a second copy of every provider's contract. The runtime —
or a page through the runtime — sends `NativeViewCommand`s the other way: replacement
props, or a verb the provider exposes. Both directions are bounded (names at 128 bytes,
payloads at 64 KiB, the same bound props carry, because both cross a boundary in a
process whose memory is not the sender's to spend) and both directions carry JSON,
because both cross a boundary neither side owns. Events queue at most 256 per window;
when the queue is full the oldest is dropped and the drop is counted, because a view that
outruns the runtime should lose its stalest news rather than its freshest — a slider
being dragged is the ordinary case — and the count is the only way an operator learns it
happened.

**The first platform is Windows, and only Windows.** gpui's macOS backend requires the
main thread — the same AppKit constraint ADR-018 records for tao, with the same absence
of an any-thread switch — and its Linux backend is unverified in this project. Scoping
the integration to Windows is what makes it verifiable rather than aspirational: the
engine thread contract above is legal on exactly the platform it ships on, and a
cross-platform claim made before a second platform has run is the kind of claim this
project treats as a defect.

## Alternatives considered

**Implementing `WebViewBackend` as-is for a native host.** The trait's honest defaults
exist so an engine lacking a capability can say so — `false` from `set_zoom`, `None`
from `native_view`. But a native host lacks *all* of the web-document capabilities:
there is no page to navigate, no script to evaluate, no zoom to scale, no inspector to
open. A host that implemented the trait as-is would answer every question with the
default, and a caller could not distinguish "this engine cannot" from "this window is
not that kind". The split keeps honesty cheap: one object implements both contracts, the
web half answers that there is no page, and the `native_view()` half answers the
questions a native window actually has. Rejected because pretending a native window is a
degenerate webview is how the runtime ends up negotiating navigation policy with a
checkbox.

**Rendering GPUI into a GPU island.** Impossible without forking gpui 0.2.2: the
framework owns its platform window and its DirectX swapchain and offers no offscreen
render target, so there is no frame to composite into an island even before the harder
problem — that nc-ui's islands are wgpu surfaces and gpui's output is a DirectX
swapchain, two render stacks that share no surface type — is addressed. ADR-001's own
logic applies here as it does to the web engine: this project does not fork large engines
to make them fit, because the fork is a decade of small decisions inherited all at once.
Rejected on cost, and on the ADR-001 precedent.

**A declarative view-tree protocol over the WASM component ABI.** Untrusted components
would describe widget trees, the host would render them natively, and the sandbox would
be kept. This is the only alternative that preserves the single trust class, and it is a
multi-month protocol project in its own right: view-tree diffing, an event table with
correlation, focus and selection ownership, text input and IME across the boundary, and
the accessibility tree, each of which is a contract as large as the page bridge. It is
recorded as a possible future direction rather than rejected forever — if untrusted
content ever needs native chrome, this is the shape it will take. Rejected *for now* on
cost, with the difference between "not yet" and "never" stated plainly.

**Main-thread ownership inversion, the way ADR-018 requires on macOS.** On macOS the
event loop must own the process's main thread because AppKit says so. On Windows the
constraint does not exist: per-thread message pumps are legal Win32, and tao already
exercises that legality with `with_any_thread`. Inverting the process's thread ownership
to satisfy a constraint a platform does not have would cost the CLI its ordinary
structure and buy nothing. Rejected because a workaround is only worth its shape when the
disease is present.

## Consequences

- Trusted-native content is a weaker guarantee than sandboxed WASM, and the difference
  is permanent rather than a gap to be closed. A misbehaving or compromised provider runs
  with the host's full privileges. The mitigation is structural, not cryptographic:
  providers are few, host-linked, registered at the composition root, and review-gated —
  a provider addition is a code-review event the way a dependency addition is under
  ADR-012, because that is exactly what it is.
- gpui's dependency tree is the largest ADR-012 exception yet — measured at roughly 385
  crates new to the workspace against the pre-gpui lockfile, where the largest previous
  exception added 27. It is recorded in ADR-012 as the tenth exception, feature-gated off
  by default (`gpui-engine` on `ncui-gpui`) so a machine with no display compiles the
  workspace unchanged, and confined to `ncui-gpui` and the kit bridge for one purpose.
  The full analysis, including what the default features pull in that Windows never
  executes, lives in `docs/native-dependency-analysis.md`.
- gpui 0.2.2's upstream limitations become limitations of native views. There is no
  AccessKit accessibility tree at all, which sits uncomfortably beside ADR-002's position
  that accessibility is a first-class requirement; the honest reading is that native
  views start life behind the standard the rest of the runtime is held to, and closing
  that is upstream work this project does not control. IME composition handling exists at
  the Win32 message level — composition string, candidate-window positioning — but the
  widget-level input-method contract is unverified in this project, so IME is a
  verification item rather than a shipped guarantee. There is no WASM backend, which is
  the premise of the whole decision rather than a consequence of it.
- Two UI event loops in one process is legal Win32 but a new failure mode: a deadlocked
  engine thread degrades one content kind while the other keeps answering, so a hang is
  now partial where it used to be total. The threading contract — commands with reply
  channels, no object ever touched off its owning thread — is what keeps the failure
  modes inspectable, and it is documented in `ncui-gpui`'s crate root because that is
  where the next implementer will look.
- Compile times grow for gpui-enabled builds. The default-feature tree compiles a Vulkan
  FFI stack on Windows that no Windows code path ever executes — recorded in the
  dependency analysis with the trimming recommendation — plus a full HTTP/TLS client and
  an AV1 encoder, none of which a settings panel asked for. This is the price of a
  framework that ships its batteries, paid only when the feature is on.
- A page can open windows whose content it did not write. The blast radius is bounded by
  construction — which provider, bounded props, nothing else — but the user-visible
  surface is real, and the audit trail treats a native-window open with the same
  seriousness as any other `window`-capable action.

## Implementation status

**Implemented on Windows.** The layer-2 contract is implemented and tested without a
display: `WindowContent` and its validation in `ncui-webview`, the event/command/error
types with their bounds, `NativeViewBackend`, and `RoutingFactory` routing by content
kind. The navigation-policy exemption for native windows is in place. In `ncui-gpui`, the
engine thread owns `gpui::Application` behind a command channel, a hidden anchor window
keeps the loop alive across the last user window closing, and the window host implements
the backend over real windows — proven on Windows by display-gated tests in
`crates/ncui-gpui/tests/native_window.rs` (engine startup, window lifecycle, command and
event round-trips, unknown-provider refusal, two-window independence) and by the wry/gpui
event-loop coexistence suite in `crates/ncui-runtime/tests/native_coexistence.rs`. The
runtime speaks the protocol end to end — `ncui.window.create` with native content,
`ncui.window.native.events` draining with drop deltas, `ncui.window.native.command`
delivering props updates and verb invocations — covered by the pipeline tests in
`crates/ncui-runtime/tests/native_windows.rs`. `ncui-kit-providers` registers six real
nc-ui-kit widgets with strictly validated props, tested headlessly through GPUI's own test
platform in `crates/ncui-kit-providers/tests/providers.rs`. The layering is enforced —
`ncui-gpui` sits at layer 3 beside `ncui-webview-wry`, and `ncui-kit-providers` at layer
4 — by the same `ncui-arch` check that has always enforced it. What remains open: the
manifest schema does not yet declare a native root window (a native window is opened
through the window surface, not named as an application's entry), GPUI 0.2.2's asset
source is fixed at application construction so provider SVG assets are not yet installed,
and neither Linux nor macOS has been verified.
