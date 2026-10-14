# Goal prompt: making NC-UI able to host a real desktop application

**Audience:** the agent or engineer developing NC-UI.
**Written by:** an integrator who tried to ship a desktop app on NC-UI and hit a wall.
**Date:** 2026-10-06. Machine: Windows x86_64, `rustc 1.98.1`, matching `rust-toolchain.toml`.

---

## 1. What we set out to do

We took NC-UI at face value as described in `README.md`: *"an open-source, cross-platform
desktop application runtime. Applications are written in HTML, CSS, and JavaScript or
TypeScript; the runtime supplies native execution, GPU accelerated rendering, high-performance
IPC, a scheduler, a capability-based security model, and a machine-readable interface that lets
an agent inspect, test, profile, and operate the whole system."*

Our plan was straightforward and needs no apology: **write one real desktop app on NC-UI, and use
that app as the instrument.** Not a synthetic benchmark — an app with real components, real
state, real subscriptions, real IPC traffic, real user input, real rendering. Then observe where
it works, where it is merely *policy*, and where it is absent. A feature that survives a real app
is ready; a feature that only survives its own unit test is not.

**We could not start. Not because of a bug, but because the layer that makes NC-UI a desktop
runtime does not exist.** Everything below is what we verified before stopping.

---

## 2. What we did

We did not patch your code. We audited it and ran it.

1. Read `README.md`, `docs/known-gaps.md`, `docs/adr/`, and the full source of every
   `ncui-runtime` module (`rpc`, `dispatch`, `agent`, `transport`, `health`, `describe`, `mcp`).
2. Built the entire workspace: `cargo build --workspace --all-targets` → **exit 0**, clean.
   Nothing is broken. This is a finished, well-gated codebase that stops short of its own
   destination.
3. Grepped every `Cargo.toml` for any windowing or rendering dependency.
4. Enumerated the agent API's real method table from `dispatch::method_table()`.
5. Cross-checked `protocols/jsonrpc/README.md` against the implementation.

The build passing matters for the conclusion below: **the wall is a missing layer, not a broken
one.** Everything NC-UI has built, it has built well.

---

## 3. What we found — the central finding

### 3.1 There is no window, no renderer, and no JavaScript engine

```
$ grep -rniE 'winit|wry|tauri|webview2|wgpu|glutin|egui|iced|gtk|tao|servo' Cargo.toml crates/*/Cargo.toml
(no matches beyond the crate *name* ncui-webview)
```

`crates/ncui-webview/Cargo.toml` declares exactly four dependencies — `ncui-core`, `ncui-ipc`,
`ncui-security`, `serde`, `serde_json`. Its description reads *"Window and WebView host policy:
lifecycle, capability isolation, and the IPC seam."* That description is **accurate and carefully
worded**: it is *policy*. It contains grant checks, navigation decisions, and seam message types.
It contains no window.

A crate named after the runtime's central noun, whose entire job is the runtime's central noun,
does not create one. `ncui-webview` is unreachable from `ncui-runtime`'s dependency set, so the
agent API cannot observe it either.

So the honest accounting of the title *"The web ecosystem. Native performance. One UI."*:

| Layer | State |
| --- | --- |
| The runtime that would *host* an app | **absent** |
| The policies that would *govern* a host | present, tested, not connected to anything |
| The introspection the host would expose | present, but process-level only |

`docs/known-gaps.md` says: *"a window can be created and shown in this session."* That was a
native Win32 demonstration that the *machine* has a window station. It is not a capability in
this repository, and nothing in the tree can reproduce it. We are not disputing the measurement —
we are pointing out that it does not travel with the code.

### 3.2 The agent API is process-level; there is no UI surface to drive

`dispatch::method_table()` (`crates/ncui-runtime/src/dispatch.rs:37`) defines **exactly eight**
methods, verified at lines 48, 54, 60, 66, 72, 78, 84, 90:

```
ncui.describe          ncui.health            ncui.feature.status
ncui.workers.inspect   ncui.scheduler.inspect
ncui.logs.query        ncui.logs.purge        ncui.metrics.query
```

