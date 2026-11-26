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

Most completed features cite, for at least one slot, a file inside their own crate. Tooling
capabilities such as `process.supply_chain_releases`, `process.verification_tooling` and
`cross_platform.verification.linux` name `ncui-core` in `crates` for its version and licence
identities but cite their evidence in `tools/` and `benches/`, which is where that work lives.

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

All 76 implemented features are recorded `verified` on Windows; the absent macOS-verification
entry makes no platform claim. Linux is verified per feature, and the registry is authoritative
rather than a count copied into this document. The gate `tools/linux_gate.py` runs covers the
default configuration, release-configuration builds and tests, and genuine GTK/WebKit windows
through WSLg. That does not automatically satisfy every feature's benchmarks, metrics and
integration evidence, so a Linux flag is set only when the feature's whole evidence set has been
observed there. The GPU-dependent islands tests run under WSL2 with Mesa's lavapipe software
Vulkan driver: the render, readback and HTTP island route assert real pixels, though this is not
physical-GPU performance evidence. macOS is `unverified` everywhere: no macOS host is reachable, so the claim is
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
under WSL2, and the tally it prints is authoritative; the latest result is recorded in
`docs/completion-audit.md` §8. Ubuntu 26.04 provides `cc` 15.2 and the pinned Rust 1.98.1
toolchain. The gate keeps its Cargo targets under host-specific `/var/tmp` directories so Linux
ELF artifacts cannot shadow Windows PE artifacts and a full build cannot exhaust `/tmp`'s tmpfs.
An earlier revision of this file said nothing had been verified on either platform, which was true
when it was written and stopped being true when the first Linux run happened.

## The scheduler declines one requested behaviour

See "Rejected alternatives" in `docs/adr/ADR-005-scheduler.md`. Ageing that could promote
background work above interactive work would break the no-starvation invariant, so the
invariant was kept and the promotion ceiling lowered instead.

## Consent is enforced and native acceptance is verified on Windows and Linux

Both destructive-consent paths use runtime-minted [`ConsentToken`](../crates/ncui-security/src/authz.rs)
values bound to the authenticated subject, method, capabilities and exact normalized parameters.

- **Agent/dispatch path — done.** A destructive method reads `consent_token`, checks it against
  the minted ledger and spends the nonce (`Runtime::mint_consent` / `Runtime::consume_consent`).
  A bare `consent: true` authorizes nothing.
- **Native page-command path — done.** `window::run_native_page_command` applies the same
  standing authorization, rate limit, token validation, one-time spend and audit rules. Forged,
  altered, wrong-subject, wrong-command and replayed tokens are refused without spending the
  valid operation's token or falling back to a prompt.
- **Host confirmation — implemented.** The desktop CLI installs an `rfd` warning dialog before
  the first window opens. Only an explicit Yes approves; decline, close, cancellation and errors
  withhold approval. The prompt contains the complete bounded operation, and approval mints and
  immediately spends a token in the same call.
- **Platform evidence — Windows and Linux.** Integration tests prove the exact-operation
  approval contract with an explicit host fixture. The real Windows dialog was driven through
  its actual Yes and No buttons: Yes ran the destructive command and wrote its marker; No left
  no output. Under WSL with X11 and Openbox, the real `zenity` dialog returned success on Yes and
  `capability_denied` on Escape. Token issuance alone was not treated as approval evidence.
  macOS remains unverified because no macOS host is available.

## The platform locale is read from the environment, so Windows reports `"und"`

`platform_locale` reads `LC_ALL` then `LANG` and strips a codeset suffix. That is where a Unix
platform exposes its locale. Windows exposes it through `GetUserDefaultLocaleName`, an FFI call
this runtime does not make (`unsafe_code = "forbid"`, and an FFI reader is a scoped, audited
addition under ADR-012). On such a platform the locale is `"und"` (undetermined) and the derived
text direction is `"ltr"`, so a page is told the locale is unknown rather than handed a wrong
one. The locale and direction still flow end to end — the page and the island descriptor carry
them — but on Windows they say "undetermined" until the FFI reader lands. An application that
knows its language declares `default_locale` in its manifest, which is validated and available
to override the platform fallback once the two are wired together.

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

