# nc-ui agent notes

## Streaming bridge state (M1)

- `crates/ncui-runtime/src/stream.rs` — subscription registry + cloneable
  `StreamSink`. The sink posts through the Arc-shared window registry, never
  the runtime lock. Cancellation: `Runtime::close_stream`, window death,
  runtime teardown. Inspect counts delivered/failed exactly.
- `Windows` now holds `registry: Arc<Mutex<Option<WindowRegistry>>>` — the
  sharing is deliberate, for stream sinks. Do not regress it to a bare Mutex.
- Live evidence: `cargo test -p ncui-runtime --test stream_bridge_live --features wry-host`.
- Page-initiated streaming: register with
  `Runtime::register_page_command_for_caller_window` — `params.window` arrives
  injected from the call's origin. The handler must capture
  `Runtime::stream_factory()`, NEVER the runtime itself: commands execute on
  the pump thread while the runtime lock is held, so a captured runtime
  deadlocks the first invoke. The factory is bound to the registry it was
  taken from — take it after any `windows` replacement.
- Fixed latent defect: the native-command path passed raw payload, not the
  injected params — caller-window injection was silently dropped for every
  native command until the first consumer exercised it. If injection seems
  missing, check `window.rs` passes `params` (not `payload`) into PageCall.

## 4K30 presenter fix (closed 2026-11-28)

The video presenter copied every frame twice (33MB `.to_vec()` at 4K even when
aligned, fresh padding per frame, recreated view/bind-group). Fixed with
zero-copy aligned upload + reused padding + persistent view/bind-group in
`crates/ncui-video-surface/src/win32.rs`. Ten-minute verdicts, both green:
4K30 p99 11.39ms (was failing 35.36ms), degradation flat; 1080p30 p50 1.58ms.

## Continuation state (handoff 2026-11-28, second)

CLOSED this session: 4K30 defect (ten-minute acceptance green both
resolutions); M1 streaming bridge core — primitive + page-initiated
composition both proven live, two real defects fixed along the way (native
commands dropped the caller-window injection; the lock-free StreamFactory
pattern). 225/225 runtime lib tests green.

CLIPPY DEBT CLEARED (2026-11-28): all three media crates clean under
`--all-targets`. Two fixes worth knowing: the presenter frame_view/sampler
fields are ownership retention (reads happen through the bind group) with
documented allows; `dispatch_page_action` now MOVES payload into params
instead of cloning — a copy removed from every invoke on the hot path.

REMAINING ARCS in priority order: input.ime editor-grade proof (U1-U3);
M2 sidecar lifecycle; M3 shell integration; V2 component.open hang repro;
V3 provenance stamping; gpu.island_custom editors + font licensing. M1's
hardening remainder: per-window subscription limits (documented in the
register). All work uncommitted — inspect `git status` before continuing.

## input.ime scoping finding (2026-11-28, CORRECTED) — the arc is UNBLOCKED

First finding was correct but answered the wrong question: tao/wry's IME is
indeed minimal (`ReceivedImeText`, commit-only, even in tao-0.38) — but that
is the WEBVIEW CHROME path, and island text editing does not happen there.
The IME target the register means — composition inside a custom-drawn
island — is the GPUI island surface, and gpui 0.2.2's Windows backend HAS
the full plumbing: `WM_IME_STARTCOMPOSITION`/`WM_IME_COMPOSITION` handlers,
`PlatformInputHandler`, `set_input_handler` on the window
(gpui-0.2.2/src/platform/windows/events.rs, window.rs). The arc is therefore
WIDGET-LEVEL work in nc-ui-kit (the U2 arc): implement InputHandler in the
text editing widgets — marked text, candidate placement from the caret rect,
commit/cancel, UTF-8↔UTF-16 mapping — then produce the registry evidence in
nc-ui with a real IME (Japanese/Chinese). No windowing-layer surgery, no
tao fork. nc-ui-kit already references InputHandler in ui/input.rs,
ui/edit.rs, ui/rich.rs — assess what is partial there first.

## Hard lesson: this project's surface is large — check before writing

