//! Latency benchmarks for the NC-UI hot paths.
//!
//! Run with `cargo bench -p ncui-bench --bench latency`. Results are written to
//! `benches/results/latency.json` together with the environment they were measured in, so
//! a later run can be compared rather than merely asserted to be fine.
//!
//! Every scenario is a real workload on real data. Spec 75 forbids using artificial
//! microbenchmarks as evidence of general performance, so each one states in its
//! `workload` field what it actually models.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ncui_bench::{Environment, NoiseEntry, Repeated, Report, Summary, measure};
use ncui_core::{Capability, CapabilitySet, CorrelationId, RequestId, SemVer};
use ncui_ipc::channel::BackpressurePolicy;
use ncui_ipc::frame::FrameKind;
use ncui_ipc::message::{Message, encode_messages};
use ncui_observability::logring::LogLevel;
use ncui_scheduler::{Priority, Task, Work};
use ncui_security::path::SafePath;
use ncui_state::patch::{Change, Patch};
use ncui_state::store::Store;

const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// How much a scenario's measured p99 may grow before the comparison calls it a
/// difference worth reporting.
///
/// A **fraction**: `4.0` permits a 500% increase, i.e. a 5.0x window. The name says
/// "fraction" because an earlier version of this constant was `1.5` in a fraction
/// parameter, which permitted 250% while three documents described it as "150%".
///
/// The value comes from the measured spread, not from taste. The harness prints the table
/// on every run and the committed baseline records it under `noise`:
///
/// ```text
/// cargo bench -p ncui-bench --bench latency
/// ```
///
/// The value is set from measurement, and the measurement is printed on every run rather
/// than written here — because a figure transcribed into a document stops describing the
/// artifact it came from, which is a defect this gate has already committed twice. The
/// recorded baseline's own `noise` array is the current source.
///
/// Two different statistics are involved and conflating them is what produced both errors.
/// `spread_ratio` in that array is *within* a run: worst repeat over median. The gate
/// compares *floor to floor* *across* runs, and that drift is far smaller, because
/// interference only ever makes a run slower and the minimum absorbs it. Three runs of
/// unchanged code gave a worst floor-to-floor drift of 2.23x, so a 5.0x window leaves
/// roughly 2x of headroom.
///
/// A gate that fires on scheduling jitter is worse than no gate, because it teaches people
/// to re-run until green. Retighten this when a baseline exists on a dedicated, quiet
/// runner — the number to change is here, and the measurement to justify it is printed on
/// every run rather than written in a document.
const ALLOWED_REGRESSION: f64 = 4.0;

const REPORTED_ONLY_FLOOR_NS: u64 = 1_000;

fn main() {
    // `--print-baseline` is answered before anything is measured. CI asks the tool for the
    // path so it does not recompute it in shell, and paying 38 seconds of benchmarking to
    // print a string made the CI check look like an oversight.
    if std::env::args().any(|arg| arg == "--print-baseline") {
        print_baseline_path();
        return;
    }

    let mut repeated: Vec<Repeated> = Vec::new();
    record(&mut repeated);
    // Two views of the same measurements: the median is what a reader sees, the floor is
    // what the gate compares. Deriving both from one pass keeps them from drifting.
    let scenarios: Vec<Summary> = repeated.iter().map(|r| r.summary.clone()).collect();
    let floors: Vec<Summary> = repeated.iter().map(|r| r.floor.clone()).collect();
    // The spread between repeats is measured, not transcribed. Three documents previously
    // carried this table by hand and disagreed with each other; now it is an output.
    let noise = repeated
        .iter()
        .map(|r| NoiseEntry {
            name: r.name.clone(),
            runs: r.repeats.len(),
            median_p99_ns: r.summary.p99_ns,
            floor_p99_ns: r.floor.p99_ns,
            max_p99_ns: r.repeats.iter().map(|x| x.p99_ns).max().unwrap_or(0),
            spread_ratio: r.noise_ratio(),
        })
        .collect();

    let report = Report {
        schema_version: 1,
        environment: Environment::capture(),
        scenarios,
        floors,
        noise,
    };

    let json = report
        .to_json()
        .unwrap_or_else(|e| fail(&format!("serialisation failed: {e}")));

    // Two destinations, deliberately distinct. `benches/results/` is regenerated output
    // and is gitignored; `benches/baseline/` is the committed artefact the gate compares
    // against, and only `--write-baseline` may put anything there.
    let results = output_path();
    write_json(&results, &json);

    // The action comes from the flag, never from the shape of the path. An earlier version
    // chose between writing and comparing by asking whether the path contained "baseline",
    // which meant `--baseline benches/baseline/linux/latency.json` — the exact invocation
    // CI uses — silently overwrote the committed baseline and compared nothing.
    match baseline_action() {
        BaselineAction::Record => {
            let path = resolve_from_root(&baseline_relative());
            write_json(&path, &json);
            let stderr = std::io::stderr();
            let mut out = stderr.lock();
            let _ = writeln!(
                out,
                "baseline recorded at {} for {}/{}",
                path.display(),
                report.environment.os,
                report.environment.arch
            );
        }
        BaselineAction::Compare(path) => compare_against_baseline(&report, &path),
        BaselineAction::MeasureOnly => {}
    }

    print_report(&report);
    print_noise(&report);
}

/// Prints the measured repeat-to-repeat spread for every scenario.
///
/// This is the table that justifies `ALLOWED_REGRESSION`. Regenerate it with
/// `--noise-report` rather than copying the numbers, because a number copied by hand
/// stops describing the artifact it came from — which is exactly what happened here.
fn print_noise(report: &Report) {
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let _ = writeln!(
        out,
        "
measured repeat-to-repeat spread ({} repeats each, {}):",
        report.noise.first().map_or(0, |n| n.runs),
        report.environment.os
    );
    let _ = writeln!(out, "  {:<38} {:>10} {:>9}", "scenario", "spread", "runs");
    for entry in report
        .noise
        .iter()
        .filter(|e| e.spread_ratio > 1.0)
        .collect::<Vec<_>>()
    {
        let _ = writeln!(
            out,
            "  {:<38} {:>9.2}x {:>9}",
            entry.name, entry.spread_ratio, entry.runs
        );
    }
    if let Some(worst) = report.worst_noise() {
        // Interpolated, not typed. A hardcoded copy of this constant is exactly how the
        // previous revision came to print "3.0" while the code said 4.0.
        let _ = writeln!(
            out,
            "  worst: {} at {:.2}x within-run spread",
            worst.name, worst.spread_ratio
        );
        let _ = writeln!(
            out,
            "  gate tolerance: {ALLOWED_REGRESSION:.1} (a {:.1}x window on floor-to-floor drift between runs)",
            1.0 + ALLOWED_REGRESSION
        );
    }
}

/// Writes a report, creating the parent directory if it does not exist.
fn write_json(path: &Path, json: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, json)
        .unwrap_or_else(|e| fail(&format!("cannot write {}: {e}", path.display())));
}

