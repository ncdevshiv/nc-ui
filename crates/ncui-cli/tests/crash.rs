//! End-to-end tests for the on-disk crash report: ADR-001's reporting half.
//!
//! The hook is installed for real, a panic is raised and caught, and the report it left
//! in the crash directory is read back and asserted on — the same path a real crash
//! takes, minus the process dying.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::panic;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[test]
fn a_caught_panic_leaves_a_report_naming_itself_in_the_crash_directory() {
    let directory = std::env::temp_dir().join(format!("ncui-crash-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);

    // Install for real, panic for real, and restore the harness's own hook afterwards so
    // any later failure reports the way the harness expects.
    let previous = panic::take_hook();
    ncui_cli::crash::install_into(directory.clone(), Instant::now());
    let outcome = panic::catch_unwind(|| panic!("boom for the crash report"));
    let _ = panic::take_hook();
    panic::set_hook(previous);

    assert!(outcome.is_err(), "the panic must still unwind as a panic");
    let report = newest_report(&directory).expect("a crash report was written");
    let text = std::fs::read_to_string(&report).expect("the report is readable");
    assert!(
        text.contains("boom for the crash report"),
        "the report names the panic: {text}"
    );
    assert!(
        text.contains("\"version\"")
            && text.contains("\"backtrace\"")
            && text.contains("\"uptime\""),
        "the report carries the diagnostic fields: {text}"
    );
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("valid json");
    assert!(
        value["location"]["line"].as_u64().is_some(),
        "the panic's source location is recorded: {value}"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

/// The one crash report in `directory`, when the hook wrote one.
fn newest_report(directory: &Path) -> Option<PathBuf> {
    std::fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
}
