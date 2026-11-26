- **Crashes leave a report on disk.** ADR-001's reporting half is real now: the runtime
installs a panic hook that writes one bounded JSON line per crash — version, wall-clock
moment, uptime, thread, message, source location, backtrace — into `NCUI_CRASH_DIR`
(default: the system temporary directory's `ncui-crash-reports`). The report's shape lives in
`ncui-observability::crash` and is written best-effort; the previous panic hook still runs, so
stderr output is unchanged. The destination and the hook live in the binary, which is where
the telemetry destination decisions already live.

Tested: the report builder's bounds (message, backtrace, multi-byte cuts), its one-line JSON
shape, its write-failure reporting, and an end-to-end test that installs the hook for real,
panics, catches, and reads the report back out of the crash directory.

- **A window's page can be printed to PDF and photographed.** `ncui.window.print.pdf` and
`ncui.window.screenshot` ride the CDP transport ADR-017 Phase 2 already built: the engine is
Chromium, and `Page.printToPDF` / `Page.captureScreenshot` are its own way to do both — no
FFI, no new dependency. Both require the `window` capability, validate their options
(scale, paper size, format, quality) before anything is asked of the engine, and return the
payload as base64 with its decoded byte length. An answer without the promised payload is a
conflict, not an empty success.

Tested: option validation, the base64 length arithmetic, refusals for windows that are not
open, the engine-answer extraction, and the surface sweeps (role reachability, advertised =
enforced, protocol specification, MCP tool-name reversibility).

- **An update can be checked, not only applied.** `ncui update --check --from <url>` fetches
the release document and reports whether a newer version exists — no bundle is downloaded
and nothing changes. The comparison is semantic-version based (the workspace's own parser);
versions that do not parse are reported as not comparable, with the document attached so the
application can apply its own rule. Behind the command is `ncui_cli::updater`: a `check` that
fetches and compares, and a `watch` that runs the check immediately and then on an interval
on its own thread, stoppable, delivering every answer — including failures — to the caller.
What is still absent is anything automatic inside the runtime: an application that wants a
schedule runs this itself, which is the line the gaps document already drew.

Tested: newer/same/older/not-comparable comparisons, wrong-application refusal, malformed
documents, and a watcher that checks immediately, repeats on its interval, and stops — all
against a real HTTP server, with the fetch the platform's own `curl`.

- **A sidecar can stay running.** A manifest sidecar may declare `mode: "stream"`: the
program starts once, on the first call, and answers one newline-delimited JSON line per call
instead of one document per process. The wire is `{"id": N, "payload": …}` in and
`{"id": N, "result": …}` or `{"id": N, "error": {"message": …}}` out. The program keeps its
state across calls — a counting sidecar answers 1, 2, 3 — and keeps the one-shot runner's
containment: root-resolved program, scrubbed environment, pinned working directory, process
group on Unix, job on Windows, per-call timeout, and a stop that fails everything waiting.

Tested against a real interpreter: state across calls, error answers that do not kill the
program, timeouts, a stopped sidecar reporting `not_running`, a program that is not there,
and a broken line that is dropped rather than misrouted.

- **A timed-out program's tree is a job, not a walk.** On Windows the runner and the
streaming sidecar put the child into a job object with kill-on-close (ADR-019, through the
`win32job` safe wrapper — ADR-012's ninth exception), so closing the job ends the child and
everything it started, including a grandchild whose parent has already exited — the case
`taskkill /T`'s parent-chain walk cannot reach. The Unix process-group path is unchanged,
and the walk remains as the fallback if the job is refused.

Tested: the existing timeout test still passes, and a new one lets the child exit while a
detached grandchild keeps ticking — the run returns, and the grandchild's heartbeat stops.

- **A colour can be picked.** `dialog.pick_color` shows the platform's own colour picker —
the .NET `ColorDialog` every Windows machine has, reached through PowerShell, the same
platform-tool shape the clipboard uses, because `rfd` has no colour picker. The request
carries an optional title and `#rrggbb` seed, validated headlessly whether or not the build
can show a picker; the answer is the chosen `#rrggbb`, a cancellation, or a named failure.

Tested: request validation, script generation, answer parsing (colour, bare hex,
cancellation, garbage), the headless registration, and the refusal a build without a backend
reports.

- **Files can be associated with an application.** A manifest may declare `associations` —
the file extensions that belong to it — and the installer registers them: a ProgID under
`HKCU\Software\Classes` whose command runs the installed program with the file, and the
extension key pointing at it, written with `reg.exe`; a desktop entry and `xdg-mime` on
Linux. The record an install writes is what uninstall removes, so removal stays exact, and
an update releases the associations the new bundle no longer names. The loader extensions
(`exe`, `dll`, `bat`, …) are refused by name.

Tested: extension validation and the reserved set, ProgID naming, and a real
write-read-remove cycle in the user's hive, beside the scheme registration's own test.

- **A browser's bookmarks can come along.** `ncui import-chrome` reads the Chrome profile's
own bookmark document (`--profile` names another) and prints the flattened list — title, URL,
folder — as JSON. It is read-only and bounded: passwords and cookies are not read, a file
past 10,000 entries is refused, and control characters are stripped from titles.

Tested: flattening across the three roots, folders in the answer, control-character
stripping, missing files, malformed documents, and the bound.

- **The machine's editors can be discovered and used.** `app.editors` reports the known
editors found on the platform's search path (`code`, `subl`, `nvim`, …), and
`app.open_in_editor` opens a file with the first of them — or one named. Both need
`process.spawn`, which is exactly the authority they use; the lookup takes its search paths
as a parameter, so the discovery is tested against directories a test owns.

Tested: search-path parsing, discovery over fixture directories, first-path-wins, and empty
answers.

- **Telemetry can be encoded for an OpenTelemetry collector.** `ncui-observability::otlp`
turns the crate's log records and metric series into the OTLP protobuf wire format — logs
with time, severity and body; counters as cumulative monotonic sums, gauges, and histograms
carrying their sample count. The encoding is hand-written for the small fixed subset of
protobuf this uses, and the HTTP posting stays with the caller, exactly like the JSON Lines
exporter. Histogram percentiles are not encoded, because an OTLP histogram data point has no
place for them; the data point carries the sample count.

Tested: a test-side protobuf decoder walks the encoded requests — log time/severity/body,
counter sums with their temporality and monotonicity, gauge doubles, and histogram counts.

Tested (full gate, this workspace, Windows host):
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo fmt --all` — applied; no functional change.
- `cargo test --workspace` — 74 test binaries, every one `test result: ok`, 0 failed.
  Includes the new suites: crash reports (unit + end-to-end panic hook), print/screenshot
  CDP parameter validation, the update checker and its watcher over real HTTP, the
  streaming NDJSON sidecar against a real Python interpreter, the Windows job-object
  containment test (a detached grandchild ends with the run), file-association
  write-read-remove in the real user hive, Chrome bookmark import (flatten, bounds,
  malformed input), editor discovery over fixture paths, the colour dialog's headless
  validation and registration, and the OTLP protobuf encoding checked by a test-side
  decoder.

Honest absences (unchanged, recorded in docs/known-gaps.md):
- Native menus: subsequently delivered on Windows and Linux through muda; ADR-012's ninth
  exception confines the one first-party unsafe block to Windows attachment, and a duplicate
  command is refused before the attached menu is disturbed.
- Mobile: still absent — there is no iOS/Android host for this runtime to run in.
- The updater checks when asked (`ncui update --check`); an automatic schedule inside the
  runtime remains out of scope by design.