/// The baseline to compare against, from `--baseline <path>`.
///
/// An unrecognised argument is a hard failure rather than being ignored. An earlier
/// version of this benchmark accepted arguments and dropped them, so a CI step passing a
/// flag it expected to work would have done nothing and said nothing — which is how a gate
/// becomes theatre without anyone noticing.
/// What the caller asked the harness to do with a baseline.
///
/// This exists as an enum rather than an `Option<PathBuf>` because the *action* is the
/// decision, and inferring it from a path is how the previous version ended up overwriting
/// the committed baseline on every CI run.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BaselineAction {
    /// No baseline argument was given: measure and report only.
    MeasureOnly,
    /// Write this platform's baseline.
    Record,
    /// Compare against this baseline.
    Compare(PathBuf),
}

fn baseline_action() -> BaselineAction {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Cargo's bench harness appends `--bench`, and `--enforce` is read separately by
    // `enforcement_requested`, so both are stripped before the arguments are interpreted.
    let flags: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| *arg != "--bench" && *arg != "--enforce")
        .collect();
    match flags.as_slice() {
        [] => BaselineAction::MeasureOnly,
        ["--write-baseline"] => BaselineAction::Record,
        ["--baseline", path] => BaselineAction::Compare(resolve_from_root(path)),
        ["--baseline"] => fail("`--baseline` needs a path"),
        [other, ..] => fail(&format!("unknown argument `{other}`")),
    }
}

/// Prints the baseline path for the current platform, for a caller that needs it.
///
/// CI used to recompute this in shell from `uname -s`, which yields `darwin` on macOS while
/// `std::env::consts::OS` yields `macos`. Asking the tool removes the second source of
/// truth entirely.
fn print_baseline_path() {
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let _ = writeln!(out, "{}", resolve_from_root(&baseline_relative()).display());
}

/// Whether a reported difference should fail the run.
///
/// Off by default. The comparison is always performed and always printed; `--enforce` only
/// decides whether a difference exits non-zero. The reason is measured rather than assumed:
/// see `ALLOWED_REGRESSION` and the spread table the harness prints on every run.
fn enforcement_requested() -> bool {
    std::env::args().any(|arg| arg == "--enforce")
}

/// Compares the run against a baseline and reports what moved.
fn compare_against_baseline(report: &Report, baseline_path: &Path) {
    let raw = std::fs::read_to_string(baseline_path).unwrap_or_else(|e| {
        fail(&format!(
            "cannot read baseline {}: {e}",
            baseline_path.display()
        ))
    });
    let baseline: Report =
        serde_json::from_str(&raw).unwrap_or_else(|e| fail(&format!("baseline is malformed: {e}")));

    // Scenarios below the resolution floor are reported but not gated, for the reason
    // documented on `REPORTED_ONLY_FLOOR_NS`.
    // The comparison uses each scenario's *fastest* repeat, not its median. Machine
    // interference can only ever make a run look slower, so the minimum is the statistic
    // that tracks the code; the median is reported separately because that is what a user
    // experiences.
    if !report.is_comparable_to(&baseline) {
        // Refusing beats guessing, and refusing *silently* would be the theatre this
        // harness exists to avoid. A p99 measured on one platform compared against a
        // baseline from another is a comparison of two different machines, not a
        // regression measurement. So this exits non-zero and says why: a platform is
        // expected to record its own baseline with `--write-baseline` on its first run.
        fail(&format!(
            "baseline at {} was recorded on {}/{} but this run is {}/{}; not comparable, so the regression gate did not run. Record one for this platform with `cargo bench -p ncui-bench --bench latency -- --write-baseline`.",
            baseline_path.display(),
            baseline.environment.os,
            baseline.environment.arch,
            report.environment.os,
            report.environment.arch
        ));
    }

    let mut current = report.clone();
    let mut previous = baseline.clone();
    let current_floors = std::mem::take(&mut current.floors);
    let mut baseline_floors = std::mem::take(&mut previous.floors);

    // Scenarios whose floor sits at the clock's resolution are reported but not gated: a
    // p99 of 100 ns is a statement about the timer, not about the operation.
    let mut skipped: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for floor in &current_floors {
        if floor.p99_ns <= REPORTED_ONLY_FLOOR_NS {
            skipped.push(floor.name.clone());
        } else {
            names.push(floor.name.clone());
        }
    }
    // Both sides compare floors, restricted to the same scenario set. Restricting one
    // side only would make the rest look as though they had disappeared from the suite —
    // which `regressions_vs_baseline` correctly reports, and which is exactly the bug an
    // earlier version of this code had.
    current.scenarios = current_floors
        .into_iter()
        .filter(|scenario| names.contains(&scenario.name))
        .collect();
    baseline_floors.retain(|scenario| names.contains(&scenario.name));
    previous.scenarios = baseline_floors;

    let regressions = current.regressions_vs_baseline(&previous, ALLOWED_REGRESSION);
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    if !skipped.is_empty() {
        let _ = writeln!(
            out,
            "reported but not gated (floor at or below the {} ns clock resolution): {}",
            REPORTED_ONLY_FLOOR_NS,
            skipped.join(", ")
        );
    }
    if regressions.is_empty() {
        let _ = writeln!(out, "no p99 change beyond the tolerated fraction");
        return;
    }
    for regression in &regressions {
        let _ = writeln!(
            out,
            "CANDIDATE REGRESSION: {} — {} (baseline {:?}, current {:?}, change {:?})",
            regression.name,
            regression.reason,
            regression.baseline_p99_ns,
            regression.current_p99_ns,
            regression.change
        );
    }
    if !enforcement_requested() {
        let _ = writeln!(
            out,
            "reporting only: {} scenario(s) differ beyond the tolerance. Pass `--enforce` to fail on this. See docs/known-gaps.md for why enforcement is off by default.",
            regressions.len()
        );
        return;
    }
    fail(&format!(
        "{} scenario(s) regressed beyond the tolerated fraction (--enforce)",
        regressions.len()
    ));
}

/// Records every scenario, grouped by the subsystem it measures.
fn record(scenarios: &mut Vec<Repeated>) {
    core(scenarios);
    ipc(scenarios);
    state(scenarios);
    virtualize(scenarios);
    scheduler(scenarios);
    security(scenarios);
    observability(scenarios);
    workers(scenarios);
    mcp(scenarios);
    webview(scenarios);
    page_commands(scenarios);
    application(scenarios);
    native(scenarios);
    a11y(scenarios);
    abi(scenarios);
    glyphs(scenarios);
}

