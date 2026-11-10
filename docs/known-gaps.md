# Known gaps and open decisions

Everything here is a deliberate, recorded decision — not a task someone forgot. Each entry
says what is not done, why, what was measured, and what would change it.

## The benchmark regression gate reports; it does not fail

**This is the most important entry in this file.**

`crates/ncui-bench/benches/latency.rs` can compare a run against a committed baseline
(`benches/baseline/<os>/latency.json`), and it does. What it does **not** do is exit non-zero,
by default, on a regression.

### Why, with the measurements

The gate compares the **minimum of nine independent repeats** per scenario, not a single
run and not the median. That statistic was chosen because machine interference can only
ever make a run look *slower*, so the minimum is the one that tracks the code.

The choice is the reason this gate is stable at all. Measuring worst/best spread across
independent runs of **unchanged code** on the machine the baseline was recorded on:

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

The median is what a *user* experiences and is what gets reported. The minimum is what a
*gate* should compare, and the difference between the two columns is the whole reason an
earlier revision of this gate — which compared median values — fired on 2 of 7 runs of
identical code, and 4 of 7 in an independent audit.

> An earlier revision of this document quoted 7.5x and 4.3x as the minimum-of-9 spread.
> Those numbers were the **median** spread, misattributed to the minimum, which overstated
> how noisy the compared statistic is. The table above is the corrected measurement. The
> design decision is unchanged, and is now better supported than the incorrect version
> claimed: the median is far too noisy to gate on, and the minimum is not.

Even so, a `security.constant_time_eq_mismatched_length` outlier of 2.0x appeared in
sampling, so the tolerance is set from that measurement rather than at the observed worst case. A gate set
exactly at the noise ceiling has no margin on a busier day.

### What the gate does do

* **It refuses to compare across platforms and exits non-zero saying so.** A baseline
  recorded on Windows compared against a Linux run is a comparison of two different
  machines. Each platform records its own baseline with `--write-baseline` on its first run.
* **It refuses to compare scenarios whose p99 sits at the clock's resolution**
  (`REPORTED_ONLY_FLOOR_NS`). A p99 of 100 ns on a ~100 ns clock is a statement about the
  timer.
* **It always prints what moved**, so a regression is visible to a human even when it does
  not fail the build.
* **`--enforce` exists** and fails on a difference. Given the measurements above it is
  close to defensible on this machine, but it is not enabled by default, because a
  dedicated runner is quieter than a workstation and a gate tuned to one is not tuned to
  the other. The choice is a measurement, not an opinion.

### What would change this

Record baselines on dedicated runners — no competing load, fixed frequency, pinned to the
machine — and re-measure. If the minimum-of-9 spread stays under about 1.5x over a long
sampling run, add `--enforce` to the benchmark step in `.github/workflows/ci.yml`. The
number to change is `ALLOWED_REGRESSION`; the switch is that flag.

## The virtualisation work bound does not catch every shape of regression

Two of the four mutation shapes run against `ncui-state`'s window suite are **not** caught,
and the gap is deliberate rather than discovered:

* **An early-returning O(n) walk** over the collection. The timing bound is on the ratio of
  collection sizes; an O(n) walk and a correct O(m) walk both scale with *something* large,
  and at a fixed `m` the two are separated only if the walk is charged to the counter — which
  a mutation is free to skip.
* **A fixed constant-factor slowdown** — say, 500 redundant iterations per measured item.
  Every bound here is a ratio, and a ratio is blind to a constant.

What *is* caught: an O(n) walk that charges the counter honestly, and an overscan scaled by
the collection size. Both were run, not assumed.

The fix for the rest is instrumentation rather than thresholding — a cost accounted at the
operation's boundary rather than inside it. That is real work and it is not done, so the
bound is documented as partial rather than presented as airtight.

## The feature registry can be made harder to evade, not unimpeachable

The validator (`tools/ncui-tools/src/registry.rs`) checks that:

* every required evidence slot is verified, and **the required set is owned by the
  validator** (`required_for()`), not authored by the registry;
* every verified citation names a repository file that exists and resolves **inside** the
  repository;
* **a citation that names no file is rejected** — so "we wrote some tests, trust us" cannot
  satisfy `unit_tests`;
* feature ids are well-formed lowercase dotted identifiers, so no rule is dodged by
  changing one letter's case;
* named crates exist in a workspace;
* a completed feature is verified on at least one platform.

Ten documented evasions were attempted against this validator — path traversal, absolute
escapes, globs, flags, directories, multi-path burying, case-dodged ids, invented crates —
and all ten are rejected.

**What it still cannot do.** `verified: true` is, at the end of the chain, a claim made by
the party being audited. The validator proves a named file exists; it cannot prove that
file demonstrates what the citation says. A determined author could cite a file that exists
but is irrelevant.

### Most citations are self-certifying

32 of the 35 completed features cite, for at least one slot, a file inside its own crate. The
three that do not are the tooling capabilities — `process.supply_chain_releases`,
`process.verification_tooling` and `cross_platform.verification.linux` — which name `ncui-core`
in `crates` for its version and licence identities but cite their evidence in `tools/` and
`benches/`, which is where that work actually lives.

`runtime.virtualization` is the clearest case of the pattern: it cites
`crates/ncui-state/src/virtualize.rs` for all nine slots, including `security_review` and
`api_review`.

