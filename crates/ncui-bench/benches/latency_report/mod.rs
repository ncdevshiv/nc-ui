//! Baseline persistence, comparison, and human-readable benchmark reporting.

use std::io::Write;
use std::path::{Path, PathBuf};

use ncui_bench::Report;

/// How much a scenario's measured p99 may grow before the comparison calls it a
/// difference worth reporting.
///
/// This is a **fraction**: `4.0` permits a 500% increase, i.e. a 5.0x window. The name says
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

/// Prints the measured repeat-to-repeat spread for every scenario.
///
/// This is the table that justifies `ALLOWED_REGRESSION`. Regenerate it with
/// `--noise-report` rather than copying the numbers, because a number copied by hand
/// stops describing the artifact it came from — which is exactly what happened here.
pub(super) fn print_noise(report: &Report) {
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
pub(super) fn write_json(path: &Path, json: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, json)
        .unwrap_or_else(|e| fail(&format!("cannot write {}: {e}", path.display())));
}

/// What the caller asked the harness to do with a baseline.
///
/// This exists as an enum rather than an `Option<PathBuf>` because the *action* is the
/// decision, and inferring it from a path is how the previous version ended up overwriting
/// the committed baseline on every CI run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BaselineAction {
    /// No baseline argument was given: measure and report only.
    MeasureOnly,
    /// Write this platform's baseline.
    Record,
    /// Compare against this baseline.
    Compare(PathBuf),
}

pub(super) fn baseline_action() -> BaselineAction {
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
pub(super) fn print_baseline_path() {
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
pub(super) fn compare_against_baseline(report: &Report, baseline_path: &Path) {
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

/// Resolves a caller-supplied path against the repository root.
///
/// Cargo runs a bench binary with the *package* directory as its working directory, not
/// the workspace root, so a path written relative to the repository — as it appears in
/// CI — would not resolve. Anchoring to the manifest's grandparent makes the same path
/// work from a shell, from `cargo bench`, and from CI.
pub(super) fn resolve_from_root(path: &str) -> PathBuf {
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
pub(super) fn baseline_relative() -> String {
    format!("benches/baseline/{}/latency.json", std::env::consts::OS)
}

pub(super) fn output_path() -> PathBuf {
    // `CARGO_MANIFEST_DIR` points at the crate, so the result lands in the repository
    // rather than wherever the benchmark happened to be invoked from.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../benches/results")
        .join("latency.json")
}

pub(super) fn print_report(report: &Report) {
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

pub(super) fn fail(message: &str) -> ! {
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let _ = writeln!(out, "benchmark failure: {message}");
    std::process::exit(1);
}