/// Scenarios for the text path a GPU island draws its rows with.
///
/// The workload is a real line of type: the machine's own font, shaped and rasterised at a size a
/// row uses. A machine with no font this runtime can read has nothing to measure, so the scenarios
/// are left out rather than measured against nothing.
fn glyphs(scenarios: &mut Vec<Repeated>) {
    let Ok(font) = ncui_gpu_renderer::text::default_font() else {
        return;
    };
    let Ok(shaped) = font.shape("Hello, world", 16.0) else {
        return;
    };
    let Some(glyph) = shaped.glyphs.first().copied() else {
        return;
    };

    scenarios.push(
        measure(
            "text.shape_line",
            "shape a twelve-character line at 16 pixels with the machine's own font",
            200,
            5_000,
            || {
                font.shape("Hello, world", 16.0)
                    .map_or(0, |text| text.len())
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "text.rasterise_glyph",
            "rasterise one 16-pixel glyph outline into per-pixel coverage",
            200,
            5_000,
            || {
                usize::from(
                    font.glyph_coverage(glyph.glyph_id, 16.0)
                        .ok()
                        .flatten()
                        .is_some(),
                )
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    // The same workload with the glyphs already rasterised, which is what every frame after the
    // first pays: a page redraws the same text, and the coverage of a glyph does not change.
    let cache = ncui_gpu_renderer::atlas::GlyphCache::new(font.clone());
    scenarios.push(
        measure(
            "text.draw_line_cached",
            "shape a twelve-character line and draw it from the glyph cache, as every frame after the first is",
            200,
            2_000,
            || {
                let Ok(line) = font.shape("Hello, world", 16.0) else {
                    return 0;
                };
                line.glyphs
                    .iter()
                    .filter(|glyph| cache.coverage(glyph.glyph_id, 16.0).is_some())
                    .count()
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    // The island's own workload: every glyph of a line, shaped once and rasterised each.
    scenarios.push(
        measure(
            "text.draw_line",
            "shape and rasterise every glyph of a twelve-character line, as an island row does",
            200,
            2_000,
            || {
                let Ok(line) = font.shape("Hello, world", 16.0) else {
                    return 0;
                };
                line.glyphs
                    .iter()
                    .filter(|glyph| {
                        font.glyph_coverage(glyph.glyph_id, 16.0)
                            .ok()
                            .flatten()
                            .is_some()
                    })
                    .count()
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for `ncui-core`.
fn core(scenarios: &mut Vec<Repeated>) {
    scenarios.push(
        measure(
            "core.semver_parse",
            "parse a 1.2.3 semantic version",
            1_000,
            20_000,
            || SemVer::parse("1.2.3").map_or(0, |_| 1),
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    let all = CapabilitySet::all();
    scenarios.push(
        measure(
            "core.capability_subset",
            "is a full capability set a subset of another (the authorization predicate)",
            1_000,
            50_000,
            || usize::from(all.is_subset_of(all)),
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "core.capability_membership",
            "single membership test over a full capability set",
            1_000,
            50_000,
            || usize::from(all.contains(Capability::Gpu)),
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for `ncui-ipc`.
fn ipc(scenarios: &mut Vec<Repeated>) {
    let request = Message::Request {
        method: "ncui.component.inspect".to_owned(),
        params: br#"{"id":"abc"}"#.to_vec(),
        correlation: CorrelationId::new(),
        request_id: RequestId::new(),
        deadline_ms: 30_000,
    };
    scenarios.push(
        measure(
            "ipc.message_encode",
            "encode a request message to a binary frame",
            1_000,
            20_000,
            || request.encode(MAX_FRAME).map_or(0, |bytes| bytes.len()),
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    let encoded = request
        .encode(MAX_FRAME)
        .unwrap_or_else(|e| fail(&e.to_string()));
    scenarios.push(
        measure(
            "ipc.message_decode",
            "decode a binary frame payload back into a message",
            1_000,
            20_000,
            || {
                let frame = ncui_ipc::frame::decode_all(&encoded, MAX_FRAME)
                    .ok()
                    .and_then(|mut frames| frames.pop());
                match frame {
                    Some(frame) => usize::from(
                        Message::decode_payload(FrameKind::Request, frame.payload()).is_ok(),
                    ),
                    None => 0,
                }
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    let batch: Vec<Message> = (0..64)
        .map(|i| Message::Event {
            topic: "ncui.state.patch".to_owned(),
            payload: vec![i as u8; 256],
            correlation: CorrelationId::new(),
        })
        .collect();
    scenarios.push(
        measure(
            "ipc.batch_encode_64",
            "encode a 64-event batch, the shape a state fan-out actually produces",
            500,
            20_000,
            || encode_messages(&batch, MAX_FRAME).map_or(0, |bytes| bytes.len()),
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "ipc.channel_send_recv_roundtrip",
            "send and receive one item through a bounded channel",
            500,
            10_000,
            || match ncui_ipc::channel::BoundedChannel::named(
                "bench",
                64,
                BackpressurePolicy::Reject,
            ) {
                Ok(channel) => {
                    let _ = channel.try_send(1u64);
                    channel.try_recv().unwrap_or(0) as usize
                }
                Err(_) => 0,
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for `ncui-state`.
fn state(scenarios: &mut Vec<Repeated>) {
    scenarios.push(
        measure(
            "state.patch_coalesce_512",
            "coalesce a 512-change patch down to its distinct keys",
            500,
            20_000,
            || {
                let patch: Patch<u64> = (0..512)
                    .map(|i| Change::upsert(format!("window.{i}"), i))
                    .collect();
                patch.coalesced().len()
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "state.store_insert_128",
            "insert 128 keys into a revisioned store and take the pending patch",
            500,
            20_000,
            || {
                let mut store: Store<u64> = Store::new();
                for i in 0..128u64 {
                    store.insert(format!("k{i}"), i);
                }
                store.len()
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for `ncui-scheduler`.
fn scheduler(scenarios: &mut Vec<Repeated>) {
    let now = Duration::from_millis(10);
    let input = Task::new(
        Priority::Input,
        now,
        Work::Immediate {
            label: "bench".to_owned(),
        },
    )
    .priority_class(now);
    let background = Task::new(
        Priority::Background,
        now,
        Work::Immediate {
            label: "bench".to_owned(),
        },
    )
    .priority_class(now);
    scenarios.push(
        measure(
            "scheduler.priority_compare",
            "compare two aged priority classes, the hot path of a ready-queue pop",
            1_000,
            50_000,
            || usize::from(input.cmp_at(now, &background).is_gt()),
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "scheduler.run_budgeted_64",
            "schedule 64 mixed-priority tasks and drain them under a frame budget",
            200,
            1_000,
            || {
                let Ok(mut scheduler) = ncui_scheduler::Scheduler::new(128) else {
                    return 0;
                };
                for i in 0..64u64 {
                    let priority = if i % 8 == 0 {
                        Priority::Input
                    } else {
                        Priority::Background
                    };
                    let _ = scheduler.schedule(Task::new(
                        priority,
                        now,
                        Work::Immediate {
                            label: "bench".to_owned(),
                        },
                    ));
                }
                scheduler
                    .run_budgeted(now, Duration::from_millis(4))
                    .ran_count()
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for `ncui-security`.
fn security(scenarios: &mut Vec<Repeated>) {
    let root = std::env::temp_dir();
    scenarios.push(
        measure(
            "security.path_resolve",
            "resolve a legitimate nested path against its root",
            1_000,
            20_000,
            || usize::from(SafePath::resolve(&root, "src/core/engine.rs").is_ok()),
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    let token_a = vec![0x5Au8; 32];
    let token_b = vec![0x5Au8; 32];
    scenarios.push(
        measure(
            "security.constant_time_eq",
            "constant-time comparison of two 32-byte tokens",
            1_000,
            50_000,
            || usize::from(ncui_security::token::constant_time_eq(&token_a, &token_b)),
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "security.rate_limiter_acquire",
            "acquire one token from a 1000/s bucket on an injected clock",
            500,
            20_000,
            || match ncui_security::ratelimit::RateLimiter::new(1_000, 2_000) {
                Ok(mut limiter) => {
                    usize::from(limiter.try_acquire(Duration::from_secs(0), 1).is_ok())
                }
                Err(_) => 0,
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    // The mismatched-length branch is the timing-sensitive one: it is where an early-exit
    // comparison would leak how much of the token matched.
    scenarios.push(
        measure(
            "security.constant_time_eq_mismatched_length",
            "constant-time comparison where the two lengths differ",
            500,
            20_000,
            || {
                let short = vec![0x5Au8; 16];
                let long = vec![0x5Au8; 32];
                usize::from(ncui_security::token::constant_time_eq(&short, &long))
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for `ncui-observability`.
fn observability(scenarios: &mut Vec<Repeated>) {
    let labels = ncui_observability::metrics::Labels::new();
    scenarios.push(
        measure(
            "observability.counter_inc_10",
            "ten counter writes in the metrics registry, the path every metric takes",
            1_000,
            2_000,
            || match ncui_observability::metrics::MetricsRegistry::new(ncui_core::Limits::default())
            {
                Ok(registry) => {
                    for _ in 0..10 {
                        let _ = registry.counter_inc("bench_counter", &labels, 1);
                    }
                    registry.len()
                }
                Err(_) => 0,
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "observability.log_ring_push_64",
            "push 64 records into the bounded log ring",
            500,
            10_000,
            || {
                let Ok(ring) =
                    ncui_observability::logring::LogRing::new(ncui_core::Limits::default())
                else {
                    return 0;
                };
                for i in 0..64u64 {
                    ring.push(ncui_observability::logring::LogRecord::new(
                        i,
                        LogLevel::Info,
                        "bench",
                        "record",
                        "a benchmark log record",
                    ));
                }
                ring.len()
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "observability.histogram_percentiles_1000",
            "observe 1000 samples then read the full percentile summary",
            100,
            300,
            || {
                let Ok(registry) =
                    ncui_observability::metrics::MetricsRegistry::new(ncui_core::Limits::default())
                else {
                    return 0;
                };
                for i in 0..1000u64 {
                    let _ = registry.histogram_observe("bench_histogram", &labels, i as f64);
                }
                registry
                    .histogram_stats("bench_histogram", &labels)
                    .map_or(0, |stats| stats.p99 as usize)
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "observability.counter_inc_1000",
            "a thousand counter writes, the path every metric takes",
            200,
            1_000,
            || match ncui_observability::metrics::MetricsRegistry::new(ncui_core::Limits::default())
            {
                Ok(registry) => {
                    for _ in 0..1000 {
                        let _ = registry.counter_inc("bench_counter", &labels, 1);
                    }
                    registry.len()
                }
                Err(_) => 0,
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for virtualisation.
///
/// The point of these is the claim in spec 18: a million logical records must not cost a
/// million anything. The scenarios below measure the *window* computation against
/// collections three orders of magnitude apart, so the numbers show whether cost tracks
/// the collection or tracks the viewport.
fn virtualize(scenarios: &mut Vec<Repeated>) {
    use ncui_state::virtualize::{Viewport, WindowCalculator};

    let mut by_total: Vec<(usize, Repeated)> = Vec::new();
    for (name, total) in [
        ("state.virtualize_window_1k", 1_000usize),
        ("state.virtualize_window_1m", 1_000_000),
    ] {
        let measured = measure(
            name,
            &format!("compute the visible window over {total} uniformly-sized records"),
            200,
            500,
            || {
                let mut calc = WindowCalculator::with_total(24.0, Some(total));
                // A few hundred measured rows, the way real layout behaves.
                for index in (0..400).step_by(7) {
                    let _ = calc.measure(index, 24.0);
                }
                let viewport = Viewport::new(120_000.0, 800.0).with_overscan(4);
                let window = calc.visible_window(&viewport);
                (window.visible.len() + window.start_offset as usize) % 1_000
            },
        )
        .unwrap_or_else(|e| fail(&e));
        by_total.push((total, measured.clone()));
        scenarios.push(measured);
    }

    // The timing half of the spec-18 claim, checked against real measurements rather than
    // against constants. This reports rather than fails, matching the enforcement level
    // chosen everywhere else: a shared machine cannot fail a build on a timing verdict.
    // The deterministic half lives in the `ncui-state::virtualize` tests.
    let small = by_total
        .iter()
        .find(|(total, _)| *total == 1_000)
        .map(|(_, r)| r);
    let large = by_total
        .iter()
        .find(|(total, _)| *total == 1_000_000)
        .map(|(_, r)| r);
    if let (Some(small), Some(large)) = (small, large)
        && !ncui_bench::costs_within_tolerance(small.floor.p99_ns, large.floor.p99_ns)
    {
        let ratio = large.floor.p99_ns as f64 / small.floor.p99_ns.max(1) as f64;
        let stderr = std::io::stderr();
        let mut out = stderr.lock();
        let _ = writeln!(
            out,
            "CANDIDATE REGRESSION: a window over 1,000,000 records cost {ratio:.1}x a window over 1,000 ({}ns vs {}ns). The window computation is bounded by the measured rows, not by the collection.",
            large.floor.p99_ns, small.floor.p99_ns
        );
    }

    // A window that has measured everything it needs must not grow with the collection.
    scenarios.push(
        measure(
            "state.virtualize_fully_measured_1m",
            "compute a window over a million records whose visible rows are all measured",
            200,
            500,
            || {
                let mut calc = WindowCalculator::with_total(24.0, Some(1_000_000));
                for index in 0..64 {
                    let _ = calc.measure(index, 24.0);
                }
                let window = calc.visible_window(&Viewport::new(0.0, 800.0).with_overscan(2));
                window.visible.len()
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for `ncui-workers`.
fn workers(scenarios: &mut Vec<Repeated>) {
    // ---- ncui-workers ---------------------------------------------------
    scenarios.push(
        measure(
            "workers.submit_and_complete_32",
            "submit 32 tasks to a native worker pool and wait for them to finish",
            10,
            50,
            || {
                let Ok(registry) =
                    ncui_observability::metrics::MetricsRegistry::new(ncui_core::Limits::default())
                else {
                    return 0;
                };
                // `with_worker_count` sizes the queue at 8, so 32 submissions under
                // `Reject` would refuse most of them and the wait below would spin out its
                // deadline. The queue is sized for the workload so the scenario measures
                // execution rather than backpressure.
                let Ok(limits) = ncui_core::LimitsBuilder::new()
                    .max_workers(4)
                    .max_worker_queue_depth(64)
                    .build()
                else {
                    return 0;
                };
                let Ok(pool) = ncui_workers::WorkerPool::new(limits, std::sync::Arc::new(registry))
                else {
                    return 0;
                };
                for _ in 0..32 {
                    let _ = pool.submit(
                        ncui_workers::WorkerTask::new("bench", 0, || {
                            ncui_workers::TaskOutcome::Completed { bytes: 0 }
                        }),
                        ncui_workers::BackpressurePolicy::Reject,
                    );
                }
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while pool.task_counters().0 < 32 && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_micros(50));
                }
                let (completed, _, _) = pool.task_counters();
                pool.shutdown();
                completed as usize
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for the MCP adapter.
///
/// Two separate scenarios rather than one, because they answer different questions and
/// mixing them would hide both. `mcp.translate` is the adapter's own cost — name mapping
/// and request construction, with nothing else running. `mcp.tool_call_roundtrip` is the
/// whole path an MCP client pays: translation plus the dispatch pipeline, which is the same
/// pipeline the JSON-RPC surface uses, so the number includes the capability check, the
/// consent gate and the audit write rather than being the adapter alone.
fn mcp(scenarios: &mut Vec<Repeated>) {
    let methods = ncui_runtime::dispatch::method_table();
    let schema = ncui_runtime::mcp::input_schema();

    scenarios.push(
        measure(
            "mcp.tool_table",
            "build the MCP tool list from the JSON-RPC method table",
            1_000,
            20_000,
            || ncui_runtime::mcp::tool_table(&methods).len(),
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "mcp.translate",
            "map a JSON-RPC method to its MCP tool name and back",
            1_000,
            50_000,
            || {
                let tool = ncui_runtime::mcp::tool_name_for("ncui.logs.purge");
                let name = ncui_runtime::mcp::method_for_tool(&tool)
                    .unwrap_or_else(|| String::from("none"));
                name.len()
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    // The fixture — runtime, session and method table — is built once, outside the measured
    // closure. Building it per iteration would measure `Runtime::start` rather than the
    // dispatch pipeline, which is the mistake several other scenarios in this file made.
    let runtime = ncui_runtime::Runtime::start(ncui_core::Limits::default())
        .unwrap_or_else(|e| fail(&format!("bench runtime: {e}")));
    let mut runtime = runtime;
    let mut session = ncui_runtime::agent::AgentSession::new(
        "bench-mcp",
        ncui_security::authz::AgentRole::Administrator,
        1_000,
        1_000,
    )
    .unwrap_or_else(|e| fail(&format!("bench session: {e}")));
    let token = ncui_security::token::SecretToken::new(vec![0x33; 32])
        .unwrap_or_else(|e| fail(&format!("{e}")));
    let _ = session.authenticate(&token, &token);
    let args = serde_json::json!({});
    let _ = &schema;

    scenarios.push(
        measure(
            "mcp.tool_call_roundtrip",
            "translate an MCP tool call and dispatch it through the full agent pipeline",
            200,
            5_000,
            || {
                ncui_runtime::mcp::call_tool(
                    &mut runtime,
                    &mut session,
                    "ncui_health",
                    args.clone(),
                    Duration::from_millis(0),
                    None,
                    &methods,
                )
                .as_object()
                .map_or(0, serde_json::Map::len)
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for the `WebView` host policy.
///
/// Two scenarios, because the two halves of the seam cost different things and averaging them
/// would hide both. `webview.navigation_evaluate` is the per-navigation decision, which runs on
/// every link the user clicks. `webview.seam_evaluate` is the whole message path, capability
/// check included, which runs on every action a page takes.
///
/// The fixtures — the allow-list and the grant — are built once, outside the measured closure.
/// Building them per iteration would measure `Vec::new`, which is not what these scenarios
/// are named for.
fn webview(scenarios: &mut Vec<Repeated>) {
    use ncui_webview::capability::CapabilityRequest;
    use ncui_webview::lifecycle::{HostState, WindowLifecycle};
    use ncui_webview::navigation::{NavigationPolicy, NavigationRequest, Origin, Scheme};
    use ncui_webview::seam::{HostSeam, SeamMessage};

    let mut origins = Vec::new();
    for index in 0..32 {
        origins.push(Origin::new(
            Scheme::Https,
            &format!("app{index}.example.com"),
            None,
        ));
    }
    let policy = NavigationPolicy::new(&origins);

    let lifecycle = WindowLifecycle::new();
    lifecycle
        .transition(HostState::Created)
        .unwrap_or_else(|e| fail(&format!("{e}")));
    lifecycle
        .transition(HostState::Attaching)
        .unwrap_or_else(|e| fail(&format!("{e}")));
    lifecycle
        .transition(HostState::Running)
        .unwrap_or_else(|e| fail(&format!("{e}")));

    // The probe target alternates between an allowed and a refused origin, so the measurement
    // covers both the permissive and the restrictive branch rather than the cheapest one.
    let allowed = NavigationRequest {
        target: origins[3].clone(),
        top_level: true,
    };
    let refused = NavigationRequest {
        target: Origin::new(Scheme::Https, "not-allowed.example.net", None),
        top_level: true,
    };

    scenarios.push(
        measure(
            "webview.navigation_evaluate",
            "decide one top-level navigation against a 32-origin allow-list",
            1_000,
            50_000,
            || {
                let a = usize::from(policy.evaluate(&allowed).is_allowed());
                let r = usize::from(policy.evaluate(&refused).is_allowed());
                a + r
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    // The seam needs a mutable host because `handle` takes `&self` but the dispatcher it calls
    // is `FnMut`-free by design; the fixture is therefore rebuilt outside the loop and the
    // measured work is the decision path only.
    let seam = HostSeam::new(
        lifecycle,
        ncui_webview::CapabilityGrant::of(&[Capability::InspectRuntime]),
        policy,
    )
    .with_action(
        "describe_metrics",
        CapabilitySet::empty().with(Capability::InspectRuntime),
    );

    scenarios.push(
        measure(
            "webview.seam_evaluate",
            "handle one page message through the capability check and the action table",
            500,
            20_000,
            || {
                let request = CapabilityRequest {
                    action: "describe_metrics".to_owned(),
                    requires: CapabilitySet::empty().with(Capability::InspectRuntime),
                };
                let _ = &request;
                let response = seam.handle(
                    &SeamMessage::Perform {
                        name: "describe_metrics".to_owned(),
                        payload: serde_json::json!({ "probe": 1 }),
                    },
                    &mut |_, _| Ok(serde_json::Value::Null),
                );
                usize::from(response.ok)
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    // The wire cost of a page call: the request as the bridge posts it, parsed and
    // validated by the host. This is paid on every call a page makes, before any
    // capability check happens, so it is the floor of the page path.
    let wire =
        r#"{"id":"p1-abc","action":"perform","name":"describe_metrics","payload":{"deep":true}}"#;
    scenarios.push(
        measure(
            "webview.bridge_request_parse",
            "parse and validate one bridge request from the page",
            500,
            20_000,
            || {
                let request = ncui_webview::SeamRequest::parse(wire)
                    .unwrap_or_else(|_| fail("the benchmark fixture must be a valid request"));
                request.id.len()
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for the application command path.
///
/// An application command is the call a page makes into the application's own code. The
/// measured work is the whole path: the page's request is parsed, the capability is
/// checked against the window's grant, and the handler runs.
fn page_commands(scenarios: &mut Vec<Repeated>) {
    use ncui_webview::capability::CapabilityRequest;
    use ncui_webview::lifecycle::{HostState, WindowLifecycle};
    use ncui_webview::navigation::NavigationPolicy;
    use ncui_webview::seam::{HostSeam, SeamMessage};

    let lifecycle = WindowLifecycle::new();
    lifecycle
        .transition(HostState::Created)
        .unwrap_or_else(|e| fail(&format!("{e}")));
    lifecycle
        .transition(HostState::Attaching)
        .unwrap_or_else(|e| fail(&format!("{e}")));
    lifecycle
        .transition(HostState::Running)
        .unwrap_or_else(|e| fail(&format!("{e}")));

    let seam = HostSeam::new(
        lifecycle,
        ncui_webview::CapabilityGrant::of(&[Capability::InspectRuntime]),
        NavigationPolicy::new(&[]),
    )
    .with_action(
        "app_command",
        CapabilitySet::empty().with(Capability::InspectRuntime),
    );

    scenarios.push(
        measure(
            "runtime.page_command_dispatch",
            "handle one application command through the capability check and the handler",
            500,
            20_000,
            || {
                let response = seam.handle(
                    &SeamMessage::Perform {
                        name: "app_command".to_owned(),
                        payload: serde_json::json!({ "n": 1 }),
                    },
                    &mut |request: &CapabilityRequest, payload| {
                        let _ = request;
                        Ok(serde_json::json!({ "echoed": payload }))
                    },
                );
                usize::from(response.ok)
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for the application layer.
///
/// Both measure something on the path a user waits on. `app.manifest_parse` is the cost of
/// reading an application's manifest, which every start pays once. `app.asset_resolve` is
/// the cost of turning a request path into a file inside the root, which is paid on every
/// asset the page asks for — the hot path of serving an application.
///
/// The manifest is parsed from real text rather than a pre-built value, because parsing is
/// what the scenario is named for. The path resolution uses the same containment check the
/// server uses, so the measurement includes the check the security argument rests on.
fn application(scenarios: &mut Vec<Repeated>) {
    use ncui_app::AppManifest;
    use ncui_security::path::SafePath;
    use std::path::Path;

    const MANIFEST: &str = r#"{
        "name": "benchmark",
        "version": "1.0.0",
        "entry": "index.html",
        "assets": "public",
        "window": { "title": "Benchmark", "width": 900, "height": 640 },
        "capabilities": ["read.metrics", "read.logs"]
    }"#;

    scenarios.push(
        measure(
            "app.manifest_parse",
            "read and validate an application's manifest",
            500,
            20_000,
            || {
                let manifest = AppManifest::parse(MANIFEST, Path::new("ncui.app.json"))
                    .unwrap_or_else(|e| fail(&e.to_string()));
                manifest.name.len()
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    // The reload loop polls the assets directory on every pump. The tree here is the shape
    // of a small application: an entry page, a script, a stylesheet and a directory of
    // pages, written once and measured on every poll.
    let watch_root = std::env::temp_dir().join(format!("ncui-bench-watch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&watch_root);
    if std::fs::create_dir_all(watch_root.join("pages")).is_err() {
        fail("the watch benchmark's directory could not be created");
    }
    for index in 0..200 {
        if std::fs::write(
            watch_root.join("pages").join(format!("page-{index}.html")),
            "x",
        )
        .is_err()
        {
            fail("the watch benchmark's fixture could not be written");
        }
    }
    let mut watcher = ncui_app::DirectoryWatcher::new(&watch_root)
        .unwrap_or_else(|error| fail(&format!("the watch benchmark could not start: {error}")));
    scenarios.push(
        measure(
            "app.watch_poll",
            "scan an application's files for a change",
            50,
            500,
            || {
                let changed = watcher
                    .changed()
                    .unwrap_or_else(|error| fail(&error.to_string()));
                usize::from(changed)
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
    let _ = std::fs::remove_dir_all(&watch_root);

    let root = Path::new("/srv/app/public");
    scenarios.push(
        measure(
            "app.asset_resolve",
            "resolve a request path to a file inside the served root",
            500,
            20_000,
            || {
                let resolved = SafePath::resolve(root, "assets/app.js")
                    .unwrap_or_else(|e| fail(&e.to_string()));
                resolved.components().count()
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for `ncui-native`.
///
/// A page's filesystem call pays for the containment check and a real read. Both are
/// measured against a file that exists, because the interesting cost is serving a call
/// that succeeds — a refusal is cheaper and would understate the path a real application
/// takes.
fn native(scenarios: &mut Vec<Repeated>) {
    use ncui_native::{FileSystem, scrubbed_from};

    let root = std::env::temp_dir().join(format!("ncui-bench-native-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    if std::fs::create_dir_all(&root).is_err() {
        fail("the benchmark's filesystem root could not be created");
    }
    if std::fs::write(root.join("note.txt"), "a note the page reads").is_err() {
        fail("the benchmark's fixture could not be written");
    }
    let filesystem = FileSystem::new(&root).unwrap_or_else(|error| {
        fail(&format!(
            "the benchmark's filesystem root is unusable: {error}"
        ))
    });

    scenarios.push(
        measure(
            "native.fs_read_text",
            "read a file through the containment check a page's call pays",
            50,
            1_000,
            || {
                let content = filesystem
                    .read_text("note.txt")
                    .unwrap_or_else(|error| fail(&error.to_string()));
                content.text.len()
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    // The scrub runs once per spawn, over the whole ambient environment. Measuring it on a
    // synthetic environment of the size a real machine has keeps the result meaningful on
    // a CI runner with a small one.
    let ambient: Vec<(String, String)> = (0..200)
        .map(|index| {
            let name = if index % 7 == 0 {
                format!("SERVICE_SECRET_{index}")
            } else {
                format!("ORDINARY_VARIABLE_{index}")
            };
            (name, format!("value-{index}"))
        })
        .collect();
    scenarios.push(
        measure(
            "native.environment_scrub",
            "filter a child's environment for secret-shaped names",
            500,
            20_000,
            || scrubbed_from(ambient.clone()).len(),
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    // The clipboard is read and written by starting the platform's own tool, so its cost is
    // a process start plus a pipe. That is what this measures — with a program that exits
    // immediately, so the figure is the framework's overhead rather than the clipboard
    // tool's own work.
    let runner = ncui_native::ProcessRunner::new(&root)
        .unwrap_or_else(|error| fail(&format!("the process runner could not start: {error}")));
    let (tool, arguments): (&str, Vec<String>) = if cfg!(windows) {
        ("cmd", vec!["/C".to_owned(), "exit".to_owned()])
    } else {
        ("sh", vec!["-c".to_owned(), "exit".to_owned()])
    };
    scenarios.push(instance_handoff());
    scenarios.push(window_state_round_trip());
    scenarios.push(window_places_round_trip());
    scenarios.push(window_presentation_parse());
    scenarios.push(window_shape_validate());
    scenarios.push(page_state_round_trip());

    scenarios.push(
        measure(
            "native.tool_spawn_overhead",
            "start a program and collect its output, the cost a clipboard call pays",
            2,
            20,
            || {
                let output = runner
                    .run(&ncui_native::RunRequest {
                        program: tool.to_owned(),
                        args: arguments.clone(),
                        stdin: None,
                        timeout_ms: Some(5_000),
                    })
                    .unwrap_or_else(|error| fail(&error.to_string()));
                usize::try_from(output.status).unwrap_or(0)
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// What a page's preference costs: one read at launch, and one write per save.
///
/// A page saves on its own schedule — a theme change, a column drag, a document open — so this
/// is the figure that decides whether saving on every keystroke is affordable or whether a page
/// should coalesce.
fn page_state_round_trip() -> Repeated {
    let root = std::env::temp_dir().join("ncui-bench-page-state");
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("state.json");
    let mut state = ncui_runtime::page_state::PageState::open(&path);
    let scenario = measure(
        "app.state_round_trip",
        "store a page's preference and read it back, the cost of remembering one",
        2,
        500,
        || {
            state
                .save("theme", serde_json::json!("dark"))
                .unwrap_or_else(|error| fail(&error.to_string()));
            let loaded = state.load("theme");
            usize::from(loaded["found"].as_bool().unwrap_or(false))
        },
    )
    .unwrap_or_else(|e| fail(&e));
    let _ = std::fs::remove_dir_all(&root);
    scenario
}

/// What remembering a window costs: one file read at launch and one write at shutdown.
///
/// The platform's own geometry comes back over the host's channel, which needs a window and is
/// therefore not measurable here; the file either side of it is, and it is the part that runs on
/// every launch whether or not the window moved.
fn window_state_round_trip() -> Repeated {
    let root = std::env::temp_dir().join("ncui-bench-window-state");
    let _ = std::fs::create_dir_all(&root);
    let path = root.join("window.json");
    let geometry = ncui_webview::WindowGeometry {
        x: 120,
        y: 90,
        width: 1280,
        height: 800,
    };
    let encoded = serde_json::to_vec_pretty(&geometry)
        .unwrap_or_else(|error| fail(&format!("the geometry could not be encoded: {error}")));
    let scenario = measure(
        "app.window_state_round_trip",
        "write a window's geometry and read it back, the cost of remembering a window",
        2,
        2000,
        || {
            std::fs::write(&path, &encoded).unwrap_or_else(|error| fail(&error.to_string()));
            let read = std::fs::read(&path).unwrap_or_else(|error| fail(&error.to_string()));
            read.len()
        },
    )
    .unwrap_or_else(|e| fail(&e));
    let _ = std::fs::remove_dir_all(&root);
    scenario
}

/// What a named window costs at launch: one read of the file holding every place, and one lookup
/// by the name the page uses.
///
/// A page that opens a window under a name pays the lookup on every open; the file is read once,
/// when the runtime is told where places live. Both are here, because a place re-read from disk on
/// every open would be a disk access nobody asked for, and this is the figure that would show it.
fn window_places_round_trip() -> Repeated {
    let root = std::env::temp_dir().join("ncui-bench-window-places");
    let _ = std::fs::create_dir_all(&root);
    let path = root.join("windows.json");
    let places: std::collections::BTreeMap<String, ncui_webview::WindowGeometry> = (0..16u32)
        .map(|index| {
            (
                format!("window-{index}"),
                ncui_webview::WindowGeometry {
                    x: i32::try_from(index * 40).unwrap_or(0),
                    y: i32::try_from(index * 30).unwrap_or(0),
                    width: 1280,
                    height: 800,
                },
            )
        })
        .collect();
    let encoded = serde_json::to_vec_pretty(&places)
        .unwrap_or_else(|error| fail(&format!("the places could not be encoded: {error}")));
    std::fs::write(&path, &encoded).unwrap_or_else(|error| fail(&error.to_string()));
    let windows = ncui_runtime::window::Windows::new();
    let scenario = measure(
        "app.window_places_round_trip",
        "read the file of remembered window places and look one up, the cost a page pays at launch",
        2,
        500,
        || {
            windows.remember_windows_in(&path);
            usize::from(windows.place_for("window-7").is_some())
        },
    )
    .unwrap_or_else(|e| fail(&e));
    let _ = std::fs::remove_dir_all(&root);
    scenario
}

/// What a presentation request costs before the platform is asked: the rule that reads a change off
/// a payload.
///
/// The round trip to the engine's event loop is what dominates a real call, and it needs a window,
/// so it is not measurable here. This is the part that runs on every call and that a hostile caller
/// controls the input to, which is the part worth having a figure for: it is pure, and it either
/// accepts a change or names the field that is wrong.
fn window_presentation_parse() -> Repeated {
    let payloads: Vec<serde_json::Value> = [
        "{}",
        r#"{"fullscreen":true}"#,
        r#"{"minimized":true}"#,
        r#"{"visible":false}"#,
        r#"{"focus":true}"#,
        r#"{"fullscreen":true,"maximized":true}"#,
    ]
    .into_iter()
    .map(|json| serde_json::from_str(json).unwrap_or(serde_json::Value::Null))
    .collect();
    measure(
        "webview.window_presentation_parse",
        "read one presentation change off a caller's payload, the validation every call pays",
        2,
        5000,
        || {
            payloads
                .iter()
                .filter(|payload| ncui_webview::WindowChange::from_payload(payload).is_ok())
                .count()
        },
    )
    .unwrap_or_else(|e| fail(&e))
}
/// What a reshape request costs before the platform is asked: the rule that says a shape holds
/// together.
///
/// The same reasoning the presentation parse follows: the round trip to the engine's event loop
/// dominates a real call and needs a window, so what is measured here is the part that runs on every
/// call and whose input a hostile caller controls. A shape is a set of optional fields, and the
/// expensive mistake it guards against is a caller naming half of a position or half of a size.
fn window_shape_validate() -> Repeated {
    let shapes: Vec<ncui_webview::WindowShape> = [
        r#"{"title":"notes"}"#,
        r#"{"x":10,"y":20}"#,
        r#"{"width":900,"height":700}"#,
        r#"{"x":10}"#,
        r#"{"always_on_top":true,"resizable":false}"#,
    ]
    .into_iter()
    .filter_map(|json| serde_json::from_str(json).ok())
    .collect();
    measure(
        "webview.window_shape_validate",
        "check one window shape, the validation every reshape pays",
        2,
        5000,
        || {
            shapes
                .iter()
                .filter(|shape| shape.validate().is_ok())
                .count()
        },
    )
    .unwrap_or_else(|e| fail(&e))
}

/// What a second launch costs the instance that is already running.
///
/// The lock is read, the port is connected to, the arguments cross a loopback socket, and the
/// instance wakes up and answers. It is the figure that decides whether handing a link to the
/// running window is cheaper than starting a second process — which is the choice the
/// single-instance capability exists to make.
fn instance_handoff() -> Repeated {
    let root = std::env::temp_dir().join("ncui-bench-instance");
    let _ = std::fs::create_dir_all(&root);
    let state = root.join("instance-bench");
    let _ = std::fs::remove_dir_all(&state);
    let guard = match ncui_native::InstanceGuard::acquire(&state, "bench") {
        Ok(ncui_native::Acquired::First(guard)) => *guard,
        Ok(ncui_native::Acquired::AlreadyRunning { port, .. }) => fail(&format!(
            "a fresh state directory named a running instance on {port}"
        )),
        Err(error) => fail(&format!("the instance guard could not be taken: {error}")),
    };
    let port = guard
        .port()
        .unwrap_or_else(|error| fail(&error.to_string()));
    let token = guard.token().to_owned();
    let scenario = measure(
        "app.instance_handoff",
        "hand a second launch's arguments to the running instance and receive them",
        2,
        200,
        || {
            let arguments = vec!["--open".to_owned(), "notes.txt".to_owned()];
            let sender = std::thread::spawn({
                let token = token.clone();
                move || ncui_native::hand_over(port, &token, &arguments)
            });
            let received = guard
                .receive()
                .unwrap_or_else(|error| fail(&error.to_string()));
            sender
                .join()
                .unwrap_or_else(|_| fail("the hand-off sender panicked"))
                .unwrap_or_else(|error| fail(&error.to_string()));
            received.len()
        },
    )
    .unwrap_or_else(|e| fail(&e));
    let _ = std::fs::remove_dir_all(&state);
    scenario
}

/// Scenarios for `ncui-a11y`.
///
/// Both measure something a user waits on, which is why they are benchmarked at all:
/// `a11y.focus_tab` is the cost of a single keypress, and it is paid again on every keypress
/// for the life of the session, so a per-step cost that doubles is felt. `a11y.announce`
/// is the cost of rendering a tree into the text a screen reader reads, which is rebuilt on
/// every state change rather than cached in a real reader.
///
/// The trees are built once, outside the measured closure — building a tree per iteration
/// would measure `BTreeMap` insertion, which is not what these are named for.
fn a11y(scenarios: &mut Vec<Repeated>) {
    use ncui_a11y::focus::{FocusOwner, FocusState, Key};
    use ncui_a11y::tree::{Node, Role, SemanticTree};

    // A tree with the shape a real window has: a toolbar, a list of rows, and a status bar.
    fn build_tree(rows: usize) -> SemanticTree {
        let mut window = Node::with_children(Role::Window, "Editor", 0, false);
        let mut order = 1;
        for name in ["New", "Save", "Save As", "Undo", "Redo"] {
            window = window.child(Node::leaf(Role::Button, name, order, true));
            order += 1;
        }
        let mut list = Node::with_children(Role::List, "Files", order, false);
        order += 1;
        for row in 0..rows {
            list = list.child(Node::leaf(
                Role::ListItem,
                &format!("row-{row}"),
                order,
                true,
            ));
            order += 1;
        }
        window = window.child(list);
        window = window.child(Node::leaf(Role::ProgressBar, "Indexing", order, false));
        SemanticTree::new().root(window)
    }

    let tree = build_tree(200);
    let mut focus = FocusState::new(FocusOwner::Native);

    scenarios.push(
        measure(
            "a11y.focus_tab",
            "move focus one node forward in a 207-node semantic tree",
            1_000,
            50_000,
            || usize::from(focus.route(&tree, Key::Tab).is_ok()),
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "a11y.announce",
            "render a 207-node tree to the text a screen reader reads",
            200,
            10_000,
            || tree.to_announcement().len(),
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "a11y.unnamed_scan",
            "scan a tree for focusable nodes with no accessible name",
            1_000,
            20_000,
            || tree.unnamed_focusable().len(),
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Scenarios for `ncui-abi`.
///
/// `abi.negotiate` is the cost of deciding whether a component may load, which a host pays
/// before instantiating anything. It is not a hot path in the way a scheduler tick is, and the
/// scenario is here because a negotiation that got *slower* by an order of magnitude would be
/// a structural change — the grant is computed by folding over the capability set rather than
/// intersected, and that is the cost being watched.
///
/// `abi.grant_narrow` isolates the fold on its own, so a regression there is distinguishable
/// from one in version checking or surface validation.
fn abi(scenarios: &mut Vec<Repeated>) {
    use ncui_abi::surface::{Import, ImportKind, Surface};
    use ncui_abi::{ComponentManifest, HostProfile, negotiate};

    let mut surface = Surface::new();
    for (order, capability) in Capability::ALL.iter().enumerate() {
        surface = surface.import(Import {
            module: "ncui".to_owned(),
            name: format!("op-{order}"),
            kind: ImportKind::CapabilityCall,
            requires: Some(*capability),
        });
    }

    let manifest = ComponentManifest::new("1.0")
        .requesting(CapabilitySet::all())
        .with_surface(surface);
    // A profile offering everything, so the negotiation reaches the grant rather than
    // short-circuiting on a forbidden import — the expensive path is the interesting one.
    let profile = HostProfile::new(&["1.0"], CapabilitySet::all());

    scenarios.push(
        measure(
            "abi.negotiate",
            "negotiate a component declaring every capability and importing every capability",
            500,
            20_000,
            || usize::from(negotiate(&manifest, &profile).is_ok()),
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "abi.grant_narrow",
            "compute the granted set for a component requesting everything",
            1_000,
            50_000,
            || {
                let granted = profile
                    .narrow_to(&ComponentManifest::new("1.0").requesting(CapabilitySet::all()));
                usize::from(granted.contains(Capability::Gpu))
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}

/// Resolves a caller-supplied path against the repository root.
///
/// Cargo runs a bench binary with the *package* directory as its working directory, not
/// the workspace root, so a path written relative to the repository — as it appears in
/// CI — would not resolve. Anchoring to the manifest's grandparent makes the same path
/// work from a shell, from `cargo bench`, and from CI.
fn resolve_from_root(path: &str) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() || candidate.exists() {
        return candidate;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(candidate)
}

/// Where committed baselines live, relative to the repository root.
///
/// One directory per platform, because a p99 measured on Windows says nothing about a p99
/// measured on a Linux runner. The previous single-file layout meant CI always compared a
/// Linux run against the committed Windows baseline, and the platform guard correctly
/// refused — so the benchmark job failed on every run for a reason that had nothing to do
/// with performance. A platform with no committed baseline records its own.
fn baseline_relative() -> String {
    format!("benches/baseline/{}/latency.json", std::env::consts::OS)
}

fn output_path() -> PathBuf {
    // `CARGO_MANIFEST_DIR` points at the crate, so the result lands in the repository
    // rather than wherever the benchmark happened to be invoked from.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../benches/results")
        .join("latency.json")
}

fn print_report(report: &Report) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(
        out,
        "environment: {} {} ({} cpus, {}, {})",
        report.environment.os,
        report.environment.arch,
        report.environment.cpus,
        report.environment.profile,
        report.environment.rustc
    );
    let _ = writeln!(
        out,
        "{:<34} {:>11} {:>11} {:>11} {:>11} {:>11}",
        "scenario", "p50", "p90", "p95", "p99", "max"
    );
    for scenario in &report.scenarios {
        let _ = writeln!(
            out,
            "{:<34} {:>11} {:>11} {:>11} {:>11} {:>11}",
            scenario.name,
            format_ns(scenario.p50_ns),
            format_ns(scenario.p90_ns),
            format_ns(scenario.p95_ns),
            format_ns(scenario.p99_ns),
            format_ns(scenario.max_ns),
        );
    }
}

fn format_ns(value: u64) -> String {
    if value < 1_000 {
        format!("{value}ns")
    } else if value < 1_000_000 {
        format!("{:.2}us", value as f64 / 1_000.0)
    } else {
        format!("{:.2}ms", value as f64 / 1_000_000.0)
    }
}

fn fail(message: &str) -> ! {
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let _ = writeln!(out, "benchmark failure: {message}");
    std::process::exit(1);
}
