//! Latency measurement for NC-UI subsystems.
//!
//! Criterion measures throughput and change detection well, but spec 72 is explicit that
//! averages are not sufficient evidence: a mean hides exactly the tail that makes an
//! application feel broken. This module therefore records raw per-iteration timings and
//! reports percentiles directly, and writes them to a machine-readable file alongside the
//! environment they were measured in (spec 73).
//!
//! # Why the samples are kept
//!
//! Each scenario keeps every sample rather than streaming a histogram, because at the
//! iteration counts used here the memory is trivial and an exact percentile is worth more
//! than a constant-memory approximation that introduces its own error.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The environment a measurement was taken in, recorded alongside every result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    /// Operating system, as reported by `std::env::consts::OS`.
    pub os: String,
    /// CPU architecture.
    pub arch: String,
    /// Logical CPUs available.
    pub cpus: usize,
    /// Compiler version string.
    pub rustc: String,
    /// Whether the measurement was taken against an optimised build.
    pub profile: String,
}

impl Environment {
    /// Captures the current environment.
    ///
    /// `rustc` is recorded by invoking the compiler. If it cannot be run the field says so
    /// rather than claiming a version that was never observed.
    #[must_use]
    pub fn capture() -> Self {
        let cpus = std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get);
        Self {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            cpus,
            rustc: rustc_version(),
            profile: if cfg!(debug_assertions) {
                "debug".to_owned()
            } else {
                "release".to_owned()
            },
        }
    }
}

/// Returns the compiler version, or an explicit statement that it could not be read.
fn rustc_version() -> String {
    let output =
        std::process::Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
            .arg("--version")
            .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8(out.stdout)
            .map_or_else(|_| "unavailable".to_owned(), |text| text.trim().to_owned()),
        _ => "unavailable".to_owned(),
    }
}

/// The statistics reported for one scenario.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// Scenario name.
    pub name: String,
    /// One-line description of the workload, so the number has a meaning.
    pub workload: String,
    /// Iterations measured.
    pub iterations: usize,
    /// Untimed iterations run first, to warm caches.
    pub warmup: usize,
    /// Median.
    pub p50_ns: u64,
    /// 90th percentile.
    pub p90_ns: u64,
    /// 95th percentile.
    pub p95_ns: u64,
    /// 99th percentile.
    pub p99_ns: u64,
    /// Slowest observation.
    pub max_ns: u64,
    /// Fastest observation.
    pub min_ns: u64,
    /// Arithmetic mean.
    pub mean_ns: f64,
    /// Sample variance.
    pub variance_ns: f64,
}

impl Summary {
    /// Summarises `samples` for `name`/`workload`.
    ///
    /// Uses nearest-rank percentiles, so every reported value is an observation that
    /// actually occurred. Interpolating would produce figures such as 500.5 that were
    /// never measured and imply resolution the data does not have.
    #[must_use]
    pub fn from_samples(name: &str, workload: &str, samples: &[Duration], warmup: usize) -> Self {
        let mut sorted: Vec<u128> = samples.iter().map(Duration::as_nanos).collect();
        sorted.sort_unstable();
        let count = sorted.len().max(1);
        let sum: u128 = sorted.iter().sum();
        let mean = sum as f64 / count as f64;
        let variance = sorted
            .iter()
            .map(|value| {
                let delta = *value as f64 - mean;
                delta * delta
            })
            .sum::<f64>()
            / count as f64;

        Self {
            name: name.to_owned(),
            workload: workload.to_owned(),
            iterations: sorted.len(),
            warmup,
            p50_ns: percentile(&sorted, 50.0),
            p90_ns: percentile(&sorted, 90.0),
            p95_ns: percentile(&sorted, 95.0),
            p99_ns: percentile(&sorted, 99.0),
            max_ns: sorted.last().copied().unwrap_or(0) as u64,
            min_ns: sorted.first().copied().unwrap_or(0) as u64,
            mean_ns: mean,
            variance_ns: variance,
        }
    }
}

/// The nearest-rank percentile of a sorted slice.
#[must_use]
pub fn percentile(sorted: &[u128], percent: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((percent / 100.0) * sorted.len() as f64).ceil() as usize;
    let index = rank.clamp(1, sorted.len()) - 1;
    sorted[index] as u64
}

