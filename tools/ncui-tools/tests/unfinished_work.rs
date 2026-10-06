//! Behavioural tests for the unfinished-work scanner.
//!
//! These live under `tests/` rather than in the library because they deliberately contain
//! the very tokens the scanner forbids. Keeping them here means the scanner can prove it
//! works against real markers without the policy having to exempt its own source
//! directory, which would be a blind spot.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::fs;
use std::path::PathBuf;

use ncui_tools::scan::{self, ScanReport};

/// Creates a fresh temporary directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ncui-scanner-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp directory should be creatable");
    dir
}

fn scan_one(dir: &PathBuf) -> ScanReport {
    scan::scan(std::slice::from_ref(dir))
}

#[test]
fn a_real_marker_in_production_source_is_reported() {
    let dir = scratch("reported");
    fs::write(
        dir.join("mod.rs"),
        "// TODO: implement the codec\npub fn f() {}\n",
    )
    .expect("write should succeed");

    let report = scan_one(&dir);
    assert!(!report.is_clean(), "a planted marker must fail the scan");
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].token, "TODO");
    assert_eq!(report.findings[0].file, "mod.rs");
    assert_eq!(report.findings[0].line, 1);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn every_banned_marker_is_detected() {
    let dir = scratch("all-markers");
    let markers = [
        "TODO",
        "FIXME",
        "TBD",
        "XXX",
        "HACK",
        "NotImplemented",
        "unimplemented!()",
        "todo!()",
        "placeholder",
        "stub",
        "fake",
        "dummy",
        "temporary",
        "demo implementation",
        "partial implementation",
    ];
    let mut body = String::new();
    for marker in markers {
        body.push_str("// ");
        body.push_str(marker);
        body.push('\n');
    }
    fs::write(dir.join("all.rs"), body).expect("write should succeed");

    let report = scan_one(&dir);
    let found: std::collections::BTreeSet<&str> =
        report.findings.iter().map(|f| f.token.as_str()).collect();
    for marker in markers {
        assert!(
            found.contains(marker),
            "marker `{marker}` was not detected; detected: {found:?}"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_marker_embedded_in_a_longer_identifier_is_not_a_finding() {
    let dir = scratch("boundaries");
    fs::write(
        dir.join("ok.rs"),
        "let todo_count = 0;\nlet stubborn = 1;\n// TODOS_ARE_COMPLETE_HERE\n",
    )
    .expect("write should succeed");

    let report = scan_one(&dir);
    assert!(
        report.is_clean(),
        "substring matches must not count; found {:?}",
        report.findings
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_reasonless_suppression_is_itself_a_finding() {
    let dir = scratch("reasonless");
    fs::write(
        dir.join("x.rs"),
        "// TODO: real // ncui-allow-placeholder:\n",
    )
    .expect("write");

    let report = scan_one(&dir);
    assert!(!report.is_clean());
    assert_eq!(report.bad_suppressions.len(), 1);
    assert!(
        report.bad_suppressions[0]
            .reason
            .contains("written justification"),
        "the reason should explain what is missing"
    );
    // A reasonless suppression does not actually suppress anything. This line contains
    // two banned tokens — the marker itself and the word inside the annotation — so
    // both are reported.
    let found: std::collections::BTreeSet<&str> =
        report.findings.iter().map(|f| f.token.as_str()).collect();
    assert_eq!(
        found,
        std::collections::BTreeSet::from(["TODO", "placeholder"])
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_written_suppression_is_honoured_and_recorded_as_exempt() {
    let dir = scratch("justified");
    fs::write(
        dir.join("x.rs"),
        "const MARKER: u8 = 1; // ncui-allow-placeholder: the word names a wire constant\n",
    )
    .expect("write");

    let report = scan_one(&dir);
    assert!(
        report.findings.is_empty(),
        "a justified suppression should silence the finding"
    );
    assert!(report.bad_suppressions.is_empty());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn policy_exempt_locations_are_reported_rather_than_hidden() {
    let dir = scratch("exempt");
    fs::create_dir_all(dir.join("tests")).expect("temp directory");
    fs::write(
        dir.join("tests/fixture.rs"),
        "// this fixture is a fake by design\n",
    )
    .expect("write");

    let report = scan_one(&dir);
    assert!(
        report.is_clean(),
        "an exempt location must not fail the build"
    );
    assert_eq!(report.exempt.len(), 1, "but it must still be reported");
    assert_eq!(report.exempt[0].file, "tests/fixture.rs");
    assert_eq!(report.exempt[0].allowed_tokens, vec!["fake".to_owned()]);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn build_output_and_vendored_directories_are_never_scanned() {
    let dir = scratch("skipped");
    fs::create_dir_all(dir.join("target")).expect("temp directory");
    fs::create_dir_all(dir.join("node_modules/pkg")).expect("temp directory");
    fs::write(dir.join("target/generated.rs"), "// TODO: generated\n").expect("write");
    fs::write(
        dir.join("node_modules/pkg/index.js"),
        "// FIXME: vendored\n",
    )
    .expect("write");

    let report = scan_one(&dir);
    assert!(
        report.is_clean(),
        "build output and vendored code are out of scope"
    );
    assert_eq!(report.files_scanned, 0);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn scanning_is_deterministic_across_runs() {
    let dir = scratch("determinism");
    fs::create_dir_all(dir.join("b")).expect("temp directory");
    fs::create_dir_all(dir.join("a")).expect("temp directory");
    fs::write(
        dir.join("b/second.rs"),
        "// FIXME: later
",
    )
    .expect("write should succeed");
    fs::write(
        dir.join("a/first.rs"),
        "// TODO: later
",
    )
    .expect("write should succeed");

    let first = scan::scan(std::slice::from_ref(&dir));
    let second = scan::scan(std::slice::from_ref(&dir));
    let names: Vec<_> = first.findings.iter().map(|f| f.file.clone()).collect();
    let again: Vec<_> = second.findings.iter().map(|f| f.file.clone()).collect();

    assert_eq!(names, again, "scan output must be stable across runs");
    assert_eq!(
        names,
        vec!["a/first.rs".to_owned(), "b/second.rs".to_owned()],
        "files must be visited in sorted order so results are reproducible"
    );

    let _ = fs::remove_dir_all(&dir);
}
