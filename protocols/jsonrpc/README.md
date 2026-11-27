# NC-UI agent protocol (JSON-RPC 2.0)

**Status:** Specification · **Date:** 2026-10-01

This document defines the machine-readable interface an agent uses to inspect, test, profile and
operate an NC-UI runtime. It is the specification the implementation is written against; where
the two disagree, that is a defect in one of them and the specification is the tie-breaker.

## Design rules

Three constraints shape everything below.

1. **Discovery before use.** An agent must be able to learn what the runtime supports and what
   *it* is permitted to do by asking, not by reading source or guessing. `ncui.describe` is
   the entry point.
2. **MCP is an adapter, never the internal architecture.** This JSON-RPC surface is the
   runtime's own interface. MCP is layered on top of it (ADR-008). If MCP were the internal
   shape, an external agent protocol's constraints would become core design.
3. **Errors are structured and stable.** Every failure carries a machine-readable `code` from a
   closed set, plus the operation and the correlation identifier. Error codes are a public API
   surface: renaming one is a breaking change.

## Transport and authentication

- **Loopback is not trusted.** The server binds to loopback only, and still requires
  authentication. An agent on the same machine is not automatically authorised.
- Every request carries a bearer token. Tokens are compared in constant time.
- Requests are rate limited per token. Limits come from `Limits::agent_rate_per_second` and
  `Limits::agent_rate_burst`.
- Every request that performs work produces an audit event (see *Audit* below).

## Request shape

Standard JSON-RPC 2.0. NC-UI reserves the `ncui.` prefix for its own methods.

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "method": "ncui.health",
  "params": { "deep": true }
}
```

`params` is an object, or absent. Positional parameters are not accepted, because they make a
method's signature unreadable and are incompatible with adding optional fields — and a request that
sends one is refused with `-32602` rather than read as a request that named no parameters, so a
caller learns its arguments were not understood instead of receiving an answer to a different
question.

## Response shape

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "result": { "status": "healthy", "checks": [ ... ] }
}
```

`status` is one of `healthy`, `degraded` or `unhealthy`.