/// Times one scenario `REPEATS` times and reports the median repeat.
///
/// This is the entry point the benchmarks use. A single run's p99 is not a statistic
/// worth comparing against another run — see [`REPEATS`] for the measurements behind
/// that claim, and [`Repeated`] for which of the two statistics the gate compares.
///
/// # Errors
///
/// Returns an error when `iterations` is zero, because an empty sample set cannot produce
/// a percentile and reporting 0 for one would be a fabricated measurement.
pub fn measure<F>(
    name: &str,
    workload: &str,
    warmup: usize,
    iterations: usize,
    body: F,
) -> Result<Repeated, String>
where
    F: FnMut() -> usize,
{
    repeat(name, workload, warmup, iterations, REPEATS, body)
}

/// How many independent times each scenario is measured.
///
/// A single run's p99 is not a statistic worth comparing across runs. Measured on the
/// machine this baseline was recorded on, the same binary on unchanged code produced p99
/// swings by a large factor between runs, and the median of nine repeats swings too. The variation is the machine's, not the code's.
///
/// Nine repeats is chosen so the minimum has a plausible chance of catching at least one
/// uncontended run: with a slow-run rate around a third, nine attempts leave the odds
/// strongly in favour of at least one clean sample. The *minimum* is what the gate
/// compares, because interference can only ever make a run look slower.
pub const REPEATS: usize = 9;

/// The ratio within which two workloads must cost the same.
///
/// The project claims a million logical records cost no more than a small multiple of a
/// thousand (spec 18). A claim like that belongs in a test rather than in a sentence: if the
/// window computation ever starts scaling with the collection, this fails and the build
/// goes red, instead of a document quietly becoming wrong.
pub const WINDOW_COST_TOLERANCE: f64 = 2.0;

/// Whether `cheap` and `expensive` cost within [`WINDOW_COST_TOLERANCE`] of each other.
#[must_use]
pub fn costs_within_tolerance(cheap: u64, expensive: u64) -> bool {
    // A measurement of zero is a broken measurement, not a fast one. Returning `true` here
    // meant a scenario that stopped measuring would report itself as within tolerance.
    if cheap == 0 || expensive == 0 {
        return false;
    }
    let ratio = expensive as f64 / cheap as f64;
    // The band runs from the reciprocal to the tolerance. Written the other way round it
    // can never be satisfied, because a range whose start exceeds its end is empty — a
    // helper that rejects every input would have made this assertion vacuous.
    (1.0 / WINDOW_COST_TOLERANCE..=WINDOW_COST_TOLERANCE).contains(&ratio)
}

/// The outcome of measuring one scenario several times.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repeated {
    /// The scenario name.
    pub name: String,
    /// The workload description.
    pub workload: String,
    /// The summary used for reporting.
    pub summary: Summary,

    /// The fastest repeat.
    ///
    /// This is the statistic worth comparing across runs, not the median. A slow run is
    /// an outlier caused by the machine — another process, a frequency change, a page
    /// fault — and outliers only ever make a run look slower. Taking the *minimum* of nine
    /// repeats estimates the operation's cost with the interference divided out, and it is
    /// stable to within a few percent where the median swings by a factor of three.
    ///
    /// The cost of using the minimum is that it understates the experience of a run that
    /// genuinely contended, which is why the median is still what gets reported. The gate
    /// wants "did the code get slower", and the minimum answers that; a user-facing
    /// latency number wants the typical case, and the median answers that.
    pub floor: Summary,
    /// Every repeat, so the spread is visible rather than assumed.
    pub repeats: Vec<Summary>,
}

impl Repeated {
    /// The ratio between the slowest and the median repeat's p99.
    ///
    /// This is the noise this scenario exhibited on the recording machine, and it is the
    /// honest answer to "how much of a p99 change is a regression and how much is the
    /// machine?"
    #[must_use]
    pub fn noise_ratio(&self) -> f64 {
        let mut values: Vec<u64> = self.repeats.iter().map(|repeat| repeat.p99_ns).collect();
        values.sort_unstable();
        let median = values.get(values.len() / 2).copied().unwrap_or(0);
        let worst = values.last().copied().unwrap_or(0);
        if median == 0 {
            0.0
        } else {
            worst as f64 / median as f64
        }
    }
}