That is a real limit and it is worth stating plainly: the validator proves a file **exists**,
not that the file contains what the citation says about it. A `security_review` that cites
the file it reviewed is circular in the same way a mark scheme that marks its own homework is
circular — the check is real, but it is not independent.

Closing it means either deriving the registry from evidence rather than validating
self-asserted claims (below), or adding a second evidence class — an artefact produced by a
different party — so `security_review` and `api_review` cannot cite the thing they are
reviewing. Neither is built.

The fix that removes the whole class rather than narrowing it is to **generate the registry
from evidence** — have the gate discover each feature's tests and benchmarks and
derive `verified` as an output rather than accepting it as an input. That is the right
long-term design and it has not been built. It is recorded here so the current validator is
not mistaken for something stronger than it is.

## macOS is the only platform with no run at all

Every feature is recorded `verified` on Windows. Most are also `verified` on Linux; the rest are
`unverified` there because the run that would cover them has not happened. The gate
`tools/linux_gate.py` runs is the default configuration plus a stage that compiles the release
configuration, and a feature whose evidence includes a test that opens a real window stays
`unverified` on Linux only where a GPU is involved: the GTK and WebKit development libraries are
installed there and WSLg supplies an X server and a Wayland socket, so the host opens genuine
windows and the real-window tests run. That is new, and it took a fix: `ncui-webview-wry` creates
its event loop on a worker thread and set tao's `with_any_thread` on Windows only, so the Unix
backend panicked and `ncui run` could not open a window on Linux at all. The islands test needs a
GPU device this session does not have. macOS is `unverified` everywhere: no macOS host is reachable, so the claim is
the absence of a run rather than a run that passed.

