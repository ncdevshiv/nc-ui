# NC-UI

The web ecosystem. Native performance. One UI.

NC-UI is an open-source, cross-platform desktop application runtime. Applications are written
in HTML, CSS, and JavaScript or TypeScript; the runtime supplies native execution, GPU
accelerated rendering, high-performance IPC, a scheduler, a capability-based security model,
and a machine-readable interface that lets an agent inspect, test, profile, and operate the
whole system.

The design rationale for every major subsystem is recorded in [`docs/adr/`](docs/adr/).

> [!IMPORTANT]
> **Nearly all of the code in this repository was written by AI, and nearly all of its
> testing was designed and executed by AI.** Humans set the direction, reviewed the output,
> and made the decisions that mattered; humans did not hand-write the bulk of the
> implementation. "Passes the gates" is not the same as "is correct", "is secure", or "is
> production-ready", and this project claims none of those. Read the full statement in
> [`DISCLAIMER.md`](DISCLAIMER.md) before you depend on anything here.
>
> **We are open to suggestions, enhancements, feature additions, and improvements.** If
> something is missing, wrong, or overstated, that is exactly the feedback worth having —
> see [Contributing](#contributing).

---

## What state this project is actually in

This section is the most important one on the page. The project treats an unverified claim as
worse than an absent feature, so the state below is a reading of the machine-readable feature
registry (`protocols/feature-registry.json`) rather than of intent. That registry is the
authoritative per-feature record — each entry names the files and tests behind it, and
`ncui-registry` refuses an entry that cites a file which does not exist — so where this table and
the registry disagree, the registry is right and this table is the bug.

| Area | State |
| --- | --- |
| Workspace, strict lint policy, CI gates | Implemented |
| Core vocabulary: errors, ids, capabilities, versioning, limits | Implemented |
| Binary IPC protocol, correlation, cancellation, backpressure | Implemented |
| Incremental state, patches, filtered subscriptions | Implemented |
| Priority scheduler with ageing and cooperative budget | Implemented |
| Security: capabilities, auth, rate limiting, path containment, audit | Implemented |
| Observability: tracing, metrics with percentiles, log ring | Implemented |
| Native worker pool | Implemented |
| Runtime composition, JSON-RPC agent API, health surface | Implemented |
| CLI truth layer (`ncui status`, `ncui health`, `ncui describe`) | Implemented |
| Benchmark harness with p50/p90/p95/p99 and a regression gate | Implemented |
| Virtualization: constant-cost windowing over large collections | Implemented |
| MCP adapter over the JSON-RPC surface | Implemented |
| Property tests and fuzz targets for every untrusted parser | Implemented |
| Soak tests asserting resource counts return to baseline | Implemented |
| WebView host policy: lifecycle, navigation, capability isolation, IPC seam | Implemented |
| WebView engine: a real window and a real page, governed by that policy | Implemented |
| Page bridge: correlated request/response, a page calls native commands | Implemented |
| Application commands: an application registers its own native commands | Implemented |
| Page-side client package (`@ncui/client`) | Implemented |
| Application layer: manifest, loopback asset server, `ncui run` | Implemented |
| Native capabilities: filesystem, program execution, clipboard — capability-gated | Implemented |
| Development loop: a file change reloads the page | Implemented |
| Page windows: a page opens a window or closes its own, when granted `window` | Implemented |
| GPU renderer: device, surface, render pass, text layout, device-loss recovery | Implemented |
| GPU islands composited with the DOM: a page fetches a rendered region and draws it into a canvas | Implemented — the pixels cross the CPU, one readback per frame |
| Island contents for assistive technology: the runtime serves a description the page mirrors into the DOM | Implemented |
| Island rows with text: a row's line is shaped, rasterised and composited over the row the GPU drew | Implemented — the glyphs are rasterised on the CPU, measured at about 8.5 µs a glyph |
| Language-neutral component ABI: wire format, negotiation, capability declarations | Implemented |
| Component host: `component.*` commands, `run.components`, services routed to application commands | Implemented |
| Notifications: `notifications.show`, payload delivered as data on every platform | Implemented |
| Building: `ncui build <dir>` writes a portable bundle with a digest manifest; `--check` catches a stale one | Implemented |
| An installer: `ncui build <dir> --installer` writes one file a user can run — the runtime with the bundle appended | Implemented |
| One instance per application: a second launch hands over and the running one reloads | Implemented |
| Deep links: a scheme in the manifest, registered by the bundle, delivered to the running instance | Implemented |
| Installing: `ncui install` / `uninstall` / `installed` / `launch`, with the identity and scheme registered | Implemented |
| Updating: `ncui update <dir>` swaps a new version in with two renames, leaving the old one on failure | Implemented |
| Updating from a release: `ncui update --from <url>` fetches a release document, checks the bundle's digest and applies it | Implemented — nothing checks automatically |
| Opening a target: `shell.open` hands a file or link to the platform's own handler | Implemented |
| Remembering a window: the geometry the user left is where the next run opens | Implemented |
| Remembering named windows: a page names a window it opens and its place outlives the run | Implemented |
| Window options: a page or a manifest floats, frames, fixes, floors, places or fills a window | Implemented |
| Window presentation: a page makes its own window fullscreen, maximised, minimised, hidden or focused, reads back the frame the platform draws, and is told when it is resized or moved | Implemented |
| Window shape: a page renames, moves, resizes, floats or fixes its own window | Implemented |
| The platform's theme: a page asks whether the system is light or dark, and is told when it changes | Implemented |
| Page preferences: `state.save` / `state.load` / `state.remove`, bounded and per application | Implemented |
| Accessibility: semantic tree, focus order, keyboard routing | Implemented |
| Global shortcuts: the manifest declares them, the runtime holds the key, the page hears the press | Implemented |
| Tray icon: the manifest declares an image and tooltip; a click brings the window forward | Implemented |
| SBOM, reproducible release builds, artifact signing | Implemented |
| Cross-platform verification on Linux | Implemented |
| WASM execution: untrusted components run under fuel, a memory cap and a copy budget | Implemented |
| Text inside a GPU island: shaping, wrapping, glyph rasterisation and drawing | Implemented — a row's text is shaped with the machine's own font, broken into lines that fit the row, and drawn |
| Cross-platform verification on macOS | **Not implemented** — no macOS host reachable |

**Linux and Windows both pass the full gate** — the full workspace suite, formatting, clippy
pedantic with `-D warnings`, rustdoc with `-D warnings`, the tools workspace, and the policy
gates (`ncui-arch`, `ncui-registry`, `ncui-placeholders`, and the count check described below).
`cargo test --workspace --all-targets` is the suite behind that claim;
`tools/linux_gate.py` runs the Linux side: every gate it lists, last run clean under WSL2
against this tree — twenty-three gates, 1373 workspace tests and 52 in the tools workspace, no
failures.
**macOS has not been run**: no host is reachable
from the machine that produced this tree, and the registry records it as `unverified` rather
than inferred from a CI matrix. That is
recorded as outstanding work rather than hidden: a feature is not treated as verified on a
platform until it has been verified on it.

The platform adapters — the `wry`, `tao`, `wgpu` and `rustybuzz` bindings — are behind
feature flags so the workspace builds on a machine with no display and no graphics stack. That
has a cost, and a job in CI now pays it: `adapters` builds and tests the workspace with
`--all-features` on all three platforms, so the code that actually reaches a user's machine
is compiled, linted and tested rather than merely present.

Run the validator for the authoritative, machine-readable version:

```sh
cargo run --release --bin ncui-registry -- protocols/feature-registry.json
```

A capability is recorded as either `complete` — implemented, tested, integrated, observable and
documented — or `not_implemented`. There is deliberately no third status, and the CI gate
rejects any feature declared complete that is missing required evidence.

---

## Repository layout

```text
crates/
  ncui-core            layer 0  errors, typed ids, capabilities, versions, limits
  ncui-observability   layer 1  tracing, metrics with percentiles, bounded log ring
  ncui-security        layer 1  authorization, tokens, rate limits, path containment, audit
  ncui-ipc             layer 1  binary framing, envelopes, correlation, backpressure
  ncui-state           layer 1  revisioned stores, coalescing patches, subscriptions, windowing
  ncui-scheduler       layer 1  priority, ageing, cooperative budget, cancellation
  ncui-a11y            layer 1  semantic tree, focus order, keyboard routing
  ncui-abi             layer 1  component ABI manifest and capability negotiation
  ncui-workers         layer 2  native worker pool with bounded queues
  ncui-webview         layer 2  webview host policy, lifecycle, navigation, capability
  ncui-gpu             layer 2  compositing boundary: geometry and the ADR-002 input contract
  ncui-app             layer 2  the application layer: manifest, loopback asset server
  ncui-native          layer 2  native capabilities: filesystem access, contained
  ncui-component       layer 2  the component host: bounded WASM execution behind the ABI
  ncui-webview-wry     layer 3  the `wry`/`tao` binding: a real window, a real page
  ncui-gpu-renderer     layer 3  the `wgpu` binding: device, surface, render pass, text
  ncui-runtime         layer 4  composition, JSON-RPC agent API, health
  ncui-cli             layer 5  machine-readable status, introspection, and `ncui run`
  ncui-bench           layer 5  latency and throughput harness with committed baselines
packages/
  ncui-client          the page side: the injected bridge and the typed client API
examples/
  counter              an application: a manifest and a page that calls the runtime
tools/                 a separate workspace: placeholder scanner, architecture checker,
                       feature registry validator, coverage battery
docs/adr/              architecture decision records
protocols/             machine-readable feature registry
```

Crate layering is a strict total order: a crate may depend only on crates at *strictly lower*
layers (ADR-011). The rule is enforced mechanically, and a crate with no declared layer is an
error rather than a warning. Layer 3 holds the **platform adapters** — the crates that
implement a layer-2 port against a real platform library — which is what lets the runtime
depend on a real window and a real GPU without depending on the policy that describes one.

---

## Calling native code from a page

A page calls the runtime through the bridge the host injects. Every call carries an id, so
several calls can be in flight and each answer reaches the call that asked for it.

```js
import { invoke, describe, NcuiError } from "@ncui/client";

try {
  const metrics = await invoke("describe_metrics");
  console.log(metrics);
} catch (error) {
  if (error instanceof NcuiError && error.code === "capability_denied") {
    console.error("this page may not read the metrics");
  }
}
```

The host decides what an action means. A page names an action, never a runtime method, and
the seam checks that action against the window's grant before anything runs.

An application adds its own commands to the same table, with the capabilities each one
requires:

```rust
runtime.grant_page_capabilities(&[Capability::FilesystemRead]);
let _command = runtime.register_page_command(
    "read_note",
    CapabilitySet::empty().with(Capability::FilesystemRead),
    "Read a note from the workspace",
    |payload| Ok(serde_json::json!({ "text": "..." })),
)?;
```

A command whose capabilities the page grant does not cover is refused at registration, so a
page is never handed authority the host did not grant it. A command that requires a
destructive capability also requires the page to state its consent in the call's payload.

The two halves of the contract are one file: `packages/ncui-client/src/bridge.js` is the
script the host injects, and it is the same file the package's own suite exercises. The
Windows test in `crates/ncui-webview-wry/tests/page_bridge.rs` drives it end to end in a
real window: a real page makes two calls, the host answers them in the opposite order, and
each call still receives its own answer.

## Reaching the machine

A page reaches the filesystem only when the application's manifest grants it, and only
inside the application's own directory:

```json
{ "capabilities": ["filesystem.read"] }
```

```js
const note = await window.ncui.invoke("fs.read_text", { path: "notes/today.txt" });
console.log(note.text, note.bytes);
```

The command exists only because the grant names the capability: `filesystem.read` registers
`fs.read_text`, `fs.list` and `fs.stat`; `filesystem.write` registers `fs.write_text`, which
is destructive and therefore needs the page to state its consent. A page that was not granted
a capability is never offered the command, and the call is authorized again when it is made.

Every path is checked twice — once lexically by the same containment check the rest of the
project uses, and once against the canonical path, so a symlink inside the directory cannot
point outside it. Reads and writes are bounded, and a write never creates a directory.

With `process.spawn` granted, a page can also run a program:

```js
const result = await window.ncui.invoke("process.run", {
  program: "git",
  args: ["status", "--short"],
  consent: true, // process.spawn is destructive
});
console.log(result.status, result.stdout);
```

With `clipboard` granted, a page reads and writes the system clipboard:

```js
const pasted = await window.ncui.invoke("clipboard.read_text", {});
await window.ncui.invoke("clipboard.write_text", { text: "copied from the page" });
```

The clipboard has no `std` interface and its bindings are not safe Rust, so it is read and
written through the tool each platform already ships (`clip`/PowerShell on Windows,
`pbcopy`/`pbpaste` on macOS, `wl-copy`/`xclip` on Linux). The text travels on standard
input, never on a command line, the tool gets the same scrubbed environment a page's
program does, and the read is bounded. A missing tool is reported as `unavailable` rather
than as an empty clipboard.

There is no shell: the program and its arguments are an argument vector, never a command
line. A program path must be inside the application or a bare name the system resolves. The
child gets a scrubbed environment — variables whose names look like keys, secrets, tokens or
passwords are removed — a time limit, and bounded output. `process.spawn` is destructive, so
the call is refused unless the page states its consent.

## Page windows

A page opens and closes windows when its application grants `window`:

```json
{ "capabilities": ["window"] }
```

```js
const second = await window.ncui.invoke("create_window", {
  url: location.href,
  title: "Second",
  width: 640,
  height: 480,
  // A named window's place outlives the run: the runtime writes down where it was when it
  // closes and opens it there next time.
  remember: "second",
});

// Closes the window this page is in — never one the page names.
await window.ncui.invoke("close_window", {});

// The same window, made fullscreen. The answer is what the platform reported, so a page learns
// what the screen is doing rather than what it asked for.
const { state } = await window.ncui.invoke("window_state", { fullscreen: true });
```

Three rules make this safe to hand out. The URL is evaluated against the navigation policy
*before* the engine is asked to open it, because a window's initial load is the one
navigation the engine does not offer to the policy. The new window's grant is narrowed to
the page's own, so a page cannot hand a window an authority it does not hold. And
`close_window` takes its target from the call's origin, not from the payload, so a page
cannot close a window it does not own. The capability is not in the default grant: an
application that does not ask for it never gives a page the action.

`remember` is the fourth thing a page can say about a window, and it is the page that says it
because the page is what knows that two windows it opens are two windows. The places live in
`windows.json` beside the application's own `window.json`; the name is bounded and may not
carry a control character, so it can be neither a path nor a line break in a file a person
reads; and the remembered geometry is treated as input, so a place no window could have is
never written down.

A page also decides how *its own* window is presented, at run time: `window_state` takes one of
`fullscreen`, `maximized`, `minimized`, `visible` or `focus`, and answers with the state the
platform reports afterwards. One change per call — two would be asking for whichever the platform
applied last — and the target is the caller's window, taken from the call's origin, so a page
cannot reach a window it does not own.

`window_shape` is the other half of that: the title, the place, the size, whether the window
floats and whether the user may resize it. A field the page does not name is left alone, so a
page that renames itself does not drag the window back to where it was created, and the
window list is updated only after the platform took the change — so it describes the window
that exists rather than the one that was asked for.

## Running an application

An application is a directory with a manifest in it. `examples/counter` is a complete one:

```text
examples/counter/
  ncui.app.json      the manifest
  public/
    index.html       the entry page
    app.js           the page's own code
```

```json
{
  "name": "counter",
  "version": "1.0.0",
  "entry": "index.html",
  "assets": "public",
  "window": { "title": "NC-UI Counter", "width": 900, "height": 640 },
  "capabilities": ["read.metrics", "filesystem.read"]
}
```

Running it serves the application's own files over loopback HTTP, opens a window at the
entry page, and answers the page's calls until the window closes:

```sh
cargo run --release -p ncui-cli --features ncui-cli/wry-host -- run examples/counter
```

The page has a real origin, so it can load modules and fetch its siblings — a page served
from `file://` can do neither. The server binds `127.0.0.1` and refuses any other address,
and a request path is resolved twice: once lexically, and once against the canonical path,
so a symlink inside the served directory cannot point outside it.

A window can carry an image. The manifest names one by a path relative to the application
directory — `"window": { "icon": "icon.bmp" }` — and the runtime reads that file, decodes
it, and hands the pixels to the platform's own window-creation call. BMP is read at every
depth an icon is stored at, and an `.ico` directory is read at its largest image; a format
this runtime cannot read is refused by name rather than guessed at.

`crates/ncui-runtime/tests/application.rs` is the end-to-end proof: it launches an
application from a manifest on disk, serves it over a real socket, loads it in a real
window, and asserts that the page reached a native command and received its answer.

Pass `--watch` and the window reloads whenever a file under the assets directory changes:

```sh
cargo run --release -p ncui-cli --features ncui-cli/wry-host -- run examples/counter --watch
```

The watcher fingerprints the tree — newest modification time, entry count and total size —
so an edit, an addition and a removal are all seen, and it is bounded so a large tree
cannot cost more than the reload it triggers. `a_file_change_reloads_the_page_in_the_window`
edits a served file and asserts the page loaded a second time in a real window.

## Building and gating

Everything below is a hard gate. Warnings are defects.

```sh
make gate          # everything CI runs
```

Without `make`, the equivalent commands are:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

cd tools
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cd ..

./tools/target/release/ncui-placeholders crates protocols
(cd tools && ./target/release/ncui-placeholders ncui-tools)
./tools/target/release/ncui-arch .
./tools/target/release/ncui-registry protocols/feature-registry.json
./tools/target/release/ncui-placeholders packages
./tools/target/release/ncui-placeholders examples
node --test packages/ncui-client/tests/
cargo bench -p ncui-bench --bench latency -- --baseline benches/baseline/windows/latency.json
```

The placeholder scanner is invoked once per workspace root. Exemption is decided by a
file's first path component, so a single invocation rooted above both workspaces would treat
every test file as production source.

The workspace denies `unsafe_code` outright, and denies `unwrap`, `expect`, `panic`, `todo`,
`unimplemented`, `print_stdout`, `print_stderr`, and `dbg!` in library code. Clippy runs at
`pedantic` with warnings denied, and every `#[allow]` must carry a written `reason`.

## Known gaps

`docs/known-gaps.md` records every deliberate limitation: what the feature registry
validator cannot prove, why the benchmark threshold is above the measured noise, what the
Linux gate covers and which features stay unverified there, and the two behaviours that were
declined on purpose. It is read alongside the ADR set rather than buried.

`docs/completion-audit.md` maps each requirement the project was given to the evidence that
settles it — the gates, the end-to-end runs, the comparison numbers — and states plainly
where the measurement does not support the claim that was asked for.

## Benchmarks

Two suites answering two different questions. `crates/ncui-bench/benches/latency.rs` measures
each scenario nine times and reports p50/p90/p95/p99 for the median, writing a
machine-readable report to `benches/results/latency.json`. `throughput.rs` uses Criterion
for change detection.

The latency harness can compare a run against `benches/baseline/<os>/latency.json`. It reports by
default and only fails with `--enforce`, and the reason is measured rather than assumed.

The comparison uses the **minimum** of the nine repeats, not the median. That choice is the
whole design: machine interference can only make a run look slower, so the minimum is the
statistic that tracks the code. Across independent runs of unchanged code:

The measured spread is **generated, not transcribed**. Run:

```
cargo bench -p ncui-bench --bench latency
```

The harness prints the table on every run and records it under `noise` in
`benches/baseline/windows/latency.json`. An earlier revision of this document carried the
table by hand in three places; the copies drifted apart, and the worst figure used to
justify the tolerance turned out not to reproduce when measured.

Measured on Windows x86_64, nine repeats each. **Regenerate rather than copy:**

```
cargo bench -p ncui-bench --bench latency
```

The harness prints this table on every run and records it under `noise` in
`benches/baseline/windows/latency.json`. An earlier revision of this document carried the
table by hand; it drifted between three copies, and the figure used to justify the
tolerance turned out not to reproduce.

An earlier revision compared the median and fired on 2 of 7 runs of identical code. The
minimum is what fixed that.

What the comparison does enforce:

* it refuses to compare across platforms and exits non-zero saying why, so each platform
  records its own baseline with `--write-baseline`;
* it refuses to compare scenarios whose p99 sits at the clock's resolution;
* it always prints what moved, so a regression is visible even when it does not fail.

`docs/known-gaps.md` records the measurements, the residual risk, and exactly what to change
to enable enforcement.

## Against Electron and Tauri

`comparison/` holds a measured comparison rather than a claim. Two real applications — the
Electron quick start and a standalone Tauri app — were cloned from upstream, ported to NC-UI,
and measured on the same workload: cold start to a visible window, idle working set of the whole
process tree, IPC round trip, and distributed size. The harness builds and runs both sides and
writes `comparison/bench/results/windows.json`; the tables in `comparison/README.md` are a
reading of it.

The headline, on Windows x86_64 with five repeats:

| | Electron | NC-UI | Tauri | NC-UI |
| --- | --- | --- | --- | --- |
| Cold start to a window | 308.8 ms | **51.9 ms** | 52.4 ms | **51.8 ms** |
| Idle working set | 301.3 MiB | 349.1 MiB | 335.9 MiB | 350.9 MiB |
| Idle CPU | **0.00 %** | **0.00 %** | **0.00 %** | **0.00 %** |
| IPC round trip | **0.188 ms** | 0.375 ms | **0.281 ms** | 0.329 ms |
| Distributed size | 356.8 MiB | **11.3 MiB** | **3.2 MiB** | 11.3 MiB |

NC-UI is 32× smaller than Electron and starts 5.9× faster; it is level with Tauri on startup,
and all three measure 0.00 % CPU at idle. It is slower than both on IPC, larger than both at
idle, and larger than Tauri on disk; the comparison records why rather than hiding it. The
IPC figure is the one that moved most: a page call used to cost 16.4 ms because the page loop
slept between pumps, a blocking wait on the engine's message channel brought it under 0.4 ms,
and removing the asset server's 2 ms accept poll took it from 0.52 ms to 0.375 ms. The
distributed size is 11.3 MiB because the release build carries the GPU island renderer that
the counter example's canvas is. The renderer costs 6.4 MiB: the same build with the window host
but without it measures 4.88 MiB, and a build with no window host at all measures 1.64 MiB but
cannot open a window. `comparison/README.md` records the measurement rather than hiding it.

## Virtualization

Spec 18's claim is that "a million logical records must not require a million DOM nodes."
That is measurable, so it is measured — and it is asserted rather than described.

`ncui_state::virtualize` computes which indices a viewport shows from a sparse map of
*measured* row heights rather than from the collection, so the work is bounded by how much
the user has actually looked at. The benchmark runs both a 1,000-record and a
1,000,000-record collection and the harness asserts
`a_window_ignores_the_size_of_the_collection_it_is_a_window_into`: if the window ever
starts scaling with the collection, the build goes red.

The claim is checked rather than quoted, on purpose. The measured p99 moves between runs on
a shared machine, so a figure transcribed into a document drifts from the artifact it cites —
a defect this project made and corrected twice. Instead there are two checks:

* **Structural, deterministic, in the test suite.** `a_window_ignores_the_size_of_the_collection_it_is_a_window_into` asserts a million-record window covers as many indices as
  a thousand-record one. It cannot flake.
* **Timing, reported by the benchmark.** The harness feeds two real measurements through
  `costs_within_tolerance` and prints a `CANDIDATE REGRESSION` line when the ratio exceeds
  2x. It reports rather than fails, because a shared machine cannot fail a build on a timing
  verdict.

The wall-clock check varies collection size at a *deep* offset — at offset zero the
traversal never runs, so a check there measures construction rather than the walk.

**What this catches, and what it does not.** Two mutation shapes are caught: an O(n)
`index_at` that charges the counter, and an overscan scaled by the collection size. Four are
not, and they are listed rather than left for a reader to discover — an early-returning
O(n) walk, a walk worse than linear in the measured count, redundant work inside one pass
that charges no counter, and a fixed constant-factor slowdown. The last two are constant
factors, which no ratio can see; closing them means instrumenting at the operation's
boundary rather than thresholding inside it. `docs/known-gaps.md` records the whole set.

Independently verified: both collections store 58 measured rows, so a million records cost
the same handful of DOM nodes as a thousand. Per-run percentiles are in
`benches/baseline/windows/latency.json`.

## The agent API over a socket

`ncui serve` binds the JSON-RPC surface to an ephemeral loopback port and prints a
one-time bearer token. The framing is a 4-byte big-endian length followed by that many bytes
of JSON; the first frame carries the token.

Loopback is not trusted, so the transport adds, over the in-process path:

* loopback-only binding — never a routable interface;
* a constant-time bearer-token handshake before any request is read;
* the same rate limit, capability check and consent gate, because it dispatches through
  the same pipeline rather than beside it;
* declared frame length validated **before** any allocation, so a peer announcing 3 GiB
  costs nothing;
* an audit event for every request, including every refusal;
* a bound on concurrent connections.

`crates/ncui-runtime/tests/agent_transport.rs` drives a live server over a real socket,
covering the authenticated round trip, refusal without a token, refusal of a wrong or malformed
token, the hostile-length case, per-connection rate limiting, and capability denial across the
socket.

No figure is quoted here: a count written in prose is a claim nothing re-derives, and
`tools/correct_test_counts.py` — which does derive them — reads the registry, where the
citations are structured. The file is named instead.

Verified by hand against the released binary: an authenticated client received the full
eight-method surface, `ncui.logs.purge` was refused without consent and served with it, and
an unauthenticated peer received nothing.

Getting it to start on Windows needed one external dependency. `std` has no API for
`BCryptGenRandom`, and calling it directly requires `unsafe`, which this workspace forbids —
so `ncui serve` originally refused to start rather than fall back to a predictable token.
The fix was the `getrandom` crate, which performs that FFI so this project's own code stays
free of `unsafe`. It is the single platform primitive reached beyond `std`, it is scoped to
`ncui-security`, and the reasoning is recorded in ADR-012.

## Contributing

The project's engineering principle is that progress is measured by independently verifiable
capability, not by the appearance of progress. A feature is not complete because code exists,
because it compiles, because a demo worked once, or because an agent said it works. It is
complete when it can be implemented, executed, tested, integrated, benchmarked, traced,
inspected, debugged, secured, recovered from failure, documented, and reproduced.

Contributors should read the relevant ADRs in `docs/adr/` before changing architecture.

## Licence

**Apache License 2.0** — see [`LICENSE`](LICENSE) for the binding terms and
[`NOTICE`](NOTICE) for attribution.

This project is almost entirely AI-written and AI-tested. Read
[`DISCLAIMER.md`](DISCLAIMER.md) for what that does and does not mean, and for how
to report a problem or propose an improvement.