/// Measures one scenario `repeats` times and returns the median.
///
/// # Errors
///
/// Returns an error when `iterations` or `repeats` is zero, because an empty sample set
/// cannot produce a percentile and reporting zero for one would be a fabricated number.
pub fn repeat<F>(
    name: &str,
    workload: &str,
    warmup: usize,
    iterations: usize,
    repeats: usize,
    mut body: F,
) -> Result<Repeated, String>
where
    F: FnMut() -> usize,
{
    if iterations == 0 {
        return Err("a measurement needs at least one iteration".to_owned());
    }
    if repeats == 0 {
        return Err("a repeated measurement needs at least one repeat".to_owned());
    }

    let mut summaries = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        summaries.push(measure_once(name, workload, warmup, iterations, &mut body)?);
    }
    let mut sorted = summaries.clone();
    sorted.sort_by_key(|summary| summary.p99_ns);
    let median = sorted[sorted.len() / 2].clone();
    let floor = sorted[0].clone();

    Ok(Repeated {
        name: name.to_owned(),
        workload: workload.to_owned(),
        summary: median,
        floor,
        repeats: summaries,
    })
}

fn measure_once<F>(
    name: &str,
    workload: &str,
    warmup: usize,
    iterations: usize,
    body: &mut F,
) -> Result<Summary, String>
where
    F: FnMut() -> usize,
{
    if iterations == 0 {
        return Err("a measurement needs at least one iteration".to_owned());
    }
    for _ in 0..warmup {
        body();
    }

    let mut samples = Vec::with_capacity(iterations);
    // A black box keeps the optimiser from deleting a loop body whose result is unused,
    // which would otherwise measure nothing at all.
    let mut sink = 0usize;
    for _ in 0..iterations {
        let started = Instant::now();
        let produced = body();
        samples.push(started.elapsed());
        sink = sink.wrapping_add(produced);
    }
    core::hint::black_box(sink);

    Ok(Summary::from_samples(name, workload, &samples, warmup))
}

/// A complete measurement run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// Schema version.
    pub schema_version: u32,
    /// Where the measurement was taken.
    pub environment: Environment,
    /// Every scenario measured, reporting the median repeat.
    pub scenarios: Vec<Summary>,
    /// Every scenario's fastest repeat, which is what the gate compares.
    ///
    /// Kept separate from `scenarios` so a report cannot quietly compare the wrong one.
    #[serde(default)]
    pub floors: Vec<Summary>,
    /// Per-scenario spread between repeats: the ratio of the slowest repeat to the median.
    ///
    /// This is the number that justifies the regression tolerance, so it belongs in the
    /// artifact rather than in a table a maintainer typed. It was previously transcribed
    /// into three documents that disagreed with each other.
    #[serde(default)]
    pub noise: Vec<NoiseEntry>,
}

/// One scenario's measured repeat-to-repeat spread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoiseEntry {
    /// The scenario.
    pub name: String,
    /// How many repeats were measured.
    pub runs: usize,
    /// The median repeat's p99.
    pub median_p99_ns: u64,
    /// The fastest repeat's p99.
    pub floor_p99_ns: u64,
    /// The slowest repeat's p99.
    pub max_p99_ns: u64,
    /// Slowest over median: the spread the tolerance must clear.
    pub spread_ratio: f64,
}

impl Report {
    /// The worst measured spread across every scenario, and the scenario that produced it.
    #[must_use]
    pub fn worst_noise(&self) -> Option<&NoiseEntry> {
        self.noise
            .iter()
            .max_by(|a, b| a.spread_ratio.total_cmp(&b.spread_ratio))
    }
}