## Bidirectional text is a bounded algorithm, not the whole of UAX #9

A GPU island row's text is shaped with `rustybuzz` (a HarfBuzz port), and HarfBuzz shapes one
direction at a time: it reorders a run that is all right-to-left, but a mixed-direction line handed
to it whole is shaped in its first strong direction with the other runs left in logical order — which
drew an embedded Hebrew or Arabic word backwards. `ncui-gpu-renderer`'s `text` module now splits every
line into single-direction runs (`bidi_runs`) and shapes each with its own direction before
concatenating them in reading order, so a right-to-left word in a left-to-right sentence and the
reverse are laid out correctly, and two right-to-left words keep their phrase order.

**What is bounded about it.** The splitter classifies strong characters by script (a fixed list of
right-to-left blocks: Hebrew, Arabic, Syriac, Thaana, NKo and the Arabic presentation forms) and gives
a neutral — a space, punctuation, a digit — the direction of the strong text around it, falling back to
the paragraph's base direction (the first strong character's) when the two sides disagree. It has no
separate handling for the Unicode bidirectional marks (`LRM`/`RLM`/`RLE`/…), no paired-bracket
resolution, and no number shaping of its own: a number follows its run's direction rather than being
laid out by UAX #9's more detailed `EN`/`AN` rules. The common cases — an embedded right-to-left word,
two right-to-left words, a right-to-left paragraph with a left-to-right word in it — are handled and
unit-tested (`a_right_to_left_word_in_a_left_to_right_sentence_is_reordered`,
`two_right_to_left_words_are_reordered_as_one_run`); the exotic ones are not, and a line that leans on
directional marks or bracket pairing may still lay out incorrectly. The full algorithm is what a
dedicated bidirectional crate provides, which is a scoped, audited addition under ADR-012 if it is
wanted.

Joining is verified for Arabic (a letter inside a word takes a joined form, not its isolated form),
which is what "scripts with ligatures/joining" means here; Latin ligature substitution depends on the
font's own default features and is applied by the shaper when the font has it, not by this code.

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

`comparison/` measures NC-UI against Electron and Tauri 2 on two real applications. The
2026-11-24 fail-closed run completed five repeats and five 200-round IPC samples for every
configuration. NC-UI is 29× smaller than Electron and starts 4.6× faster; against Tauri 2 it
starts 1.05× faster and is 3.5× faster on IPC. All three report 0.00 % framework-process CPU at
idle. The Electron port is slower on IPC (0.351 ms against 0.196 ms) and larger at idle
(361.9 MiB against 304.6 MiB). The Tauri port is 3.6 % larger at idle (362.5 MiB against
349.9 MiB) and larger on disk (12.3 MiB against 8.5 MiB). The standard build ships the GPU
island renderer; earlier isolated builds measured the renderer at about 6.4 MiB, the same host
without it at about 4.9 MiB, and no window host at 1.64 MiB. Those are historical decomposition
measurements, not a fresh breakdown of the current 12.3 MiB binary. The harness and
`comparison/bench/results/windows.json` are the evidence.

The IPC figure has a known cause and a measured history. A page call crosses the webview
boundary twice (the page's `postMessage`, the host's `evaluate_script`) and the host's event
loop runs on its own thread, so the response makes a thread hop Electron's direct C++ IPC does
not. Before the page loop blocked on the engine's message channel, the same measurement was
16.4 ms; the blocking wait brought it to 0.52 ms, and removing the asset server's 2 ms accept
poll took it to 0.375 ms. The current Electron-port median is 0.351 ms and the Tauri-port median
is 0.410 ms. The in-process half of that round trip — capability check, handler, response
serialisation — is measured at a p99 of 900 ns, so the remaining gap is the boundary itself,
and the reply can only be delivered by evaluating a script in the page: `wry`'s public
API exposes no `PostWebMessageAsJson` seam, and `unsafe_code = "forbid"` rules out calling the
COM interface directly.