Seven are reads. The eighth empties the log ring. **There is no method that loads HTML, mounts a
component, creates or lists a window, inspects a component tree, renders a frame, or injects
input.** An agent can ask how many threads are idle and what the p99 was, and nothing else.

Three capabilities confirm the intent and its absence:

- `Capability::Window` — *"Create or destroy windows"* — is granted to `administrator` and is
  **required by no method**.
- `Capability::InspectRuntime` is documented as *"Inspect the runtime's component and window
  tree"* but gates only `workers.inspect` and `scheduler.inspect`, neither of which returns a
  tree.
- `Capability::InspectGpu` — *"Inspect GPU surfaces and pipeline state"* — **gates no method at
  all**.

`Limits::max_windows` (default 64) is validated and surfaced in the deep health check. Nothing
counts windows.

### 3.3 The registry contradicts the spec about what is finished

`protocols/feature-registry.json` records **`webview.host` as `complete`**.
`protocols/jsonrpc/README.md:126-139` blocks `ncui.window.list` and `ncui.components.list`
explicitly on **`webview.host`**. Both cannot be true.

We understand the defence — `docs/known-gaps.md` argues at length that a self-certified registry
is weak and that the fix is to *generate* evidence. We agree with that argument, and it cuts the
other way here: the validator proved a file exists and cited a policy crate. **The one claim we
would want independently generated is precisely the one that turned out to be the load-bearing
overclaim.** That is an argument for finishing the generator, not for defending this entry.

### 3.4 Spec-versus-code drift — four confirmed defects

These are cheap to fix and they are the kind of thing that costs a third-party integrator a day.
Each is verified in both directions.

| # | `protocols/jsonrpc/README.md` says | The code does | Evidence |
| --- | --- | --- | --- |
| 1 | `-32601 \| protocol \| Unknown method` (line 85) | No `-32601` exists. Unknown method → `not_found` / `-32005`. | `rpc.rs:162-171` |
| 2 | `-32007 \| unauthorized \| Missing or invalid token` (line 94) | No `-32007` exists. A bad token closes the connection with **no frame at all** — not an error response. | `transport.rs:453-462` |
| 3 | Destructive actions need **`authorized: true`** (line 157) | The dispatcher reads **`consent`**. `authorized` is silently ignored. | `dispatch.rs:168-178` |
| 4 | Collections take an opaque `cursor` (line 144); long methods take `deadline_ms` (line 149) | **Neither is read anywhere.** There is no pagination and no cancellation. | grep across `ncui-runtime` |

Two more that are not spec drift but are overpromises in the code's own output:

- `ncui.logs.query`'s summary string promises *"Bounded log ring filtered by level"*. **No level
  filtering is implemented** — only `limit` is read. The registry has already been caught
  inventing citations; this is the same failure one layer down, in a string a machine reads.
- `ncui.health`'s documented example shows `"status": "ok"`. The enum serialises
  `healthy | degraded | unhealthy`. A client written from the doc's example breaks on the first
  response.

We deliberately did **not** test against the spec doc as written. Every one of these would have
produced a false failure report and sent the integrator chasing bugs that are in the prose.

### 3.5 What is genuinely excellent, and should not be lost

`ncui-ipc`, `ncui-security`, `ncui-state`, `ncui-scheduler`, `ncui-observability` and the framing
layer are real, well-tested, and unusually well-instrumented. The loopback transport is
genuinely careful: length validated before allocation, constant-time token compare, shared rate
limiter, audit event on every request including every refusal, connection bound. `docs/known-gaps.md`
is a better honesty document than most projects manage, and the corrections it documents
(13 false citations, the median-vs-minimum benchmark statistic) are the behaviour of an honest
maintainer.

**We are not asking for a rewrite. We are asking for the missing floor to be poured.**

---

## 4. What we want

### 4.1 The one thing that unblocks everything: a real window host