**macOS has a blocker beyond the missing host, and it is now known from the source rather than
guessed.** tao's macOS backend panics — `On macOS, 'EventLoop' must be created on the main
thread!` — and its `PlatformSpecificEventLoopAttributes` is an empty struct, so unlike Windows
and the X11/Wayland targets there is no any-thread switch to set. This host creates the loop on
a worker thread by design, because the runtime's pump thread must not be the one blocking in
`run`. The release configuration therefore cannot open a window on macOS as it stands, and the
host's tests are compiled out there with this reason attached rather than left to panic in CI's
`macos-latest` leg. Making macOS work means giving the engine loop the main thread on that
platform — a design change to the host, not a flag, and one that cannot be verified from this
machine.

The Linux claim is a run, not an inference: `tools/linux_gate.py` executes every gate it lists
under WSL2 and its tally is the authoritative count — 1373 tests across the workspace and 52 in
the tools workspace on the revision this file records, over the gate's twenty-three stages. The distro
has no system C compiler and no root to install one, so the run links with a compiler assembled
from unpacked packages under `$HOME`; that is an adaptation of the environment, not a property of
the tree, and it is why the gate's numbers are about the code rather than about the machine. An
earlier revision of this file said nothing had been verified on either platform, which was true
when it was written and stopped being true when the first Linux run happened.

## The scheduler declines one requested behaviour

See "Rejected alternatives" in `docs/adr/ADR-005-scheduler.md`. Ageing that could promote
background work above interactive work would break the no-starvation invariant, so the
invariant was kept and the promotion ceiling lowered instead.

## `require_destructive` takes a plain `bool` consent signal

A caller that writes `true` unconditionally has opted out of the gate. The correct design is
an unforgeable consent token minted by the runtime at the point of user confirmation; that
belongs in the runtime's UI surface, which is not implemented. Until then the gate fails
closed: `AgentSession::require_consent` treats an absent consent parameter as a refusal.

## `SafePath` does not follow symlinks

Containment is lexical. A symlink planted inside a root can still point outside it.
`canonicalize` would follow symlinks, making the answer a time-of-check/time-of-use race,
and errors on paths that do not exist yet — turning "create a new file" into a denial. The
limitation is documented on `SafePath`; the mitigation is forbidding symlink creation inside a
root, or re-checking containment after canonicalising.

## The registry validator checks existence, not truth — and that was worse than documented

The entry above says the validator "cannot prove that file demonstrates what the citation
says", and frames the limit as "a determined author could cite a file that exists but is
irrelevant". The measured limit is stronger than that.

Every citation was re-checked against the file it names. The files existed in all cases. The
*claims inside them did not hold*:

- Three `unit_tests` slots quoted a crate-wide total against a single file —
  `core.error_model` claimed 35 tests in `error.rs`, which has 5;
  `observability.metrics` claimed 142 in `metrics.rs`, which has 52.
- `ipc.binary_framing.failure_tests` cited "a deterministic PRNG stream of malformed frames"
  in `frame.rs`. No such test exists. The property is real — it is now asserted by
  `decoding_is_deterministic` — but the cited evidence was fictional.
- `runtime.virtualization.documentation` cited `virtualize.rs` for "why the arithmetic is
  **O(log m)**". The file says the opposite three times, explicitly retracting an earlier
  version's claim. The registry documented the retracted statement.
- `core.resource_limits.tracing` cited `logring.rs` for a limit-saturation emission. That
  1,073-line file contains no `Limits` field and no such emission.
- Five slots cited a module that emits no metric at all as evidence for a `metrics` slot.

All thirteen are corrected. The lesson is the one the validator's own documentation already
implied but understated: **path traversal is the narrow failure, and a claim that is merely
about the wrong thing is the wide one.** Nothing about the validator needed to change to catch
the narrow case, and nothing about it catches the wide case.

The three newly registered capabilities (`runtime.mcp_adapter`, `process.fuzzing`,
`process.soak_and_leaks`) are the first entries whose `security_review` and `api_review`
slots cite a file other than the one under review.

## The libFuzzer engine runs on Linux, not on the Windows host

`cargo fuzz run` cannot execute on this Windows machine: the x64 AddressSanitizer runtime is
absent from the MSVC toolchain (only the `i386` libraries are present), and the MinGW `g++`
rejects libFuzzer's C++ shim. It runs perfectly well under WSL2, where Rust ships its own
ASan runtime, so that is where the targets were executed:

| Target | Executed units | Coverage |
| --- | --- | --- |
| `fuzz_frame_decode` | 953,542 | 220 |
| `fuzz_message_decode` | 1,402,499 | 581 |
| `fuzz_semver_parse` | 1,895,778 | 280 |
| `fuzz_path_resolve` | 779,456 | 196 |
| `fuzz_token_hex` | 9,854,158 | 42 |
| `fuzz_registry_parse` | 566,023 | 851 |

Thirteen and a half million executions, no crashes. `tools/run_fuzzers.sh` runs the set with
a bounded duration per target and prints the unit count, so the number above is regenerated
rather than transcribed.

The residual limitation is the *host*, not the fuzzing: these results are Linux results. The
Windows binary of each target is built by CI's `fuzz` job on ubuntu and has not been executed
on Windows, because this machine cannot execute it.

What *is* verified, and runs in CI: 23 property tests across three crates, and
`crates/ncui-bench/tests/fuzz_targets_compile.rs`, which compiles every target body against
the real crates with `fuzz_target!` replaced by a stand-in and runs it over generated input.
That check earned its place immediately — it found two defects in the targets themselves, one
of which would have stopped a target from fuzzing at all:

- `fuzz_message_decode` indexed `&data[1..]`, panicking on **empty input** — the first thing a
  fuzzer tries.
- `fuzz_path_resolve` asserted on `requested.contains("..")`, which fails on an ordinary file
  named `report..final`. It now tests for an actual `ParentDir` component.

What was originally true here — that the engine had never been run — was a consequence of the
host rather than of the work, and running the same targets under WSL2 resolved it.

## Four capabilities recorded as blocked were not blocked

The first assessment of what this machine could build concluded that six capabilities needed
platforms it lacked. Checking rather than inheriting that conclusion changed the picture: five
of the six were buildable, and one was blocked on something a single command would generate.

| Recorded as blocked on | What was actually there |
| --- | --- |
| "Linux and macOS runners" | WSL2 with Ubuntu 26.04; the full gate runs on it (every gate in `tools/linux_gate.py` passing) |
| "a signing key" | `gpg` is installed; a key is generatable and the pipeline signs |
| "a WASM component-model toolchain" | The wire format, negotiation and capability declarations needed none, and execution needed a pure-Rust interpreter rather than a toolchain: `wasmi` (ADR-012, fifth exception) |
| "a platform Webview library" | The WebView2 runtime is installed and a window can be created *and shown* in this session; what was missing was the host policy, which is pure logic |

The lesson generalises past this repository: **a capability recorded as blocked is a claim
about the environment, not about the code, and it goes stale.** The cost of trusting it is that
the registry says `absent` for something that is done.

## `gpu.islands` was the last capability blocked, and the composition boundary is what unblocked it

The capability needed a *composition boundary*, not a renderer and not a device. Both of those
already existed: the renderer draws a frame and reads its own pixels back, and `ncui-a11y`'s
semantic tree is wired into the compositing boundary, so all eleven of ADR-002's input items are
demonstrated by a probe against a live host.

The boundary was the renderer drawing into the webview's window, the two agreeing on the pixel
geometry, and the DOM being told which pixels the GPU owns. The sections below record how it was
built and what it still costs; the capability is recorded `complete` because the boundary exists
and is exercised, not because its parts did.

## How the GPU renderer came to be composited with the DOM

The renderer exists and is verified: `ncui-gpu-renderer` owns the device, the surface, the
render pass and the text layout path, and `crates/ncui-gpu-renderer/tests/render_offscreen.rs`
creates a real device, draws a frame and reads the pixels back.

What a frame *is* is a set of coloured rectangles the GPU draws, plus the text on those
rectangles: `RenderPlan::from_frame` turns a rectangle and a colour into two triangles, and a
row's text is shaped with the machine's own font, broken into lines that fit the row, rasterised
into coverage, and composited over the pixels the GPU produced. The geometry, the clip-space
conversion, the readback and the device-loss path are all real and tested, and the row pipeline
gives an island's rows their places. Reading "the renderer draws a frame" as "the renderer
draws a UI" would still be a claim this document exists to prevent: a row's text is drawn, but
a row is not a text box — a line taller than its row is clipped, not scrolled or elided.

The step that took longest was drawing into a *window* a page is in. A GPU island is a region of
a page, and the region has to be composited with the DOM around it: the renderer's surface
has to be the webview's window, the two have to agree on the pixel geometry, and the DOM's
copy of the page has to be told which pixels the GPU owns. That is ADR-002's "highest-risk
area in the renderer", and the paragraphs below record how each part of it was built.

Progress, as of this revision: `ncui-gpu-renderer` now owns a `WindowSurface` primitive
(`surface.rs`, `wgpu_target::WindowSurface`) that configures a real `wgpu::Surface` and
presents a `RenderPlan` into it, and `WgpuDevice` exposes the `wgpu::Instance` it was
created from so an adapter can create a compatible surface. A new feature-gated test,
`crates/ncui-gpu-renderer/tests/window_surface.rs`, creates a real `tao` window, creates a
`wgpu` surface from it, configures it, and presents a frame on a machine with a display
and a GPU; it passes on Windows.

Adapter wiring is now also built: `ncui-webview-wry` gained an optional `gpu-surface`
feature that pulls in `wgpu`. When both `wry-engine` and `gpu-surface` are enabled, the
host creates a `wgpu` instance, adapter, device, and surface from a window it has just built,
configures the surface, and presents a clear frame. This proves the renderer can draw into the
window the page lives in, on a real GPU. It runs **once per process**, for the first window:
what it establishes is a fact about the machine and the driver, and repeating it per window
would bring up a device per window on the engine's single thread — which an application that
opens several windows would pay on each one, and which made a request queued behind it wait
long enough to be answered "the engine cannot do that". A feature-gated smoke test
(`crates/ncui-webview-wry/tests/host_smoke.rs`) confirms a host still comes up.

The DOM side — telling the page which pixels the GPU owns, and letting the page's own compositor
place them — is what the next section records as built, and the way it was built is the reason the
capability is complete rather than partial.

## A GPU island is composited with the DOM, and its content is its rows

The DOM side is built. `ncui-runtime` registers a route on the page's own origin — `/__ncui/island`
— and a page fetches a frame from it exactly as it fetches its own files, wraps the bytes in an
`ImageData`, and draws them into a `canvas`. From there the browser composites the island with the
rest of the document, so z-order, clipping, scrolling and opacity are the DOM's business rather than
something this renderer has to reimplement. `crates/ncui-runtime/tests/islands.rs` fetches a frame
over real HTTP on a real GPU and asserts that the island's colour is where the island was, that
everything else is transparent, and that a row carrying text comes back with ink on it.

Two things about it are worth stating rather than leaving to be discovered.

**The pixels cross the CPU.** A frame is drawn on the GPU, read back, and handed to the page, so a
640×480 island costs about a megabyte per frame and a frame at that size is not something to do at
sixty a second. `wry`'s public API has no shared-texture or child-surface seam, so a zero-copy path
is not expressible through the port this runtime draws through. The route is a real delivery of the
composition boundary, not a stand-in for a faster one — but it is not the fast path either, and a
framework that claimed otherwise would be claiming a seam it does not have.

**An island's content is its rows, and a row is drawn where the model says.** The row pipeline
exists: `IslandHost::visible_rows` says where each of an island's rows is drawn — stacked from the
content origin, offset by the scroll, clipped to the island, and only the rows the viewport reaches
— and that is the same geometry `row_at` hit-tests, so what a renderer draws and what a pointer
lands in cannot disagree. A page asks for such a region on the island route, naming each row's
height, colour and — when it carries any — its text.

**A row's text is drawn.** The text is shaped with the machine's own font (`rustybuzz`, through
`ncui-gpu-renderer::text`), broken into the lines that fit the row, and each glyph's outline is
rasterised into per-pixel coverage (`ncui-gpu-renderer::glyphs`, which fills by the non-zero
winding rule so a letter's counter is a hole), and the coverage is composited over the row's
pixels. The rasterised glyphs are kept (`ncui-gpu-renderer::atlas`), because a page redraws the
same text and the coverage of a glyph does not depend on the frame: measured in
`crates/ncui-bench/benches/latency.rs`, a twelve-character line costs a median 183 µs with every
glyph rasterised each time and 77 µs with them cached — what remains is shaping, which is the part
that cannot be cached away because it depends on the whole line. `crates/ncui-runtime/tests/islands.rs`
fetches the same island twice — once plain and once with a letter on it — and asserts that the
letter is in the second frame and not in the first.

A row taller than one line gets more than one. `Font::wrap` breaks the run at the last space on
each line, so words stay whole; between glyphs when a word is wider than the row, so it is drawn
rather than dropped; and at a newline, so a run can ask for its own lines. The block of lines is
centred in the row when it fits and top-aligned when it does not, so a block taller than its row
loses its last lines rather than its first. The accessible description still carries the row's
whole text, which is what it did when a single line was clipped horizontally: the description
describes the row's content and the frame carries what fits.

The rasterisation is on the CPU, and that is the boundary worth stating. A glyph is a set of
contours with holes; filling one on the GPU needs either a stencil pass, which draws hard edges, or
an analytic coverage pass, which is a project of its own. The pixels cross to the CPU for the
readback regardless, so the compositing happens where the frame already is. The rectangles are drawn
by the GPU; the text is rasterised and composited on the CPU.

**An island's contents reach a screen reader.** A canvas says nothing to assistive technology, and
the page owns the DOM (ADR-001), so the page has to mirror the island's content into it — and what the
runtime owes the page is the content in a form it can use. `GET /__ncui/island?…&a11y=1&label=…`
answers with the island's accessible description: its label, and each visible row's text and place,
taken from the same model the renderer draws from, so what a reader is told cannot drift from what the
page shows. A description with no label is refused rather than answered, because a label announcing
an absence is worse than no island. `crates/ncui-a11y` holds the descriptor and the rule that makes
an island reachable, `examples/counter` mirrors the description into a visually hidden list, and
`crates/ncui-runtime/tests/islands.rs` asserts the description matches the rows the GPU drew.

So `gpu.islands` is recorded `complete`: the compositing exists, the input contract is demonstrated by
a live `IslandHost`, the region's content is type rather than a colour, and its contents are described
to assistive technology through the page that owns them.

## The window host is behind a feature, and a default build opens no window

`ncui-webview-wry` implements `HostFactory` over `wry`/`tao`, and the policy governs the
engine: a navigation is offered to the window's seam and the seam's verdict is what the
engine does.

The `wry-engine` feature is **off by default**, so a machine with no display and no
`webkit2gtk` must still build the workspace. That is deliberate. It has a cost: a default
build of `ncui` cannot open a window, and `ncui run` says so rather than serving assets
nobody will look at.

The cost is paid off rather than left. CI's `adapters` job builds, lints, tests and
documents the workspace with `--all-features` on Linux, Windows and macOS, so the binding
is compiled and its tests — which open a real window and drive a real page — run on every
platform the project claims. The release build turns the feature on explicitly, so the
shipped binary can open a window.

**What is still missing:** the shipped binary's window host is the only engine, and
nothing in the runtime chooses between engines. A second engine would need the same
seam-based policy the first one obeys, and the port is written for that.

## The unit suite used to run `cargo bench`

`every_advertised_method_has_a_handler` used to exercise every method in the table, and two
of them (`ncui.test.run`, `ncui.benchmark.run`) shell out to cargo. Running them from inside
the suite deadlocked: the outer cargo holds the target-directory lock the nested cargo waits
on, so the nested build sat until its 900-second budget expired and the test failed. The
benchmark method's handler has a limit, and the test that reached it spent most of its time
compiling and running the release benchmark suite.

It is fixed rather than worked around. The two cargo-running methods are now skipped by the
sweep and covered by `the_nested_build_methods_are_guarded`, which calls them with the guard
off and asserts the refusal — proving the handler is wired without spawning a nested build.
The deadline test calls `enforce_deadline` directly with a start instant already in the past
instead of making a handler run long enough to outrun its budget.

**What remains:** the two methods' success path (actually running a nested build) is not
exercised by the suite, because a nested cargo build cannot run inside the cargo invocation
that is itself running the suite. An embedder with `NCUI_RUN_BENCH=1` exercises it; the suite
asserts the guard and the wiring, not the nested build.

## The cross-framework comparison, and what it shows

`comparison/` measures NC-UI against Electron and Tauri on two real applications. The
headline is that NC-UI is 32× smaller than Electron and starts 5.9× faster, it is level with
Tauri on startup, and all three measure 0.00 % CPU at idle. It is slower than both on IPC
round trip (0.375 ms against Electron's 0.19 ms and Tauri's 0.28 ms), larger than both at idle
(349 MiB against Electron's 301 MiB and Tauri's 336 MiB), and larger than Tauri on disk
(11.3 MiB against 3.2 MiB, because the release build ships the GPU island renderer, which is
6.4 MiB of it: the same build with the window host but without the renderer measures 4.88 MiB,
and a build with no window host at all measures 1.64 MiB and refuses to open a window). The
harness and the committed report are the evidence:
`comparison/bench/results/windows.json`.

The IPC figure has a known cause and a measured history. A page call crosses the webview
boundary twice (the page's `postMessage`, the host's `evaluate_script`) and the host's event
loop runs on its own thread, so the response makes a thread hop Electron's direct C++ IPC does
not. Before the page loop blocked on the engine's message channel, the same measurement was
16.4 ms; the blocking wait brought it to 0.52 ms, and removing the asset server's 2 ms accept
poll took it to 0.375 ms. The in-process half of that round trip — capability check, handler,
response serialisation — is measured at a p99 of 900 ns, so the remaining gap is the boundary
itself, and the reply can only be delivered by evaluating a script in the page: `wry`'s public
API exposes no `PostWebMessageAsJson` seam, and `unsafe_code = "forbid"` rules out calling the
COM interface directly.

The idle CPU figure had a cause this document first got wrong. The page loop's 50 ms wait was
the suspect, and raising it fourfold changed nothing — 0.94 % of a core over a ten-second
sample either way, which is how the hypothesis was disproved rather than assumed away. The
consumer was the asset server's accept loop: it accepted on a non-blocking listener and slept
2 ms between attempts, about five hundred wakeups a second for an application serving
nothing. It now waits on a channel that `stop` wakes at once, with a 250 ms timeout kept as
the guarantee that the loop still ends, and the measurement reads 0.00 %.

The memory gap is the webview runtime, and the decomposition is measured: `ncui.exe` is about
26 MiB against Electron's 95 MiB main process, while the WebView2 runtime's own browser
process is about 129 MiB — a process Electron folds into its main process — and the renderer
and GPU processes are smaller than Electron's. The size gap to Tauri is the runtime's own
machinery — scheduler, metrics, audit log, worker pool, capability checks, and the GPU island
renderer — which a minimal Tauri binary does not carry. Neither is a defect this document
hides; both are recorded so a reader comparing the three frameworks starts from the
measurement rather than from a claim.

## The `windows` version pin is easy to break

`wgpu` 29 and `tao` 0.33 disagree about the `windows` crate, and `gpu-allocator` sits
between them. The resolution is a pair of `cargo update --precise` invocations recorded in
`Cargo.lock` and explained in ADR-012.

A routine `cargo update` will undo it, and the error it produces — "expected
`ID3D12Device`, found `Direct3D12::ID3D12Device`" — says nothing about versions. The two
commands are in ADR-012 for that reason.

**This is not hypothetical.** It broke once while this work was being done, and the
`windows` crate's own version split is what caused it. A CI job that runs `cargo update` and
checks the build would catch it before a contributor does; there is not one yet.

## The release binary is reproducible for one directory, not across directories

`tools/build_release.py` builds the release binary twice and compares digests, which is the
only honest way to check the claim. On Windows the two builds differ: **403,978 bytes of an
11,816,448-byte binary**, and the check reports `NOT reproducible`.

The cause is in the bytes, not in the build. A dependency's build script writes into its
`OUT_DIR`, and that path is compiled into the binary — `.rdata` holds
`<target>\release\build\glutin_wgl_sys-<hash>\…` from the graphics stack (the `glutin`/`wgpu`
dependency chain), so a build into `target/release` and a build into a temporary directory
embed different strings of different lengths, and everything the linker placed after them
moves. Two builds into two *different* temporary directories differ by only 69 bytes — the PE
timestamp and the PDB GUID, both derived from the content — which is how the path was
isolated from ordinary nondeterminism.

**What holds:** the same source built twice into the same directory produces the same bytes,
and the code is identical across directories (23,656 differing bytes inside `.text`, all of
them downstream of the shifted data). **What does not hold:** a consumer who rebuilds the
release in a different directory gets a different binary and cannot verify it against the
published digest. The claim "reproducible release builds" is therefore true only for a fixed
build directory, and this section is where that is recorded rather than discovered by a
failed release. The `/Brepro` flag in `.cargo/config.toml` fixes the timestamp and the PDB
age; nothing in this tree fixes the embedded `OUT_DIR`, because it is emitted by a
dependency's build script and `unsafe_code = "forbid"` plus the dependency policy rule out
patching it. A dependency that reads `CARGO_TARGET_DIR` at build time is the thing to look
for when this is revisited.

## The page bridge carries JSON only

**The bridge carries JSON.** A request is a JSON object and a response is a JSON object.
There is no binary path, so an application that moves a large buffer through a command
pays JSON's cost twice: once into the page and once out of the runtime. The transport
underneath the agent API already has a binary framing (`ncui-ipc`), and it is not the
transport the page uses. Wiring it to the page is a real piece of work: the engine's IPC
handler hands the host a string, and the host's answer travels back as a script.

**The page's consent is a boolean.** A command that requires a destructive capability asks
the page to state `consent: true` in its payload, and a page that says so is believed. The
same limitation is recorded against the agent API above, and it has the same cause: the
runtime has no surface through which a user can confirm anything. The fix is the same in
both places: a confirmation the runtime mints, not a value the caller supplies.

## An application is a directory, and there is no bundler

The application story exists: a manifest (`ncui.app.json`) names the entry page, the
assets, the window and the granted capabilities; a loopback asset server gives the page a
real origin; `ncui run` starts the server, opens the window and answers the page's calls
until the window closes (ADR-014). What is *not* built is the step before all of it: turning
what a developer writes into the files the server serves.

**There is no bundler.** The asset server serves the files that are on disk. An
application that imports a package from `node_modules`, or that writes TypeScript, has to
build its own output first. The server does not transform a file it is asked for.

`ncui run --watch` closes the other half of the loop — a change reloads the page — but it
reloads what is on disk, so a TypeScript file is not compiled by it. A bundler is a separate
tool with its own dependency decision.

**The asset server sends no cache validator.** A response carries its content type, its length and
nothing else, so a reload — or a `--watch` reload — re-sends every asset in full even when nothing
about it changed. A conditional `GET` would fix that: an `ETag` derived from the file's length and
modification time, and a server that reads the request's `If-None-Match` and answers `304 Not
Modified` when it still matches. The plumbing is small but real (a validator on the response, the
header in the response, the request header parsed on the way in, and the `304` path that sends no
body) and it is not built.

**There is a build command, an installer and an update path.** `ncui build <directory>` writes a
portable bundle: the runtime binary, the application, a `bundle.json` describing every file with its
size and digest, and — where the platform needs one — the script that registers the application's
identity and its URL scheme. `ncui build <directory> --check` writes nothing and compares the bundle
in `--out` with a fresh build of the tree, which is how a stale bundle is caught rather than shipped.

`ncui build <directory> --installer` also writes one file a user can run. The installer is the
runtime binary itself with the bundle appended and a footer that says where it starts, so a
double-click installs the application with the same code that runs it — no second program, no
archive dependency, and no way for the two to disagree about what a bundle is. The footer is checked
before anything is allocated, every path in the archive is refused unless it is a plain relative path
that stays inside the directory being extracted into, and the extracted copy is removed once the
install finishes.

**A bundle is checked against what it says it is.** `bundle.json` names every file with its size and
SHA-256, and an install or an update hashes what it copied against those records: a file that is not
what the bundle says, or one the bundle does not name, is refused. An install removes what it copied
rather than leaving a half-installed application; an update checks the staged copy *before* the swap,
so a damaged bundle cannot take the place of one that is intact. The one file the check cannot cover
is `bundle.json` itself, which cannot name itself — it *is* the claim, and what makes a bundle from
elsewhere trustworthy is how it arrived.

**An installed application can be replaced by a newer one.** `ncui update <directory>` copies the
new bundle *beside* the installed one, moves the installed directory aside, moves the new one into
its place, and removes the old copy only then — so an update that fails at any point before the
second rename leaves the previous version exactly as it was. The registrations follow the swap: what
the new bundle names is registered and what the old install had and the new bundle does not is
released, in that order, so the application is never left with no registration at all.

**A release can be fetched and applied.** `ncui update --from <url>` fetches a release document —
which names the application, the version, the bundle's URL and its SHA-256 — then fetches the bundle,
checks it against that digest, unpacks it, and applies it through the same path as an update from a
directory, where the bundle's own per-file records are checked again. A release for a different
application, or a download that is not what the document promised, is refused with the reason and the
installed version is left alone. What is still absent is anything *automatic*: nothing checks on a
schedule, nothing tells the user an update exists, and an application that wants one runs this itself.
The transport is the platform's own `curl`, so a machine without it is told so rather than failing
obscurely.
**An installed application is not a signed artefact.** The release pipeline signs and publishes the
`ncui` binary itself (ADR-012), not applications built with it, so a bundle carries no signature of
its own; what makes one from elsewhere trustworthy is the digest the release document names and how
it arrived. The registrations are scripts the user runs rather than steps the build performs — a
build that rewrote the user's registry would be a build whose output depended on the machine that
ran it.

**There is no window configuration beyond the manifest and the page's own requests.** The manifest
names a title, a size, a minimum size, the four window options (`always_on_top`, `decorations`,
`transparent`, `resizable`), whether the window opens fullscreen or maximised, and the image the
window carries; a page that opens a window adds a place, those options, a minimum size and a name
to remember it by; and a page that holds a window can make it fullscreen, maximised, minimised,
hidden or focused, or give it a new title, place, size, float or resizability while it runs. What is missing
is a menu and a per-application display mode.

**The application icon is read from a bitmap file.** The manifest names an icon by a path relative
to the application directory, and the runtime reads that file and decodes it into the straight RGBA
the platform's window-creation call takes. BMP is read at every depth an icon is stored at — 1, 4,
8, 24 and 32 bits per pixel — and an `.ico` directory is read at its largest image, with the AND
mask that gives an icon its transparency where the alpha channel has none. What is refused, by
name, is everything else: a PNG inside an `.ico` (it needs a decompressor this runtime does not
carry), a run-length compressed bitmap, and a bit depth no icon uses. The refusal is reported to
the person who named the file, because that is the person who can replace it.

**The application's native commands are Rust.** A page can call a command the *host*
registers, and the host is a Rust program. There is no way for an application written only
in JavaScript to register a native command, and no way for an application to own a native
module. This is the largest remaining difference from a framework whose native side is a
packaged library rather than a compiled host.

## The native surface, capability by capability

A framework is judged by what a page can do to the machine. The table below is that list, with
what each capability's state actually is. A capability that is absent is **absent**, not stubbed
— a page calling a command that does not exist gets the seam's refusal, which is the honest
answer.

| Capability | State | What it needs |
| --- | --- | --- |
| Filesystem read/write/list/stat | Implemented | — |
| Running a program (no shell, scrubbed environment) | Implemented | — |
| Clipboard read/write | Implemented, through the platform's tool | — |
| Running a WebAssembly component | Implemented, bounded by fuel, a memory cap and a copy budget | — |
| Notifications | Implemented, through the platform's tool | — (the bundle ships `register-identity.ps1`) |
| Deep links | Implemented on Windows and Linux; the link reaches the running instance, and the page reads it through `app.link` | macOS needs the bundle's `Info.plist` |
| Installing | Implemented per user: files, identity, scheme, an exact uninstall, a self-installing executable (`ncui build --installer`) and an update path (`ncui update`, from a directory or a released bundle) | No MSI, no per-machine install, and nothing automatic — an application that wants an update runs `ncui update` itself |
| Opening a file or link with the platform's handler | Implemented, through the platform's tool | A scheme registered during a session may not be live in it: Windows resolves associations through a cache the shell refreshes on its own schedule |
| Remembering a window's place and size | Implemented for the application's own window, and for a window a page opens under a name: `create_window` takes `remember: "notes"`, the place is applied when the window is created and written down when it closes | A window a page does not name is not remembered |
| A page's own preferences | Implemented: bounded key-value storage per application | No migrations — a page that changes the shape of what it stores reads its own older shapes |
| File and colour dialogs | Absent | `rfd`, which is safe to call — but a dialog is *interactive*: `pick_file` blocks until a person answers, so a test cannot complete one. This project's standard is that a capability is evidenced by running it, and a dialog whose core cannot be run by a test would be a capability recorded as verified on the strength of its argument parsing. The honest options are a human-driven verification step the registry does not model, or leaving it absent with this note |
| Tray icon | Implemented: the manifest declares an image and a tooltip, the host creates it on its engine thread, and a click brings the application's window forward and tells its page | — (the platform's own call, through `tray-icon`) |
| Native menus | Absent | Not a dependency question. `muda` is already in the tree through `tray-icon`, and on Linux (`init_for_gtk_window`) and macOS (`init_for_nsapp`) it attaches safely — but on **Windows** its entry point is `pub unsafe fn init_for_hwnd(hwnd: isize)`, and `unsafe_code = "forbid"` is a workspace-wide ban this project has refused to lift for any reason. A Windows menu bar therefore needs either a windowing crate that offers a safe seam, or a deliberate change to the safety rule |
| Developer tools (the platform's web inspector) | Absent | `wry` exposes `open_devtools`, `close_devtools` and `is_devtools_open`, and its Linux backend makes all three real. Its **Windows** backend opens the inspector with `OpenDevToolsWindow` and then reports `is_devtools_open() == false` unconditionally and makes `close_devtools()` a no-op, so on the platform this project verifies first, opening could not be evidenced by a test and closing would not work — the partial implementation the standard here refuses to ship. The three methods are also `#[cfg(any(debug_assertions, feature = "devtools"))]`, so a release build of `ncui` could not call them at all without enabling wry's `devtools` feature, which this project builds with `default-features = false`. A capability is added when a host can both do it and answer whether it was done |
| Global shortcuts | Implemented: the manifest declares them, the runtime holds them while it runs and releases them when it stops, and the page hears a `shortcut` event | — (the platform's own registration call, through `global-hotkey`) |

Each capability still absent needs a new dependency, and ADR-012's dependency policy requires an ADR
exception for each one — the same bar `wry`, `wgpu`, `rustybuzz` and `global-hotkey` had to clear.
Adding them without that bar is how a runtime ends up with an unaudited native surface, which is
the failure this project's capability model exists to prevent.

The one thing that is *not* waiting on a dependency is the shape: the seam, the grant, the
command table and the audit trail are already there, which is why each capability so far
was added as one module rather than a subsystem.

**The platform's theme is reported, not chosen.** A page asks (`window_theme`, requiring the
`window` capability) whether the platform is drawing its interface light or dark, and is told when it
changes through a `theme` event. The answer comes from the platform rather than from the runtime,
because the theme is the user's setting for the whole machine — a window that picked its own would
disagree with every other one on the screen. A backend that cannot ask reports nothing rather than
guessing, and a theme this runtime does not know is not mapped to one it does.