The idle CPU figure had a cause this document first got wrong. The page loop's 50 ms wait was
the suspect, and raising it fourfold changed nothing — 0.94 % of a core over a ten-second
sample either way, which is how the hypothesis was disproved rather than assumed away. The
consumer was the asset server's accept loop: it accepted on a non-blocking listener and slept
2 ms between attempts, about five hundred wakeups a second for an application serving
nothing. That became a channel receive with a 250 ms timeout, which read 0.00 % idle — but
the timeout was itself a latent latency defect, because an *incoming* connection cannot wake a
channel receive, so every fresh connection waited for the poll to expire before `accept()` ran
(a page's asset fetch measured a p50 of ~253,000 µs). The accept now blocks in the kernel and
`stop` wakes it by opening one throwaway loopback connection, so the idle CPU stays 0.00 %
while a connection is answered the moment it arrives (~560 µs p50) rather than on the next
poll. Both numbers are measured over a real socket, not asserted.

The current memory gap is the webview runtime, and an earlier decomposition measured it:
`ncui.exe` was about 26 MiB against Electron's 95 MiB main process, while the WebView2 runtime's own browser
process is about 129 MiB — a process Electron folds into its main process — and the renderer
and GPU processes are smaller than Electron's. The size gap to Tauri is the runtime's own
machinery — scheduler, metrics, audit log, worker pool, capability checks, and the GPU island
renderer — which a minimal Tauri binary does not carry. Neither is a defect this document
hides; both are recorded so a reader comparing the three frameworks starts from the
measurement rather than from a claim.

The largest single one of those, the GPU island renderer (measured at 6.4 MiB in an earlier
isolated build), is **optional at link time**: `ncui build --profile minimal` ships the window
host without it and measured **4.902 MiB** on the counter example in that earlier comparison.
The current default `standard` build is **12.3 MiB** after native-menu and other runtime work.
That historical minimal measurement narrows the size gap to Tauri 2's 8.5 MiB for applications
that draw no islands, but it is not a fresh measurement of today's minimal profile. What
remains — a window host without the renderer previously measured ~4.9 MiB on this toolchain, because
that is the WebView2 host itself plus the runtime's own machinery — is the residual, recorded rather
than overclaimed. An application that draws no islands and ships `minimal` is within 1.7 MiB of
Tauri; one that draws islands, or ships the default `standard`, still carries the renderer by
choice.

A2's "≤ 300 MiB idle working set" target is **blocked on the engine, not on host allocations**.
The current comparison measures 361.9 MiB for the NC-UI Electron port and 362.5 MiB for its Tauri
port. A separate earlier decomposition of a minimal scaffolded application — a window and a counter,
granted only `read.metrics` and `window` — the tree measures **317.4 MiB**, of which
`msedgewebview2.exe` is **301.6 MiB** and `ncui.exe` **23.1 MiB**. The host is already lean and
the subsystems A2 names are now lazy: the worker pool spawns its threads on first submitted task
rather than at startup, the GPU renderer's `host` stays `None` until islands install, and the
metrics/log rings bound their pre-allocation. A minimal app's worker pool therefore spawns **zero**
threads, and the tree is *still* over target — because WebView2's own processes alone measure
301.6 MiB. No lazy-initialisation of host subsystems can close a gap that is the web engine's
multi-process footprint. Reaching ≤ 300 MiB requires changing the engine or its process model — a
different WebView port, a single-process mode, or a lighter engine for minimal apps — which is a
project decision, recorded here and in the goal prompt rather than claimed as met.

## A command can be answered by any executable or by the page's own JS

C3 asked for two things: a command answered by *any* executable, and a command handler written
in the page's own JS. Both are delivered and gated.

The any-language sidecar is real. An application declares it in `ncui.app.json`
(`sidecars: [{command, program, args, requires, timeout_ms, mode, summary}]`), the runtime writes the
call payload to the program's stdin and reads one JSON answer from its stdout, and the command
is gated twice — the application must hold `process.spawn` for any sidecar to register, and each
sidecar's `requires` must sit inside the page grant and gates the call at the seam. It runs
inside the `process` runner's containment (root-resolved program, secret-scrubbed environment, no
shell, timeout, bounded output, process-tree stop), and its working directory is pinned to the
app root so a relative script path resolves next to the manifest — a fix the end-to-end tests
forced, because an interpreter resolves its script argument against the child's cwd, not the
host's. A non-zero exit or a non-JSON answer reaches the page as an error. It is tested against
a real interpreter end to end (`crates/ncui-runtime/src/native.rs`, five sidecar tests) and the
manifest declaration is tested from real JSON (`crates/ncui-app/src/manifest.rs`).

