//! Adversarial tests for the unfinished-work scanner.
//!
//! Each test reproduces a bypass found by attacking the scanner, so a regression would be
//! caught rather than rediscovered.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::fs;
use std::path::PathBuf;

use ncui_tools::scan::{self, ScanReport};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ncui-bypass-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp directory should be creatable");
    dir
}

fn scan(dir: &PathBuf) -> ScanReport {
    scan::scan(std::slice::from_ref(dir))
}

#[test]
fn production_source_is_not_exempt_because_a_parent_directory_is_named_docs() {
    let dir = scratch("nested-docs");
    fs::create_dir_all(dir.join("crates/ncui-foo/src/docs")).expect("temp directory");
    fs::write(
        dir.join("crates/ncui-foo/src/docs/lib.rs"),
        "// TODO: not exempt\n",
    )
    .expect("write should succeed");

    let report = scan(&dir);
    assert!(
        !report.is_clean(),
        "a source file under src/docs is production code and must not be exempt"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_marker_in_a_machine_readable_protocol_file_is_found() {
    let dir = scratch("json");
    fs::write(
        dir.join("protocol.json"),
        "{\"note\": \"TODO: finish the schema\"}\n",
    )
    .expect("write should succeed");

    let report = scan(&dir);
    assert!(
        !report.is_clean(),
        "the registry and protocol documents are exactly where a marker must be caught"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn renaming_a_marker_by_case_does_not_evade_the_scanner() {
    let dir = scratch("case");
    fs::write(dir.join("a.rs"), "// ToDo: x\n").expect("write");
    fs::write(dir.join("b.rs"), "// FixMe: y\n").expect("write");
    fs::write(dir.join("c.rs"), "let unimplemented = UNIMPLEMENTED!();\n").expect("write");

    let report = scan(&dir);
    let found: std::collections::BTreeSet<&str> =
        report.findings.iter().map(|f| f.token.as_str()).collect();
    assert!(
        found.contains("TODO") && found.contains("FIXME") && found.contains("unimplemented!()"),
        "case-renamed markers must still be found, got {found:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_top_level_tests_directory_is_still_exempt() {
    let dir = scratch("top-level-tests");
    fs::create_dir_all(dir.join("tests")).expect("temp directory");
    fs::write(dir.join("tests/fixture.rs"), "// TODO: fixture by design\n").expect("write");

    let report = scan(&dir);
    assert!(
        report.is_clean(),
        "top-level test directories remain exempt"
    );
    assert_eq!(report.exempt.len(), 1);

    let _ = fs::remove_dir_all(&dir);
}