**A global shortcut is a key taken from every other application, so it is declared rather than
requested.** The manifest names the accelerator and the command it stands for; the runtime asks the
platform for the key at launch, holds it while the application runs and releases it when the
application stops, and the page hears a `shortcut` event carrying the command name. A page cannot
register one — a page that could would be a page that could take the key another program is using —
and the vocabulary is closed: `Ctrl+Shift+K` is accepted, `Hyper+K` is not, a bare letter is not, and
a bare function key is, because the function keys are the ones a machine has to spare.

**The tray icon is created on the engine's own thread.** A tray belongs to the platform's event
loop, so the host builds it there rather than wherever the runtime happens to be, and a click comes
back through the loop: the application's window is brought forward and its page hears a `tray` event.
The platform delivers tray events on a channel of its own and the loop waits rather than polls, so a
thread carries them across — without it a click would sit in the channel until some window event
happened to wake the loop, which for an application with one quiet window is never. The image is
decoded before it reaches the host, so a file that cannot be read is reported to the person who named
it rather than becoming a tray icon that never appears.

**The clipboard costs a process start.** Reading or writing it starts the platform's own
tool — `clip` and PowerShell on Windows, `pbcopy`/`pbpaste` on macOS, `wl-copy`/`xclip` on
Linux — so a call costs tens of milliseconds on the Unix tools and a few hundred on
PowerShell, where a direct binding would cost microseconds. It is also only as available as
the tool is: a Linux session with neither a Wayland nor an X11 clipboard tool gets
`unavailable`, which is reported rather than hidden. A direct binding (`arboard` or FFI) is
the fix, and it is a dependency decision under ADR-012, not a change of design.

**A timed-out program's tree is stopped, but a program that deliberately detaches is not.**
`process.run` stops the program it started *and everything that program started*. On Windows the
runner asks `taskkill /T /F` to walk the parent chain; on Unix the child is started in its own
process group and one signal reaches the whole group. A test drives both paths: a grandchild
appends to a file once a second, the parent's run is stopped by its time limit, and the file must
stop growing. Disabling the tree kill makes that test fail with the grandchild's extra bytes,
which is how the fix was checked rather than assumed.

What remains is the case the platform tools cannot reach: a program that deliberately breaks
away — Windows `start` in a new process group, Unix `setsid`, a daemon that re-parents itself —
is no longer a descendant by the time the limit is reached, and neither `taskkill /T` nor a
process-group signal will find it. Closing that needs a job object (Windows) or a cgroup (Linux),
which `std` does not expose and this workspace cannot reach without FFI or a new dependency. The
Unix path is compile-checked here and exercised by CI on Linux; the Windows path is the one
measured on this machine.

## macOS has no host here

`cross_platform.verification` was one entry covering two platforms. Linux ran; macOS did not.
It is now two entries, because a single entry would have let the completed half hide the
unrun one — which is what the registry's `unverified` platform states exist to prevent.