A sidecar that pays a start-up cost on every call — a model, a connection, a warm index — can
declare `mode: "stream"` instead: the program is started once, on the first call, and kept
running, and each call writes one newline-delimited JSON line to its stdin and reads the answer
line that names the call's `id`. The wire is the NDJSON dialect log pipelines speak, an answer
that reports an error reaches the page as that error, and every call is timed against the
sidecar's own limit. The program's containment is the one-shot runner's — the same scrubbed
environment, the same job (or process group) that ends its whole tree when the runtime stops —
and its state across calls is the point: a counting sidecar answers 1, 2, 3 to three identical
calls, which is how the mode is tested.

The JS half is delivered by the webview-callback round trip, with no new dependency. An
application declares a page command (`page_commands: [{command, requires, summary}]` in
`ncui.app.json`) and the page registers the function that answers it with
`window.ncui.registerCommand`. Calling it through the usual `window.ncui.invoke` seam is gated
exactly like a native command — declared, and the page holds `requires`, or the seam refuses it
under the page's own correlation id so its promise rejects rather than hangs. Because `wry`
exposes no way to read a value back out of `evaluate_script`, the runtime does not answer
directly: it holds the original invoke open, evaluates `window.ncui.__runCommand(replyId, name,
payload)`, the handler runs and posts its answer back under `replyId`, and the runtime settles the
original invoke with it. A handler that is missing or throws is reported to the page as an error
rather than left to time out; a command declared but never handed a handler answers with an
error rather than running anything. A component cannot reach a page command. It is tested end to
end through the real page pump (`crates/ncui-runtime/tests/page_bridge.rs`) and against a real
WebView (`crates/ncui-webview-wry/src/host.rs`). The **component ABI** remains the first-class
path for precompiled native modules in an AOT language that emits WASM.

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

## The release binary is reproducible in a fixed build directory

`python tools/build_release.py` generated the SBOM, built the release twice with
`SOURCE_DATE_EPOCH=1704067200` and incremental compilation disabled, and compared the digests.
Both Windows builds produced
`ee3be54581946f5a7b72c55243b9a6643cf76e078bedc5fd6db49742511cb32f`.

The earlier cross-directory experiment still explains the boundary of that claim: a graphics
dependency embeds its `OUT_DIR`, so building the same source under a different target directory
changes path strings and shifts later data. The release pipeline therefore rebuilds twice in
the same directory after `cargo clean`; it does not claim that a consumer's build elsewhere will
be byte-identical. The `/Brepro` linker flag fixes the PE timestamp and PDB age, while the fixed
directory removes the known path-dependent input. No signing key is configured on this machine,
so the detached-signature step remains unexercised.

## The page bridge carries JSON only

**The command bridge carries JSON.** A request is a JSON object and a response is a JSON
object. A separate bounded raw `POST` channel now carries large binary payloads without JSON
encoding, but the WebView command reply still travels back as an evaluated script because the
engine's page-message seam hands the host a string. The agent transport's binary framing
(`ncui-ipc`) is not the page's command transport.