impl Report {
    /// Serialises the report as pretty JSON with a trailing newline.
    ///
    /// # Errors
    ///
    /// Returns the underlying serialisation error.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        Ok(text)
    }

    /// The scenarios whose p99 exceeds `threshold_ns`.
    #[must_use]
    pub fn regressions_against(&self, threshold_ns: u64) -> Vec<&Summary> {
        self.scenarios
            .iter()
            .filter(|s| s.p99_ns > threshold_ns)
            .collect()
    }

    /// Whether two reports were recorded on a comparable machine.
    ///
    /// A p99 measured on a Windows workstation and one measured on a shared Linux runner
    /// are not comparable numbers, and comparing them produces a "regression" that is only
    /// an artefact of the hardware. The gate refuses rather than guessing.
    #[must_use]
    pub fn is_comparable_to(&self, other: &Self) -> bool {
        self.environment.os == other.environment.os
            && self.environment.arch == other.environment.arch
    }

    /// Compares this run against a recorded `baseline`, returning the scenarios whose p99
    /// regressed by more than `allowed_fraction`.
    ///
    /// A scenario present in the baseline but missing from this run is a regression in its
    /// own right: a benchmark that quietly disappeared would otherwise look like a
    /// performance improvement.
    #[must_use]
    pub fn regressions_vs_baseline(
        &self,
        baseline: &Self,
        allowed_fraction: f64,
    ) -> Vec<Regression> {
        let mut out = Vec::new();
        let allowed = 1.0 + allowed_fraction;

        for scenario in &self.scenarios {
            let Some(previous) = baseline
                .scenarios
                .iter()
                .find(|candidate| candidate.name == scenario.name)
            else {
                out.push(Regression {
                    name: scenario.name.clone(),
                    baseline_p99_ns: None,
                    current_p99_ns: Some(scenario.p99_ns),
                    change: None,
                    reason: "new scenario with no baseline".to_owned(),
                });
                continue;
            };
            if previous.p99_ns == 0 {
                continue;
            }
            let change = scenario.p99_ns as f64 / previous.p99_ns as f64;
            if change > allowed {
                out.push(Regression {
                    name: scenario.name.clone(),
                    baseline_p99_ns: Some(previous.p99_ns),
                    current_p99_ns: Some(scenario.p99_ns),
                    change: Some(change),
                    reason: "p99 above the allowed regression".to_owned(),
                });
            }
        }

        for previous in &baseline.scenarios {
            if !self.scenarios.iter().any(|s| s.name == previous.name) {
                out.push(Regression {
                    name: previous.name.clone(),
                    baseline_p99_ns: Some(previous.p99_ns),
                    current_p99_ns: None,
                    change: None,
                    reason: "scenario disappeared from the suite".to_owned(),
                });
            }
        }
        out
    }
}