or

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "error": {
    "code": -32001,
    "message": "capability `read.logs` is not granted to agent",
    "data": {
      "ncui_code": "capability_denied",
      "operation": "agent::authorize",
      "correlation": "0f3a...",
      "retryable": false
    }
  }
}
```

`error.data.ncui_code` is the stable machine-readable code. `error.data.correlation` joins the
failure to its trace.

### Parameters

Every parameter the surface documents. A parameter that reaches a method's payload without being
listed here is undocumented; one that is listed here and read by nothing is a claim with nothing
behind it, which is why `protocol_conformance.rs` checks this table against the runtime's source.

| Parameter | Methods | Meaning |
| --- | --- | --- |
| `consent_token` | every destructive method | A host-issued, consume-once approval bound to the authenticated subject, method, capabilities and exact operation parameters |
| `deadline_ms` | every method | The budget the caller gives the call, in milliseconds |
| `deep` | `ncui.health` | Ask every subsystem for its detail rather than a summary |
| `limit` | every method that returns a collection | How many records to return: 100 by default, 10 000 at most |
| `cursor` | `ncui.logs.query` | An opaque value from a previous `next_cursor`; absent means the first page |
| `role` | `ncui.describe`, `ncui.serve` | The role whose grants are described or served |
| `package`, `filter` | `ncui.test.run`, `ncui.benchmark.run` | Which package to run, and which test or benchmark within it |
| `url` | `ncui.window.create`, `ncui.window.post` | The page to open, or the page to evaluate a script in |
| `content` | `ncui.window.create` | What the window carries: `{"kind": "page"}` (the default) or `{"kind": "native", "provider": "...", "props": {...}}` |
| `command` | `ncui.window.native.command` | What the native view is told: `{"kind": "update_props", "props": {...}}` or `{"kind": "invoke", "action": "...", "payload": {...}}` |
| `window` | every method that names one window | The window's identity |
| `title`, `width`, `height`, `x`, `y` | `ncui.window.create` | What the window is called, how big it is and where it sits |
| `always_on_top`, `decorations`, `transparent`, `resizable` | `ncui.window.create` | How the window is framed, floated, made see-through and resized |
| `min_width`, `min_height` | `ncui.window.create` | The floor the user may drag the window to |
| `remember` | `ncui.window.create` | The name a window's place is remembered under |
| `fullscreen`, `maximized`, `minimized`, `visible`, `focus` | `ncui.window.state` | The one change to make to a window's presentation: fill the display, maximise, minimise, show or hide, or take focus |
| `script` | `ncui.window.post` | The script to evaluate in the page |
| `landscape`, `print_background`, `scale`, `paper_width`, `paper_height` | `ncui.window.print_to_pdf` | The print options the engine accepts: orientation, page backgrounds, a scale between 0.1 and 2.0, and paper size in inches between 1 and 100 |
| `format`, `quality` | `ncui.window.screenshot` | The image format (`png` or `jpeg`) and, for JPEG, a quality between 0 and 100 |

### Consent and host approval

Standing capability authorization and per-call destructive consent are separate gates. A destructive
method is served only after the host approves the exact operation, either by installing a real
confirmation surface or by supplying a token it minted after obtaining that approval. The legacy
`consent` boolean authorizes nothing. There is no client-callable mint method: `ncui.consent.mint`
was removed and returns `not_found`.

A `consent_token` carries the logical-operation correlation, authenticated subject, method,
capability set, SHA-256 digest of the normalized parameters and a 128-bit nonce. The runtime's
ledger, not the token JSON, proves issuance and one-time use. The digest covers every operation
parameter and excludes only the reserved `consent_token` and legacy `consent` transport fields.
Changing the subject, correlation, method, required capabilities or parameters refuses the token;
a refused attempt does not spend it, and a successful destructive call spends it once.

Agent subjects name the authenticated session; socket subjects name the individual connection
(`tcp-{connection_id}`), and page subjects name the window (`page:{target}`). Without a supplied
token, the runtime rate-limits and authorizes the caller before prompting. Prompt text contains the
subject, method, required capabilities and complete normalized parameters; oversized operation
details are refused rather than truncated. A malformed, mismatched, replayed or wrong-subject token
never falls back to a prompt. The token's correlation is used for the request's audit event.

### Error codes

| JSON-RPC code | `ncui_code` | Meaning |
| --- | --- | --- |
| -32600 | `validation` | Malformed request or invalid parameter |
| -32602 | `validation` | Invalid params |
| -32603 | `internal` | Runtime defect — a defect, not bad input |
| -32001 | `capability_denied` | The agent lacks a required capability, or authentication failed |
| -32002 | `limit_exceeded` | Rate or resource limit exceeded |
| -32003 | `timeout` | The operation exceeded its deadline |
| -32004 | `cancelled` | The operation was cancelled |
| -32005 | `not_found` | The referenced entity or method does not exist |
| -32006 | `conflict` | Conflicts with current runtime state |
| -32008 | `backpressure` | A bounded queue is saturated |

Unknown methods return `-32005` / `not_found`. Authentication failures return
`-32001` / `capability_denied` — the connection is closed without a response frame,
because a peer that cannot authenticate must not receive any information about the
surface.

## Methods

Every method declares the capability it requires. A method whose required capability is not
held returns `-32001` and performs no work. A method marked **destructive** additionally
requires exact host approval, represented by a valid `consent_token` or an installed native
confirmation surface; the default is refusal.

### Implemented

These are dispatched today. Each is covered by
`crates/ncui-runtime/tests/agent_surface.rs`, which asserts that the advertised requirement
is the one actually enforced.

| Method | Required capability | Destructive | Description |
| --- | --- | --- | --- |
| `ncui.describe` | none | no | Runtime identity, methods, roles, capabilities and the caller's own grants |
| `ncui.health` | none | no | Liveness, readiness and per-subsystem detail (`deep`) |
| `ncui.feature.status` | none | no | The feature registry, when the host loaded one |
| `ncui.workers.inspect` | `inspect.runtime` | no | Worker lifecycle, queue depth and task counters |
| `ncui.scheduler.inspect` | `inspect.runtime` | no | Ready-queue depth against capacity |
| `ncui.logs.query` | `read.logs` | no | Bounded log ring, filtered by level and correlation |
| `ncui.logs.purge` | `read.logs` | **yes** | Discards the log ring; requires exact host approval |
| `ncui.metrics.query` | `read.metrics` | no | Metric snapshot with percentiles |
| `ncui.window.list` | `inspect.runtime` | no | Every open window, with its lifecycle state and page grant |
| `ncui.window.create` | `window` | no | Open a window on a URL; requires a host to be installed |
| `ncui.window.post` | `window` | no | Evaluate a script in a window's page; the host-to-page half of the bridge |
| `ncui.window.close` | `window` | no | Close a window by identity |
| `ncui.window.state` | `window` | no | How a window is presented, and how to change it: fullscreen, maximised, minimised, shown or hidden, or focused. The answer also reports the platform's frame — decorated, resizable, always on top — as the platform has it. A page is told when its window is resized or moved, through `resized` and `moved` events |
| `ncui.window.theme` | `window` | no | Whether the platform is drawing its interface light or dark, and a `theme` event when it changes |
| `ncui.window.native.events` | `window` | no | Poll one event a native window's view has emitted, or null when its queue is empty |
| `ncui.window.native.command` | `window` | no | Deliver one command to a native window's view: update its props, or invoke one of the verbs it answers |
| `ncui.window.zoom` | `window` | no | Scale a window's page by a factor between 0.25 and 5.0, where 1.0 is the platform default |
| `ncui.window.print.pdf` | `window` | no | Render a window's page to PDF through the engine's own DevTools protocol; the answer carries the PDF as base64 |
| `ncui.window.screenshot` | `window` | no | Capture a window's page as a PNG or JPEG through the engine's own DevTools protocol; the answer carries the image as base64 |
| `ncui.window.child.create` | `window` | no | Embed a child webview into a window's frame: a starting URL and a rectangle (logical pixels, relative to the window's client area) |
| `ncui.window.child.bounds` | `window` | no | Move a live child webview to a new rectangle |
| `ncui.window.child.close` | `window` | no | Close a live child webview |
| `ncui.window.child.list` | `window` | no | Every live child webview across open windows, with its parent and rectangle |
| `ncui.window.devtools.open` | `window` | no | Open a window's platform web inspector |
| `ncui.window.devtools.close` | `window` | no | Close a window's platform web inspector |
| `ncui.window.devtools.state` | `window` | no | Whether a window's inspector is open: `open`, `closed`, or `unknown` when the platform cannot say |
| `ncui.window.shape` | `window` | no | Change a window's title, place, size, always-on-top or resizability |
| `ncui.components.list` | `inspect.runtime` | no | Every component declared by mounted windows' manifests |
| `ncui.gpu.inspect` | `inspect.gpu` | no | GPU islands, their render modes, the input contract they demonstrate, and the device's health |
| `ncui.capabilities.describe` | none | no | Every capability, grouped by scope |
| `ncui.component.inspect` | `inspect.runtime` | no | One mounted component, by identity, with its window and grant |
| `ncui.state.inspect` | `inspect.runtime` | no | A snapshot of the runtime's observable state |
| `ncui.ipc.inspect` | `inspect.runtime` | no | The IPC protocol version and frame limits |
| `ncui.devtools.tree` | `inspect.runtime` | no | Live snapshot of the window tree: every open window, its state, and its capability grant |
| `ncui.devtools.events` | `inspect.runtime` | no | Recent runtime events (navigations, capability denials, window state changes), filterable by kind |
| `ncui.devtools.profile` | `inspect.runtime` | no | Per-method timing histograms recorded while profiling was active |
| `ncui.devtools.profile.reset` | `inspect.runtime` | no | Clear all recorded profiling data |
| `ncui.devtools.summary` | `inspect.runtime` | no | A summary of the devtools subsystem: event count, capacity, profile methods, and call count |
| `ncui.app.emit` | `window` | no | Emit an application-defined event to a window's page (deep links, notification clicks, custom events) |
| `ncui.window.drag.region` | `window` | no | Initiate a window drag from a drag region (titlebar) |
| `ncui.window.create.child` | `window` | no | Open a child window with a narrowed grant |
| `ncui.cdp.send` | `cdp` | **yes** | Send a CDP command to a window's own page target; requires exact host approval |
| `ncui.trace.start` | `inspect.runtime` | no | Begin verbose tracing of operations |
| `ncui.trace.stop` | `inspect.runtime` | no | End verbose tracing of operations |
| `ncui.profile.start` | `inspect.runtime` | no | Begin profiling: record per-method timing |
| `ncui.profile.stop` | `inspect.runtime` | no | End profiling |
| `ncui.test.run` | `run.tests` | no | Run the project's test suite |
| `ncui.benchmark.run` | `run.benchmarks` | no | Run the project's benchmark suite |

### Native windows

A window's content is either a page at a URL or a native view from a provider registered
with the host. `ncui.window.create` chooses by its `content` parameter. Absent means
`{"kind": "page"}`, so a payload written before native windows existed means exactly what it
always meant; `{"kind": "page"}` said out loud is the same request. A native window is asked
for with `{"kind": "native", "provider": "...", "props": {...}}` and obeys three rules the
request is validated against before any host is asked:

- A native window carries **no URL**. Naming `url` alongside native content is a
  `validation` error, because the request describes two different windows and neither is
  the one the caller will get.
- The provider identifier is at most 128 bytes of lowercase ASCII letters, digits, `.`,
  `_`, `-` or `:`, starting with a letter — the name of a provider the host registered,
  never a free-form label.
- The props, serialized, are at most 65 536 bytes. The bound is the contract's, and it is
  enforced where the request is parsed rather than left for the host to discover.

`ncui.window.list` reports what each window carries: `content` is the machine-readable
kind (`page` or `native`) and `provider` is present when the content is native. Every
window method that acts on a window accepts native windows; the two below exist only for
them, and asked of a page window they answer `-32006` / `conflict` with a message naming
what the window is — because "no events yet" and "no events ever, this window is a page"
are different facts.

`ncui.window.native.events` takes `{"window": "<id>"}` and polls one event:

```json
{ "window": "<id>", "event": { "source": "test.panel", "action": "clicked", "payload": { "row": 2 } } }
```

`event` is `null` when the queue is honestly empty, so an agent polls rather than
subscribes. The event object is the provider's own, serialized in the native contract's
shape — `source`, `action`, `payload` — carried unchanged: what a view emits is the
provider's business, and the runtime does not interpret it. Events are bounded per window
at 256 queued; a view that outruns the polling drops its oldest events.

`ncui.window.native.command` takes `{"window": "<id>", "command": {...}}` and answers
`{"window": "<id>", "sent": true}` when the view accepted the command. The command is the
contract's tagged shape — `{"kind": "update_props", "props": {...}}` replaces the props
the view was built with, `{"kind": "invoke", "action": "...", "payload": {...}}` invokes a
verb in the provider's own vocabulary. Actions are at most 128 bytes and payloads at most
65 536 bytes serialized; a command outside the bounds is refused with `-32602` /
`validation` **before it is sent**, so a host is never asked to carry what the contract
forbids.

A command the view itself refuses is a structured error whose message carries the stable
`native_*` code as a prefix, because the code is the part of the refusal an agent branches
on and it must survive every layer:

| The view's answer | `ncui_code` | Message prefix |
| --- | --- | --- |
| The window's view is gone | `conflict` | `native_window_gone` |
| The view refused the command | `conflict` | `native_view_refused` |
| The native engine is unavailable | `conflict` | `native_engine_unavailable` |
| The command was out of bounds | `validation` | `native_out_of_bounds` |

### Planned, not implemented

These are specified so the interface is stable, but **they do not exist**. Each is recorded
`not_implemented` in `protocols/feature-registry.json`, so an agent reading either document
agrees with the other.

| Method | Required capability | Blocked on |
| --- | --- | --- |
| (none) | - | - |

The "Blocked on" column is deliberate: `component.inspect` is not merely unwritten, it has
nothing to report until a component tree exists. Publishing it as merely "planned" would
hide that dependency.

### The page bridge

A page is not an agent, but it is a caller. The runtime exposes a narrow seam to the
document in a window: the page names an **action**, never a method, and the host maps the
action to a method and checks the page's grant.

| Action | Method | Requires |
| --- | --- | --- |
| `describe` | `ncui.describe` | none |
| `health` | `ncui.health` | none |
| `describe_metrics` | `ncui.metrics.query` | `read.metrics` |
| `query_logs` | `ncui.logs.query` | `read.logs` |
| `list_windows` | `ncui.window.list` | `inspect.runtime` |
| `list_components` | `ncui.components.list` | `inspect.runtime` |

An action the host did not declare is refused. A page's request is rate limited,
authorized and audited exactly as an agent's is, and it appears in the audit trail under
the identity `page:<window-id>`.

## Pagination and limits

Every method that returns a collection accepts `limit` (default 100, maximum 10 000).
Exceeding the maximum is a `validation` error, not a silent clamp — a silently clamped result
reads as "this is everything" and is worse than a refusal.

An opaque `cursor` paginates past the first page: pass the `next_cursor` value returned by
a previous response to fetch the next page. Absent means the first page. `ncui.logs.query`
honours it today; other collection methods will follow.

## Deadlines

Long-running methods accept an optional `deadline_ms`: a budget in milliseconds, between `1` and
`3600000`. The runtime measures the call against it and answers `-32003` when the budget was
exceeded, with `elapsed_ms` in the error and a `request.timeouts` count and a warning in the log. A
timeout must generate observable diagnostics; a timeout is never used to conceal a correctness
problem.

The pipeline is synchronous, so the handler runs to completion: what a budget buys a caller is the
truth about how long the call took, not an interruption. A caller that asked for a millisecond and
was answered with a success ten seconds later has been told something untrue by that success — and a
failure is never rewritten as a timeout, because the failure is the more useful answer.

A budget that is not a budget — zero, a year, a value that is not a whole number — is refused with
`-32602` rather than read as "no deadline" or "no limit". A call that names no `deadline_ms` is not
bounded by one.

## Audit

Every method invocation produces an audit event containing `agent_id`, `request_id`,
`correlation`, `method`, `target`, `result`, and the capabilities used. Destructive actions
require exact host approval as well as the standing capability; a supplied token's correlation
identifies the audited logical operation (ADR-007).

## Versioning

The interface is versioned independently of the runtime via `SemVer::is_compatible_with`. A
pre-release agent client will not bind to a stable runtime, and a stable client will not bind to
a pre-release runtime.