**Consent transport fields are reserved, not authorization.** `consent_token` is validated
against the runtime's ledger and exact operation, then spent once for a destructive call. The
legacy `consent` boolean is ignored. With no supplied token, the installed host prompter must
obtain real approval; malformed or mismatched tokens never fall back to that prompt. Both fields
are excluded from the operation digest and from the confirmation text.

## An application is a directory, and there is no bundler

The application story exists: a manifest (`ncui.app.json`) names the entry page, the
assets, the window and the granted capabilities; a loopback asset server gives the page a
real origin; `ncui run` starts the server, opens the window and answers the page's calls
until the window closes (ADR-014). **`ncui new <directory>`** now writes that first
application for you — a manifest, an entry page and a script — so first-run-to-window is one
command rather than a blank directory and a page of documentation. The scaffold grants the
smallest capability set that opens a window and answers the page's first calls (not
everything, which would teach the wrong habit on day one), and its entry page includes the
reload bridge and designates its state, so the dev loop the runtime ships is the one the
template demonstrates. It refuses to scaffold over an existing non-empty directory, because
scaffolding over work in progress would destroy it. What is *not* built is the step after
that: turning what a developer writes into the files the server serves.

**There is no bundler.** The asset server serves the files that are on disk. An
application that imports a package from `node_modules`, or that writes TypeScript, has to
build its own output first. The server does not transform a file it is asked for.

`ncui run --watch` closes the other half of the loop — a change reloads the page — and the
reload now **preserves the state the page asks to keep**. The page designates what it wants kept
by assigning it to `window.__ncui_state`; before reloading, the runtime snapshots that one value
into `sessionStorage`, and after the reload a bridge script restores it and dispatches
`ncui:state-restored`. The bridge is opt-in: an application includes one `<script
src="/__ncui/reload.js">` tag on its own origin (the counter example does), and one that does not
gets the old fresh-reload behaviour unchanged. State the page does not put on `__ncui_state` is
*not* preserved — a reload tears down the JavaScript heap, which is what a reload is, so this is
one well-defined slot the application opts into, not a framework's component-tree resurrection.
The bridge is served from the runtime's own route table (the same mechanism that serves a GPU
island frame), so the asset server stays a file server that rewrites nothing, and the mechanism
carries no new dependency.

**The mechanism has a sharp edge, and it is the part a page must get right.** The bridge is a
classic script and the application's own code is usually a deferred module, so the bridge restores
`window.__ncui_state` *before* the application runs. A page therefore has to **seed its state from
the restored global** — read `window.__ncui_state` into its working variables on load. A page that
initialises from a literal (`let count = 0`) and writes that back in its render function mentions
`__ncui_state`, passes a check that only looks for the name, and preserves *nothing*: the literal
clobbers the restored value on the first render. The counter example and the `ncui new` template
both seed from the global, and a test asserts the scaffolded script seeds from it rather than from
a literal — because this was got wrong once and a name-only check did not catch it.

A bundler is still a separate tool with its own dependency decision: `--watch` reloads what is on
disk, so a TypeScript file is not compiled by it. That decision — a built-in Rust transform layer
versus a dev-server proxy to an external bundler — trades directly against the no-Node, auditable
surface this project treats as its moat, and is recorded here rather than made unilaterally.