/// A scenario whose measured p99 regressed beyond the allowed fraction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Regression {
    /// The scenario that regressed.
    pub name: String,
    /// Its recorded p99, when the baseline had one.
    pub baseline_p99_ns: Option<u64>,
    /// Its p99 in this run.
    pub current_p99_ns: Option<u64>,
    /// The ratio, when both runs had a measurement.
    pub change: Option<f64>,
    /// Why it was reported.
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nanos(values: &[u64]) -> Vec<Duration> {
        values.iter().map(|v| Duration::from_nanos(*v)).collect()
    }

    #[test]
    fn percentiles_use_nearest_rank_on_known_data() {
        let samples = nanos(&(1..=1000).collect::<Vec<u64>>());
        let summary = Summary::from_samples("t", "known dataset", &samples, 0);
        assert_eq!(summary.p50_ns, 500);
        assert_eq!(summary.p90_ns, 900);
        assert_eq!(summary.p95_ns, 950);
        assert_eq!(summary.p99_ns, 990);
        assert_eq!(summary.min_ns, 1);
        assert_eq!(summary.max_ns, 1000);
        assert_eq!(summary.iterations, 1000);
    }

    #[test]
    fn every_reported_percentile_is_a_real_observation() {
        // Nearest rank must never invent a value that was not measured.
        let samples = nanos(&[10, 20, 30]);
        let summary = Summary::from_samples("t", "odd count", &samples, 0);
        for value in [
            summary.p50_ns,
            summary.p90_ns,
            summary.p95_ns,
            summary.p99_ns,
        ] {
            assert!(
                [10, 20, 30].contains(&value),
                "percentile {value} was never observed"
            );
        }
    }

    #[test]
    fn a_single_sample_reports_itself_as_every_percentile() {
        let summary = Summary::from_samples("t", "one", &nanos(&[42]), 0);
        assert_eq!(summary.p50_ns, 42);
        assert_eq!(summary.p99_ns, 42);
        assert_eq!(summary.max_ns, 42);
        assert_eq!(summary.min_ns, 42);
        assert!((summary.mean_ns - 42.0).abs() < f64::EPSILON);
        assert!(summary.variance_ns.abs() < f64::EPSILON);
    }

    #[test]
    fn mean_and_variance_match_hand_computation() {
        let samples = nanos(&[2, 4, 4, 4, 5, 5, 7, 9]);
        let summary = Summary::from_samples("t", "classic", &samples, 0);
        assert!((summary.mean_ns - 5.0).abs() < 1e-9);
        // Population variance of that classic dataset is 4.
        assert!(
            (summary.variance_ns - 4.0).abs() < 1e-9,
            "got {}",
            summary.variance_ns
        );
    }

    #[test]
    fn measuring_zero_iterations_is_refused_rather_than_reported_as_zero() {
        let err = measure("t", "empty", 0, 0, || 1).expect_err("zero iterations must be refused");
        assert!(err.contains("at least one iteration"), "got: {err}");
    }

    #[test]
    fn one_repeat_runs_the_warmup_and_the_measured_iterations() {
        let mut calls = 0usize;
        let repeated = repeat("t", "counter", 5, 10, 1, || {
            calls += 1;
            calls
        })
        .expect("measurement should succeed");
        assert_eq!(calls, 15, "warmup plus iterations must all run");
        assert_eq!(repeated.summary.iterations, 10);
        assert_eq!(repeated.summary.warmup, 5);
    }

    #[test]
    fn measure_repeats_by_default() {
        let calls = std::cell::Cell::new(0usize);
        measure("t", "counter", 0, 1, || {
            calls.set(calls.get() + 1);
            calls.get()
        })
        .expect("measurement should succeed");
        assert_eq!(
            calls.get(),
            REPEATS,
            "the default entry point must measure the scenario REPEATS times"
        );
    }

    #[test]
    fn a_repeated_measurement_runs_every_repeat_and_reports_the_median() {
        let mut calls = 0usize;
        let repeated = repeat("t", "counter", 2, 3, 5, || {
            calls += 1;
            calls
        })
        .expect("repeated measurement should succeed");

        // Five repeats of (two warmups plus three iterations).
        assert_eq!(calls, 5 * 5, "every repeat must be measured in full");
        assert_eq!(repeated.repeats.len(), 5);
        assert_eq!(
            repeated.summary.iterations, 3,
            "the reported figure is one repeat's"
        );

        // The median is a real member of the set, not an interpolation.
        let p99s: Vec<u64> = repeated.repeats.iter().map(|r| r.p99_ns).collect();
        let mut sorted = p99s.clone();
        sorted.sort_unstable();
        assert_eq!(
            repeated.summary.p99_ns, sorted[2],
            "the median must be a real repeat"
        );
        assert!(p99s.contains(&repeated.summary.p99_ns));
    }

    #[test]
    fn a_repeated_measurement_needs_at_least_one_repeat() {
        let err = repeat("t", "empty", 0, 1, 0, || 1).expect_err("zero repeats must be refused");
        assert!(err.contains("at least one repeat"), "got: {err}");
    }

    #[test]
    fn the_noise_ratio_reports_what_the_machine_actually_did() {
        let summary = |p99| Summary {
            name: "t".to_owned(),
            workload: "w".to_owned(),
            iterations: 1,
            warmup: 0,
            p50_ns: p99,
            p90_ns: p99,
            p95_ns: p99,
            p99_ns: p99,
            max_ns: p99,
            min_ns: p99,
            mean_ns: p99 as f64,
            variance_ns: 0.0,
        };
        let quiet = Repeated {
            name: "quiet".to_owned(),
            workload: "w".to_owned(),
            summary: summary(100),
            floor: summary(100),
            repeats: vec![summary(100), summary(100), summary(100)],
        };
        assert!(
            (quiet.noise_ratio() - 1.0).abs() < 1e-9,
            "identical repeats are 1.0"
        );

        let noisy = Repeated {
            name: "noisy".to_owned(),
            workload: "w".to_owned(),
            summary: summary(100),
            floor: summary(100),
            repeats: vec![summary(100), summary(100), summary(300)],
        };
        assert!(
            (noisy.noise_ratio() - 3.0).abs() < 1e-9,
            "the worst repeat is three times the median, and the gate must be able to see that"
        );
    }

    #[test]
    fn the_environment_records_the_build_profile() {
        let env = Environment::capture();
        assert!(!env.os.is_empty());
        assert!(!env.arch.is_empty());
        assert!(
            env.profile == "debug" || env.profile == "release",
            "the profile must be stated, got {}",
            env.profile
        );
    }

    fn report_of(summaries: Vec<Summary>) -> Report {
        Report {
            schema_version: 1,
            environment: Environment::capture(),
            scenarios: summaries.clone(),
            floors: summaries,
            noise: Vec::new(),
        }
    }

    #[test]
    fn the_tolerance_band_is_actually_a_band() {
        // An inverted range is empty, so every input would be rejected — or, if written the
        // other way, every input would be accepted. Both are vacuous. Check the shape.
        assert!(
            costs_within_tolerance(1_500, 2_900),
            "1.93x is inside a 2x band"
        );
        assert!(
            costs_within_tolerance(2_900, 1_500),
            "and so is its reciprocal"
        );
        assert!(!costs_within_tolerance(1_500, 100_000), "66x is not");
        assert!(
            !costs_within_tolerance(100_000, 1_500),
            "nor is its reciprocal"
        );
    }

    #[test]
    fn a_broken_measurement_fails_rather_than_passing() {
        assert!(
            !costs_within_tolerance(0, 100),
            "a zero measurement is not a fast one"
        );
        assert!(!costs_within_tolerance(100, 0));
    }

    #[test]
    fn reports_from_different_platforms_are_not_comparable() {
        let baseline = Report {
            schema_version: 1,
            environment: Environment {
                os: "windows".to_owned(),
                arch: "x86_64".to_owned(),
                cpus: 12,
                rustc: "test".to_owned(),
                profile: "release".to_owned(),
            },
            scenarios: Vec::new(),
            floors: Vec::new(),
            noise: Vec::new(),
        };
        let linux = Report {
            environment: Environment {
                os: "linux".to_owned(),
                ..baseline.environment.clone()
            },
            ..baseline.clone()
        };
        assert!(
            !linux.is_comparable_to(&baseline),
            "a Linux run must not be compared against a Windows baseline"
        );
        assert!(baseline.is_comparable_to(&baseline));
    }

    #[test]
    fn a_baseline_comparison_flags_a_real_regression() {
        let baseline = report_of(vec![Summary::from_samples(
            "hot",
            "w",
            &nanos(&[100, 100]),
            0,
        )]);
        let current = report_of(vec![Summary::from_samples(
            "hot",
            "w",
            &nanos(&[500, 500]),
            0,
        )]);
        let regressions = current.regressions_vs_baseline(&baseline, 0.20);
        assert_eq!(regressions.len(), 1, "a 5x p99 regression must be reported");
        assert_eq!(regressions[0].name, "hot");
        assert!(regressions[0].change.expect("change") > 4.0);
    }

    #[test]
    fn a_baseline_comparison_tolerates_change_within_the_allowed_fraction() {
        let baseline = report_of(vec![Summary::from_samples(
            "hot",
            "w",
            &nanos(&[100, 100]),
            0,
        )]);
        let current = report_of(vec![Summary::from_samples(
            "hot",
            "w",
            &nanos(&[110, 110]),
            0,
        )]);
        assert!(
            current.regressions_vs_baseline(&baseline, 0.20).is_empty(),
            "a 10% change is inside a 20% allowance"
        );
    }

    #[test]
    fn a_scenario_that_disappeared_counts_as_a_regression() {
        let baseline = report_of(vec![
            Summary::from_samples("kept", "w", &nanos(&[100, 100]), 0),
            Summary::from_samples("removed", "w", &nanos(&[100, 100]), 0),
        ]);
        let current = report_of(vec![Summary::from_samples(
            "kept",
            "w",
            &nanos(&[100, 100]),
            0,
        )]);
        let regressions = current.regressions_vs_baseline(&baseline, 0.20);
        assert_eq!(regressions.len(), 1);
        assert_eq!(regressions[0].name, "removed");
        assert!(regressions[0].reason.contains("disappeared"));
    }

    #[test]
    fn a_report_round_trips_through_json_and_reports_regressions() {
        let report = Report {
            schema_version: 1,
            environment: Environment::capture(),
            scenarios: vec![
                Summary::from_samples("fast", "w", &nanos(&[10, 10, 10]), 0),
                Summary::from_samples("slow", "w", &nanos(&[5000, 6000, 7000]), 0),
            ],
            floors: vec![
                Summary::from_samples("fast", "w", &nanos(&[10, 10, 10]), 0),
                Summary::from_samples("slow", "w", &nanos(&[5000, 6000, 7000]), 0),
            ],
            noise: Vec::new(),
        };
        let json = report.to_json().expect("serialisable");
        assert!(json.ends_with('\n'));
        let back: Report = serde_json::from_str(&json).expect("deserialisable");
        assert_eq!(back, report);

        let regressions = back.regressions_against(1_000);
        assert_eq!(regressions.len(), 1);
        assert_eq!(regressions[0].name, "slow");
        assert!(back.regressions_against(100_000).is_empty());
    }
}