The register's gap analysis understates what exists. Before writing ANY new
module: grep the ncui-native/ncui-runtime/ncui-app sources for the concept
first. Concrete examples learned the expensive way: `single_instance.rs`
already existed complete (748 lines, loopback handoff, token auth, stale-lock
takeover) when a duplicate was almost written over it — the overwrite was
caught only because the module registration collided, and restored from git.
`StreamingSidecar` already existed for the NDJSON wire layer when the
supervision crate was designed. M3's dialogs/open/associations are largely
BUILT. The honest gaps are smaller than the register implies.

## M2 sidecar lifecycle — CLOSED 2026-11-28

ncui-sidecar: ZERO unsafe (per-sidecar win32job job objects — kill is dropping
the job handle, the kernel ends the whole tree). Restart-on-crash with backoff
+ counted ceiling; transport survives restart (stable pipe ends, forwarder
follows incarnations); death-window writes error honestly. Runtime owns it as
`runtime.sidecars`; drop is the app-exit path. Evidence: 7/7 crate tests +
4/4 runtime wiring tests. Race fixed along the way: incarnation pipes publish
BEFORE status flips to Running (writes in that window would vanish into the
dead pipe). Test-harness lesson: self-spawned libtest children glue harness
noise onto echo lines — match with ends_with, never equality.

## M3 shell integration — CLOSED 2026-11-28

