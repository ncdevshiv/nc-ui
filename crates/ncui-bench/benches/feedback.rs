//! Developer feedback-loop scenarios: the wall-clock cost of what a developer waits on.
//!
//! Every other scenario group in `latency.rs` measures a hot path that can be looped
//! thousands of times. These measure one-shot operations — `cargo check`, a focused
//! unit-test run, an edit→visible cycle — that are inherently seconds-to-minutes long and
//! cannot be repeated nine times per invocation without dominating the suite.
//!
//! They are measured with [`ncui_bench::measure_wall_clock`], which runs the workload once
//! and records the elapsed time as a single sample. The `noise` entry for each is therefore
//! 1.0 (one sample), which states plainly that these are point-in-time measurements rather
//! than distributions. A regression is still visible, because the baseline comparison uses
//! the single recorded value.
//!
//! These are the evidence behind the "faster build/test/debug" goal: a change that makes
//! `cargo check` or the unit suite slower shows up here rather than only in developer
//! frustration.

use std::path::PathBuf;

use ncui_bench::{Repeated, measure_wall_clock};

fn fail(message: &str) -> ! {
    use std::io::Write;
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let _ = writeln!(out, "benchmark failure: {message}");
    std::process::exit(1);
}

/// Records every feedback-loop scenario.
pub fn record(scenarios: &mut Vec<Repeated>) {
    build_check(scenarios);
    test_unit(scenarios);
    edit_to_visible(scenarios);
}

/// `cargo check -p ncui-core` — the fastest representative compile check.
///
/// `ncui-core` is the foundational crate: almost every other crate depends on it, so a
/// change there forces the widest rebuild. Checking it is the lower bound on "how long
/// until the compiler tells me I broke something". A warm cache is the common case (the
/// developer just built), so this measures the incremental check path, not a cold build.
fn build_check(scenarios: &mut Vec<Repeated>) {
    scenarios.push(measure_wall_clock(
        "build.check",
        "cargo check -p ncui-core (warm cache, the lower bound on compile feedback)",
        || {
            let output = std::process::Command::new("cargo")
                .args(["check", "-p", "ncui-core", "--quiet"])
                .current_dir(repo_root())
                .output();
            match output {
                Ok(out) if out.status.success() => 1,
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    fail(&format!("cargo check failed: {stderr}"));
                }
                Err(e) => fail(&format!("cargo check could not be spawned: {e}")),
            }
        },
    ));
}

/// `cargo test -p ncui-core --quiet` — the fastest representative unit-test run.
///
/// `ncui-core` holds the foundational vocabulary (errors, ids, capabilities, versions,
/// limits) and has a focused unit suite. Running it is the lower bound on "how long until
/// I know my change broke a test". A warm build is the common case.
fn test_unit(scenarios: &mut Vec<Repeated>) {
    scenarios.push(measure_wall_clock(
        "test.unit",
        "cargo test -p ncui-core --quiet (warm build, the lower bound on test feedback)",
        || {
            let output = std::process::Command::new("cargo")
                .args(["test", "-p", "ncui-core", "--quiet"])
                .current_dir(repo_root())
                .output();
            match output {
                Ok(out) if out.status.success() => 1,
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    fail(&format!("cargo test failed: {stderr}"));
                }
                Err(e) => fail(&format!("cargo test could not be spawned: {e}")),
            }
        },
    ));
}

/// Edit→visible change: touch a source file, then `cargo check` to see the recompile.
///
/// This is the "instant rebuild" promise made concrete. The workload writes a trivial
/// comment to a real source file, runs `cargo check`, and restores the file. The measured
/// time is what a developer pays between saving a change and seeing the compiler's verdict.
/// A runtime that cannot reload a page in less time than this is not competitive on
/// developer experience regardless of its runtime performance.
fn edit_to_visible(scenarios: &mut Vec<Repeated>) {
    let target = repo_root().join("crates/ncui-core/src/lib.rs");
    if !target.exists() {
        fail("the edit→visible benchmark's target file does not exist");
    }
    scenarios.push(measure_wall_clock(
        "edit_to_visible",
        "touch a source file then cargo check (the edit→visible developer loop)",
        || {
            // Append a comment, check, then restore. The comment is a no-op for the
            // compiler but changes the file's mtime and content hash, which is what
            // forces a recompile of the crate.
            let original = std::fs::read_to_string(&target)
                .unwrap_or_else(|e| fail(&format!("cannot read target: {e}")));
            let touched = format!("{original}\n// edit_to_visible benchmark touch\n");
            if std::fs::write(&target, &touched).is_err() {
                fail("cannot write touched file");
            }
            let output = std::process::Command::new("cargo")
                .args(["check", "-p", "ncui-core", "--quiet"])
                .current_dir(repo_root())
                .output();
            // Restore the original content regardless of the check's outcome.
            let _ = std::fs::write(&target, &original);
            match output {
                Ok(out) if out.status.success() => 1,
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    fail(&format!("cargo check after edit failed: {stderr}"));
                }
                Err(e) => fail(&format!("cargo check could not be spawned: {e}")),
            }
        },
    ));
}

/// The repository root, anchored to this benchmark's manifest directory.
///
/// `cargo bench` runs with the package directory as its working directory, so a relative
/// path would not resolve. Anchoring to `CARGO_MANIFEST_DIR`'s grandparent makes the same
/// path work from a shell, from `cargo bench`, and from CI.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