We need **one** working window on **one** platform — Windows is fine, Linux is fine, whichever you
can prove — with:

- a window that opens and is visible;
- an HTML/CSS document loaded and rendered;
- JavaScript executing, with a bridge in **both** directions (host → page, page → host);
- input delivered (at minimum: mouse click, keyboard, focus);
- `ncui-webview`'s existing policy wired in as the thing that *governs* it — a navigation
  attempt must actually be refused by `NavigationRequest`, and a capability request must actually
  be refused by `CapabilityGrant`, not merely be *modelled* by them.

That last point is the real acceptance test. **The policy crates are already tested against
themselves. They have never been tested against a webview that exists.** Wiring them to a host
would immediately make ~1,671 lines of previously-unreachable code live, and that is where we
expect the next round of real bugs to be found.

We recognise ADR-012 deliberately keeps external dependencies minimal, and adding `wry`/`winit`
cuts against it. **We think the trade has now tipped**, and here is the argument rather than the
assertion: ADR-012 exists to keep the project honest and portable, and it has succeeded — the
dependency list is tiny and every gate is real. But a runtime whose central dependency is
*absent* cannot be assessed by its own test suite, and the cost of that is now larger than the
cost of the dependency. If you disagree with that conclusion on the merits, argue it back to us
with specifics rather than citing the ADR.

**Expect this to violate your own gates.** `unsafe_code = "forbid"`, `print_stdout` denied,
zero-`unwrap`, pedantic clippy, and `ncui-arch` will all have opinions about a FFI-bearing
windowing crate. `ncui-arch`'s strict layer order will need a new layer, and it should.

### 4.2 Make the registry mean something at the top of the stack

`webview.host` should not read `complete` until a window exists and an app has run in it. We
recognise the registry's stated weakness — self-certification — and we are not asking you to fix
that whole class first. **Fix this one entry honestly**, and let the evidence generator follow.

### 4.3 Close the four drift defects, and add a gate that stops them returning

Spec and code must agree, and *something must prevent them drifting apart again*. A generated
or assertion-backed check over `method_table()` — method names, params, required capabilities,
destructive flags, and the full JSON-RPC error-code table — would have caught all four of §3.4
automatically. The project already has the pattern in `ncui-arch`, `ncui-registry` and
`ncui-placeholders`. **This gap is what those three tools exist to prevent, and nothing covers it.**

### 4.4 Then: the surface an app and an agent both need

Once a host exists, the introspection surface in `protocols/jsonrpc/README.md:126-139` becomes
the next thing. We need, at minimum:

`ncui.window.list` · `ncui.window.create` · `ncui.window.close` ·
`ncui.components.list` · `ncui.component.inspect` · `ncui.state.inspect` ·
`ncui.trace.start` / `.stop` · `ncui.ipc.inspect`

Plus the two params you already documented and never read: **`cursor`** (so a log query over a
long-running app can paginate) and **`deadline_ms`** (so an agent can bound a call and get a
cancellable `-32003`). Both are prerequisites for the "inspect, test, profile, and operate the
whole system" claim in your own README.

### 4.5 We will do the rest

Once 4.1 and 4.4 land, we will write the app — a real one, with real components and real state —
and come back with what broke. **We are offering to be your hostile test case, not to review your
documentation.** The bugs worth finding are the ones that only appear when something is actually
loaded, rendered, clicked, and torn down, and no amount of reading the source substitutes for that.

---

## 5. Summary for the record

| | |
| --- | --- |
| Build status | **Green.** `cargo build --workspace --all-targets` exits 0. |
| Hosted a desktop app | **Impossible.** No window, renderer, or JS engine exists. |
| Agent-drivable UI surface | **None.** 8 methods, 7 reads, 0 UI-related. |
| Confirmed spec-vs-code defects | **4** (§3.4), plus 2 overpromising summaries. |
| Registry overclaim | `webview.host: complete` — nothing hosts it. |
| Blocked on | One window host, and honesty about `webview.host`. |
| What we judged well | The IPC, security, state and honesty layers. All of them. |