**The asset server sends cache validators.** Every file response carries both a strong
`ETag` (derived from the file's modification time and the length actually served, so it can
never disagree with `Content-Length`) and a `Last-Modified` HTTP-date. A conditional `GET`
that echoes either back is answered `304 Not Modified` with no body, so a `--watch` reload
re-sends nothing it does not have to: the cold fetch is a `200` and every request for an
unchanged file is a `304`. `If-None-Match` takes precedence over `If-Modified-Since`
(RFC 9110 §13.1.3), a date the server cannot parse is treated as no condition rather than
as a match — a false `304` would hand back an empty body — and the date comparison is done
in whole seconds, because that is the resolution an HTTP-date carries. Both paths are
measured over a real socket (`app.asset_get_200`, `app.asset_conditional_304` in the latency
harness) rather than asserted.

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
hidden or focused, or give it a new title, place, size, float or resizability while it runs. The manifest
also declares native menus, and a selection reaches the page as a `menu` event. What is still missing is a
per-application display mode.

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
| File, save and folder dialogs | Implemented behind the `dialogs` feature, except the one step a test cannot drive | The vocabulary (`open.file`, `open.files`, `save.file`, `pick.folder`), the option and filter rules, the validation of the path that comes back and the command registration under `Capability::Dialog` are plain Rust in `ncui-native::dialog` and are tested headlessly in both feature states — a page holding the capability gets `dialog.pick_file`/`dialog.save_file`, whose request rules are enforced whether or not the build can show a picker, and which reports `dialog_unavailable` by name on a build without the feature rather than claiming success. The `rfd` adapter behind the feature shows the platform's own picker. **What is not automated:** the single real modal open — a person clicking the picker and choosing a file — is the one step a test cannot drive, because a test has no person. This project's standard is that a capability is evidenced by running it, and everything *around* the modal is so evidenced; the modal itself needs a one-time human verification the feature registry does not model. It is recorded here rather than claimed. A **colour** dialog is implemented the way the clipboard is: `rfd` has no colour picker, so the
picker is the platform's own `ColorDialog`, reached through the tool every Windows machine ships
(PowerShell, ADR-015's platform-tool shape). The request (`dialog.pick_color`, requiring the
`dialog` capability) carries an optional title and an optional `#rrggbb` seed, both validated
headlessly in `ncui-native::dialog`; the answer is the chosen `#rrggbb`, a cancellation, or a
named failure. The modal itself — a person clicking a colour — is the same one-time human
verification the file dialogs record, and the picker is Windows-only until another platform's
tool is added. |
| Tray icon | Implemented: the manifest declares an image and a tooltip, the host creates it on its engine thread, and a click brings the application's window forward and tells its page | — (the platform's own call, through `tray-icon`) |
| Native menus | Implemented on Windows and Linux: the manifest declares a bounded recursive tree, the host creates the platform menu, and a selection reaches the page as a `menu` event | macOS remains unverified. Windows needs muda's `pub unsafe fn init_for_hwnd`; ADR-012's ninth exception confines that one unsafe block and call site to `crates/ncui-webview-wry/src/menu.rs`, on the engine thread, with the menu dropped before its HWND. Labels do not imply native accelerators; applications declare global shortcuts separately. Windows' repeatable probe dispatches the real menu through `WM_COMMAND` and verifies the page handler; a physical mouse click remains a one-time human verification |
| Developer tools (the platform's web inspector) | Implemented on Linux; partial on Windows | `ncui.window.devtools.open` and `ncui.window.devtools.close` call wry's `open_devtools`/`close_devtools`; `ncui.window.devtools.state` answers `open`, `closed`, or `unknown`. Linux's real WebKitGTK inspector is verified open and closed by a live host test. On **Windows** the state answers `unknown`, because wry's Windows backend opens the inspector and then reports `is_devtools_open() == false` unconditionally. The runtime says what the platform told it rather than claiming a state it cannot know. The three calls require the `window` capability |
| Global shortcuts | Implemented: the manifest declares them, the runtime holds them while it runs and releases them when it stops, and the page hears a `shortcut` event | — (the platform's own registration call, through `global-hotkey`) |
| Chrome DevTools Protocol (drive a page's engine: navigate, evaluate, inspect) | Implemented on Windows behind `Capability::Cdp`; explicitly unsupported on WebKitGTK | The Windows engine is created with `--remote-debugging-port=0`; the port and bearer path are read from the user-data directory's `DevToolsActivePort` file, and `ncui.cdp.send` delivers the command over a loopback WebSocket to the page's own DevTools endpoint, returning the endpoint's answer (ADR-017 Phase 2; no `unsafe`, no engine patching). A page can still relay through `ncui.on("ncui:cdp", ...)`. WebKitGTK exposes its native inspector but neither publishes `DevToolsActivePort` nor speaks CDP; non-Windows calls return `cdp_platform_unsupported` by name rather than waiting for a file that cannot exist. WebKit's WebDriver automation is a different protocol and is not presented as CDP. Platform-specific host tests cover the live WebView2 exchange and the WebKitGTK refusal. Logical child windows (`ncui.window.create.child`) are a separate top-level-window API |
| Embedded child webviews | Implemented on Windows and Linux: a parent owns the child's lifecycle and isolated policy, the engine embeds a real surface, and create/bounds/close/list plus targeted page IPC are verified on both platforms | Geometry is explicit rather than DOM-synchronized; a `<ncui-webview>` custom element remains future work, and macOS is unverified |
| Running on macOS | The code is written; the platform is not verified | AppKit requires the event loop on the main thread and `tao` offers no any-thread switch, so on macOS the engine loop owns the main thread and the runtime pump runs on a worker (ADR-018). Cross-compiling even a `cargo check` for the macOS target stops in `objc2-exception-helper`'s build script — this machine has no C compiler for the Apple target — so the path is reviewed but **not compiled or run here**; a platform is not "supported" until its gate has run on it, and `cross_platform.verification.macos` stays `not_implemented` until a macOS run says otherwise |

A new native capability needs an ADR-012 exception when it requires a new dependency — the same bar
`wry`, `wgpu`, `rustybuzz`, `global-hotkey` and `muda` had to clear. Adding one without that bar is how
a runtime ends up with an unaudited native surface, which is the failure this project's capability
model exists to prevent.

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

**A timed-out program's tree is stopped, and on Windows the tree is a job, not a walk.**
`process.run` stops the program it started *and everything that program started*. On Unix
the child is started in its own process group and one signal reaches the whole group. On
Windows the child is put into a job object with kill-on-close (ADR-019, through the
`win32job` safe wrapper), so closing the job — at a timeout, or when the run returns —
terminates the child and everything it started, however deeply nested and however
successfully its descendants hide from a parent-chain walk. A test drives both paths: a
grandchild appends to a file once a second, the parent's run is stopped by its time limit,
and the file must stop growing; a second test lets the child exit while the grandchild
keeps ticking, and the grandchild must still end when the run returns. Disabling the tree
kill makes the first test fail with the grandchild's extra bytes, which is how the fix was
checked rather than assumed.

The residual is narrower now, and it is the platform's shape rather than a walk's: a
process created with breakaway is refused a start under this job rather than escaping it,
and a grandchild created in the window between the child's process creation and its job
assignment is not in the job (the fallback walk still reaches the ordinary cases). A
backend that genuinely needs to outlive its caller must not be run through this runner.
Closing the last gap on Linux would mean a cgroup, which `std` does not expose and this
workspace cannot reach without FFI or a new dependency. The Unix path is compile-checked
here and exercised by CI on Linux; the Windows path is the one measured on this machine.

## macOS has no host here

`cross_platform.verification` was one entry covering two platforms. Linux ran; macOS did not.
It is now two entries, because a single entry would have let the completed half hide the
unrun one — which is what the registry's `unverified` platform states exist to prevent.

## Mobile has no host, and no claim is made that it does

The capability table above is desktop: Windows, Linux, macOS. There is no iOS or Android
host, no touch input pipeline, and no mobile packaging, and — unlike macOS, where the host
is written and waiting on a machine to run it — no mobile code exists to verify. The `wry`
engine compiles for those targets, but an engine that can be compiled is not a platform
story: a platform is supported when its gate has run on it, and there is nothing here for a
phone to run. Adding mobile means a host per platform (UIKit/Android view integration), a
touch and lifecycle contract in the input model, and store packaging — a project of its own,
recorded here so the absence is a decision rather than an oversight.