The spec's surface is now all present and verified: dialogs, open-with-default,
`reveal_in_folder` (detached explorer `/select,` — never contained, explorer's
exit code is meaningless), single-instance lock with loopback handoff
(pre-existing, 748 lines), `userdata.rs` (config/data/cache separated on the
platform's own roaming conventions), association primitives, and frameless
window drags. Edge-resize added this session end-to-end: `ResizeEdge` port
type, `WebViewBackend::drag_resize`, `HostEvent::DragResizeWindow`,
`Windows::drag_resize`, `ncui.window.drag.edge` (page method, `edge` param).

SECOND latent-seam defect this arc (same family as the caller-window
injection): `WindowHost::drag_window` existed but was never bound into the
`WebViewBackend` impl — the trait default meant every runtime drag request
answered false on wry. Overrides added for both drag calls. Lesson reinforced:
an API existing at every layer except the trait-impl seam is a designed,
wired, broken feature — grep trait impls, not just method definitions.

gpui island windows intentionally keep the `false` default — they own their
chrome natively, never frameless-via-page.

## M2 composition + M1 hardening — CLOSED 2026-11-28

M1's remainder landed: `MAX_STREAM_SUBSCRIPTIONS = 64` per window (the
workspace's own bounding precedent), open is fallible, refused as Conflict;
close frees capacity. Test: `a_window_cannot_own_the_pump`.

M2 composition landed: manifest `stream` sidecars now spawn through
`runtime.sidecars` (OnFailure, 8 restarts, 200ms backoff) — a crashed backend
recovers instead of refusing forever. The seam: `SidecarHandle::into_transport`
hands the stable pipes to `StreamingSidecar::from_pipes` (boxed pipe ends,
Option<Child>, transport-open atomic for supervised liveness). Supporting
changes: `SidecarSpec.env_clear` (supervised children get the scrubbed env),
`resolve_program` is pub (containment survives the alternate spawn lane),
`Supervisor: Clone` with teardown moved to `SupervisorInner` Drop — the LAST
handle's drop kills, a clone leaving scope cannot murder siblings.
Evidence: `a_supervised_sidecar_survives_a_crash_through_the_wire` — crash,
restart, third call answers through the same wire.

TEST-HARNESS LESSON, stage two: libtest noise does not just glue onto echo
lines — it makes answer lines NON-JSON, which a protocol reader must drop.
The fix for protocol children: emit `
` + payload + `
` in ONE atomic
write so the noise line ends and the answer owns a whole line.

## Verification


- `make gate` runs exactly what CI runs — fmt, lint, test, doc, tools, policy, client, budget. Every target is a hard gate.
- Feature-gated truth: the renderer's GPU tests need `wgpu-device` (default-on in ncui-gpu-renderer); the runtime's live WebView2 tests need `--features wry-host` and are Windows-only. `cargo test --workspace` alone does not exercise them — the gate's dedicated legs do.
- Live video-surface tests: `cargo test -p ncui-video-surface --test win32_live`, `cargo test -p ncui-runtime --features wry-host --test video_surface_wiring_live`, `--test video_surface_composite_live`, `--test video_surface_acceptance_live` (the acceptance pair wants `--test-threads=1`; duration via `NCUI_VIDEO_ACCEPTANCE_SECS`, default 30, acceptance 600).
- Registry validation: build the tools (`cd tools && cargo build --release`), then `./tools/target/release/ncui-registry protocols/feature-registry.json`. Entries must carry every evidence dimension — see `input.pointer_capture` for the shape. `tools/record_execution_provenance.py` stamps execution after a full gate run, not piecemeal.
- The workspace forbids `unsafe_code`; `ncui-video-surface` is the single documented boundary crate (copies the workspace lints with `deny` instead of `forbid`, because cargo cannot override one inherited lint and raw Win32 child windows are impossible without FFI). Every unsafe block there carries its invariant. Do not spread unsafe elsewhere.

## Lessons the evidence cost real iterations to learn

- Live window tests must pump the runtime on a dedicated thread (`Arc<Mutex<Runtime>>`, `wait_page_message` event-driven pacing). Interleaved pumping starves because a blocking CDP call can freeze the only serving thread.
- Large evaluated scripts hang the WebView2 devtools endpoint; test harnesses belong in the served page, with small CDP probes reading flags.
- CDP evaluates are at-least-once — they can execute but lose the response. Triggers must be idempotent page-side.
- Screen capture on a machine its owner is actively using is weather, not evidence: browsers occlude test windows regardless of topmost flags, and the multi-monitor virtual screen bounding box contains dead space. The load-bearing window-compositing evidence is z-order plus GPU readback, which is environment-independent.
- Frames are the hottest path in the runtime. Producers must hold their own cloneable registry handle (`Runtime::video_surface_producer()`); sharing the runtime lock with the page pump cost a measured p99 of 63ms against the 33ms frame budget (mutex convoy) before the handle existed.
- Writing large Rust files: shell heredocs truncate around 8k characters in this environment — use the write tool or chunked appends. Python interprets `/tmp` differently than git-bash on Windows; use relative paths for handoffs.
- `cargo fmt` in this repo is scoped per crate (`cargo fmt -p <crate>`); the baseline is not uniformly formatted.

## Continuation state (2026-11-28, third session) — gate green

ALL ARCS CLOSED. The remaining arcs from the second handoff are done: input.ime
(widget layer in nc-ui-kit, 8/8 tests; real-IME evidence is interactive-only and
documented), M1 subscription limits, M2/StreamingSidecar composition, M3 shell
integration, F2 component.open consent repro (live: prompt surfaces, yes answers,
open+invoke settle ok; residual defect documented — agent-transport consent holds
the runtime lock while rfd waits, lock-hostage measured). V3 provenance stamping
is live: `tools/record_execution_provenance.py` stamps `execution` blocks from
gate-test-*.log; 81/83 complete features stamped (supply-chain and linux entries
honestly unexecutable on this machine; macOS is not_implemented — no host).

Full `make gate` composition is green on this machine: fmt (nc-ui crates; the
nc-ui-kit sibling's baseline is deliberately unformatted per its own AGENTS),
clippy -D warnings, all five test legs (workspace 88 suites, islands 13/13,
islands-lib 226/226, wry-host 29 suites, tools), doc -D warnings, tools, policy
(every scanner + arch layers + test counts + component generators), client
14/14, budget within ceilings.

Lessons the gate run cost:
- `record_execution_provenance.py` treats EVERY cited `crates/*/src/*.rs` path
  as requiring that crate's lib-test binary to have passed tests. Crates whose
  tests live only under `tests/` (ncui-video-surface, ncui-audio-output,
  ncui-web-shell) were unstamplable until the tool learned to check whether the
  cited file actually contains tests — a src citation is often just an
  existence claim.
- The AV ring's read position is anchor-plus-count for the stream's life:
  mid-stream pts reassignment is IGNORED. Corrections and scrubs actuate
  through `SampleRing::flush()` — the next push on an empty ring re-anchors.
  The av_sync_live test's "correction" only became real when the producer
  flushed; before that it measured pipeline depth and called it a shift.
- The placeholders policy scanner flags words like `stub`/`fake`/`placeholder`
  even in comments; `todo = "deny"` in a [lints] table needs an inline
  `ncui-allow-placeholder:` justification. `ncui-arch`'s LAYERS table must be
  extended for every new crate — adding a crate without a layer is a policy
  violation.
- `cargo test --workspace` kills with LNK1104 if an earlier bench process is
  still holding its exe — check for zombie `latency-*.exe` before assuming
  disk corruption.
- clippy's let-chain style (`if let Some(x) = y && cond`) is enforced at
  `-D warnings`; collapsed ifs must rebalance their closing braces.

All work remains uncommitted (~130 files) — the user has not asked for a commit.

## Continuation state (2026-11-28 end of session)

DONE AND GREEN: media.video_surface registered (feature 83) — 1080p30 10-min sustained proven (p99 5.5ms, zero drops/degradation), 4K30 honestly not-sustained (buffer-pooling is the named fix). Audio model delivered in ncui-gpu/src/audio.rs (tests green). Registry validates zero violations. ncui-video-surface clippy-clean.

BROKEN AT HANDOFF — FIX FIRST: `cargo check -p ncui-runtime` fails with "unexpected closing delimiter" at crates/ncui-runtime/src/video_surface.rs:404, introduced during the final clippy cleanup (clippy --fix plus manual edits). Brace counting balances, so the damage is subtle — inspect the `count`/`inspect`/dead_code-allow region and the clippy --fix diff. Until repaired, the runtime does not compile. After repair: re-run `cargo test -p ncui-runtime --lib` (was 221/221), the video live tests, and finish the remaining clippy warnings in ncui-runtime tests (too_many_lines on the acceptance/wiring tests — allow with reason is the established pattern; pass-by-value pedantics in dispatch/video_surface.rs).

Audio backend design is in futureplan/what to add.md §2: cpal crate, WASAPI clock is QPC = same timebase as Instant, OutputStreamTimestamp.playback gives DAC-truth for the lip-sync measurement.

## Audio output arc state (2026-11-28)

`media.audio_output` is three increments in, not registered yet:

1. Model (`ncui-gpu/src/audio.rs`): ring never drops, counted underruns/refusals,
   anchor-plus-exact-count positions. 7 unit tests.
2. Backend (`ncui-audio-output`, cpal 0.18, zero unsafe): live-proven — positions
   exact, device delay ~31.9ms real, **drift 700ns-1µs** (QPC same-rate validated,
   the fact lip-sync rests on). WASAPI shared mode mixes per-stream.
3. Runtime wiring (`ncui-runtime::audio` + `audio_stream_wiring_live.rs` green
   in 3.3s): registry keyed by window, convoy-proof producer handle, lifecycle,
   inspect carrying accounting + sync reading.

Remaining in order: page-facing seam methods + the capability decision
(`Capability::Gpu` fits video; audio wants its own or a shared media one — design
decision NOT yet made), control intents (play/pause/seek/volume — none modeled yet),
the lip-sync measurement harness (correlated indices, arithmetic not perception),
then the registry entry. Full detail in `futureplan/what to add.md` section 2.

## F2 component.open hang — CLOSED 2026-11-29 (the suspected stall is disproven)

`crates/ncui-runtime/tests/consent_dialog_live.rs` (wry-host): the counter plugin's
full click — `component.open` then `component.invoke` on the real `greeter.wat` —
completes end to end in a live window. The CLI's own wiring is reproduced
(`NativeConsentPrompter` as the DEFAULT prompter), so the asking window's parented
`WryWindowConfirmation` is the approval surface. Measured: the `Confirm destructive
operation` dialog SURFACES (the roadmap's "never surfaces" hypothesis is wrong),
both prompts answer through `WM_COMMAND`/IDYES, the chain settles
`{ok:true, stage:"opened:greeter"}` — greet reads notes.txt through `fs.read_text`
under the negotiated grant.

REAL residual measured: the lock-hostage. `pump_pages` held `&mut Runtime` across
dispatch, so an unanswered prompt parked every other caller — an unrelated
`Mutex::lock` waited ≥1s while the prompt was held. CLOSED 2026-11-29 by the
structural fix, two-phase consent, applied on both hostage sites:

- `consent::gate`/`consent::settle` (crates/ncui-runtime/src/consent.rs): the
  gate judges under a brief lock (destructive + unconsented + rate-limit
  availability, non-consumingly) and returns a `ConsentGate` — `Dispatch` or
  `Ask { request, required, surface }`. The surface owns the prompt channel
  (parented `WindowConfirmation`, custom prompter, or default prompter, same
  ordering as before); `settle` consumes the person's answer under the lock
  into a minted `ConsentToken` or the identical `consent_denied` error + audit.
- Agent transport (`transport.rs::serve_request`): lock → gate → unlock →
  `surface.ask` (human wait with NO lock held) → re-lock → settle →
  `dispatch_with_consent`, which threads the token into `consent::resolve`'s
  new `pre_approved` slot. Denial builds `Response::from_error` identically.
- Page path (`window/page_dispatch.rs`, extracted under the file-size cap):
  `dispatch_page_action` parks a destructive call as `PendingPageConsent`,
  spawns a worker that asks with no lock held and posts the answer onto
  `consent_answers`; `pump_pages` drains answers and resumes the parked call
  through the same dispatch — rate limit, capability check, audit — with the
  request id attached at park time. Bounded: 8 parks, 120s TTL.
- Regression tests proving the lock is free mid-prompt:
  `agent_transport.rs::a_consent_prompt_parks_its_caller_not_the_runtime_lock`
  (try_lock + a served call while the person reads; decline arrives as
  `capability_denied` + denied audit) and
  `page_bridge.rs::a_consent_prompt_parks_the_call_not_the_pump` (a second
  call answers during the prompt; both approve and decline settle correctly).

The unapplied minimal alternative remains available: `recv_timeout` on
`WryWindowConfirmation::confirm` bounds the forever-wait — but the pump no
longer waits under the lock at all, so it is defense-in-depth, not a fix.

Evidence helpers added: `ncui-video-surface::evidence::post_window_command(hwnd,
command_id)` posts WM_COMMAND (dialog button click) — the safe wrapper this test
and future consent evidence reuse.

## Session-end disposition (2026-11-29) — what is genuinely left

CLOSED this session: F2 component.open hang — disproven as a hang, proven as a
working consent path; the lock-hostage residual it measured is now structurally
closed by two-phase consent on both sites (see the F2 block above). Registry
stands at 83/84 complete; the only `not_implemented` is
`cross_platform.verification.macos` — honest absence: no macOS host is
reachable from this environment, and the register itself records that.

REMAINING, with honest dispositions:

- **gpu.island_custom remnants** — NOT engineering gaps. The renderer
  foundation (display list, hit regions, blits, atlas, wire v2) is complete and
  pixel-proven. What remains is application work on top of it: the
  timeline/ruler/keyframe editors (product UI work, open-ended) and the font
  licensing decision (vendored OFL family vs per-platform system fonts — a
  product/packaging decision for a human, not code).
- **input.ime** — the widget layer is BUILT in nc-ui-kit (EntityInputHandler on
  Input/Textarea/RichText, UTF-16↔byte mapping, marked-range state, candidate
  geometry from shaped text) with 8/8 contract tests green. No
  `input.ime` registry entry exists — it is a roadmap arc, and its remaining
  evidence is a real-IME end-to-end run (Japanese/Chinese through the actual
  Windows TSF), which is interactive evidence, not scriptable.
- **Consent lock-hostage hardening** — CLOSED: two-phase consent applied on
  both hostage sites (see the F2 block). The only deliberate remainder is the
  optional `recv_timeout` bound on `WryWindowConfirmation::confirm` —
  defense-in-depth now that the pump never waits under the lock.
- **V3 provenance** — DONE: full gate run green on the final tree, 81/83
  complete features stamped (the two unstamped are honestly non-executable
  here: `cross_platform.verification.linux` has no Linux host,
  `process.supply_chain_releases` has no test-bearing citation).

All work remains uncommitted — `git status` shows 138 changed files (69
modified, 69 new) spanning the M1/M2/M3/F2 arcs, the consent lock-hostage
fix, and their changelog fragments. The full gate is green on this tree and
81/83 features are provenance-stamped; the remaining items are all honest
non-completables on this machine (macOS/Linux hosts, interactive IME
evidence, product decisions).
