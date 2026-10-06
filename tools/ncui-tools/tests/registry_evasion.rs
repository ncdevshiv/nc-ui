//! Adversarial tests for the feature-registry validator.
//!
//! The validator is the gate that makes the registry trustworthy, so the way to be sure it
//! works is to try to defeat it. Each test here reproduces an evasion that a previous
//! version of the validator allowed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use ncui_tools::registry::{
    self, Evidence, EvidenceItem, Feature, FeatureStatus, PlatformState, Registry,
};

/// Every path the shared fixture cites. A scratch root must contain all of them, or the
/// test reports on the fixture's own setup instead of the behaviour under test.
const CITED_FILES: &[&str] = &[
    "crates/ncui-core/tests/runtime_contracts.rs",
    "crates/ncui-core/src/error.rs",
    "crates/ncui-bench/benches/latency.rs",
    "docs/adr/ADR-007-security-model.md",
    "docs/adr/ADR-009-observability.md",
    "docs/adr/ADR-011-crate-layering.md",
    "docs/adr/ADR-012-dependency-policy.md",
];

/// Builds a scratch root containing every file the shared fixture cites.
///
/// A scratch root that omits them would make the test report on its own setup rather than
/// on the behaviour under test.
fn scratch_with_citations(name: &str) -> PathBuf {
    let dir = scratch(name);
    for file in CITED_FILES {
        let path = dir.join(file);
        fs::create_dir_all(path.parent().expect("a file has a parent")).expect("temp directory");
        fs::write(
            path,
            b"// cited by the fixture
",
        )
        .expect("write should succeed");
    }
    dir
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ncui-registry-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp directory should be creatable");
    dir
}

/// Citations that name no repository path, so the path-resolution check is not the thing
/// under test. The separate `a_citation_naming_a_real_file_is_accepted` test covers
/// resolution with a file that exists.
fn verified_evidence() -> Evidence {
    Evidence {
        unit_tests: EvidenceItem::verified("crates/ncui-core/tests/runtime_contracts.rs"),
        integration_tests: EvidenceItem::verified("crates/ncui-core/tests/runtime_contracts.rs"),
        failure_tests: EvidenceItem::verified("crates/ncui-core/src/error.rs"),
        benchmarks: EvidenceItem::verified("crates/ncui-bench/benches/latency.rs"),
        // `tracing` and `metrics` are required, and a required slot must name an artifact.
        tracing: EvidenceItem::verified("docs/adr/ADR-009-observability.md"),
        metrics: EvidenceItem::verified("docs/adr/ADR-009-observability.md"),
        documentation: EvidenceItem::verified("docs/adr/ADR-011-crate-layering.md"),
        security_review: EvidenceItem::verified("docs/adr/ADR-007-security-model.md"),
        api_review: EvidenceItem::verified("docs/adr/ADR-012-dependency-policy.md"),
    }
}

fn platforms() -> BTreeMap<String, PlatformState> {
    [
        ("linux".to_owned(), PlatformState::Verified),
        ("windows".to_owned(), PlatformState::Verified),
        ("macos".to_owned(), PlatformState::NotApplicable),
    ]
    .into_iter()
    .collect()
}

fn complete_feature(id: &str) -> Feature {
    Feature {
        id: id.to_owned(),
        summary: "a feature".to_owned(),
        status: FeatureStatus::Complete,
        crates: vec!["ncui-core".to_owned()],
        evidence: verified_evidence(),
        platforms: platforms(),
    }
}

fn registry_of(features: Vec<Feature>) -> Registry {
    Registry {
        schema_version: 1,
        verification_note: "a note".to_owned(),
        features,
    }
}

#[test]
fn a_complete_feature_with_no_evidence_at_all_is_rejected() {
    // The original evasion: mark every evidence slot `required: false`. That field no
    // longer exists, so the only way to express this is empty evidence, and the
    // validator-owned required set rejects it.
    let feature = Feature {
        id: "attack.no_evidence".to_owned(),
        summary: "claims to be complete with nothing behind it".to_owned(),
        status: FeatureStatus::Complete,
        crates: vec!["ncui-core".to_owned()],
        evidence: Evidence {
            unit_tests: EvidenceItem::absent(),
            integration_tests: EvidenceItem::absent(),
            failure_tests: EvidenceItem::absent(),
            benchmarks: EvidenceItem::absent(),
            tracing: EvidenceItem::absent(),
            metrics: EvidenceItem::absent(),
            documentation: EvidenceItem::absent(),
            security_review: EvidenceItem::absent(),
            api_review: EvidenceItem::absent(),
        },
        platforms: platforms(),
    };

    let result = registry::validate(&registry_of(vec![feature]), None);
    assert!(
        !result.is_clean(),
        "a feature with no evidence must never pass: {:?}",
        result.violations
    );
    assert!(
        result
            .violations
            .iter()
            .any(|v| v.message.contains("unit_tests")),
        "the missing required evidence must be named"
    );
}

#[test]
fn a_performance_sensitive_feature_must_carry_a_benchmark() {
    // `required_for` is keyed by prefix, so an author cannot drop the benchmark dimension
    // by declining to mention it.
    let mut feature = complete_feature("ipc.binary_framing");
    feature.evidence.benchmarks = EvidenceItem::absent();
    let result = registry::validate(&registry_of(vec![feature]), None);
    assert!(
        result
            .violations
            .iter()
            .any(|v| v.message.contains("benchmarks")),
        "an ipc feature claiming completeness without a benchmark must be rejected"
    );
}

#[test]
fn a_non_performance_feature_is_not_demanded_a_benchmark() {
    let dir = scratch_with_citations("not-perf-sensitive");
    let mut feature = complete_feature("agent.jsonrpc_surface");
    feature.evidence.benchmarks = EvidenceItem::absent();
    assert!(
        registry::validate(&registry_of(vec![feature]), Some(&dir)).is_clean(),
        "a control-plane feature is not performance-sensitive and needs no benchmark"
    );
}

#[test]
fn a_citation_naming_a_file_that_does_not_exist_is_rejected() {
    let dir = scratch("citation");
    let mut feature = complete_feature("core.capability_model");
    feature.evidence.unit_tests = EvidenceItem::verified("benches/does_not_exist.rs");
    fs::write(dir.join("placeholder.txt"), "x").expect("write should succeed");

    let result = registry::validate(&registry_of(vec![feature]), Some(&dir));
    assert!(
        !result.is_clean(),
        "a citation must resolve to something on disk"
    );
    assert!(
        result
            .violations
            .iter()
            .any(|v| v.message.contains("does not exist in the repository")),
        "got: {:?}",
        result.violations
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_citation_naming_a_real_file_is_accepted() {
    let dir = scratch_with_citations("citation-ok");

    let result = registry::validate(
        &registry_of(vec![complete_feature("core.versioning")]),
        Some(&dir),
    );
    assert!(result.is_clean(), "{:?}", result.violations);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn prose_and_method_names_are_not_mistaken_for_paths() {
    // A command citation and an API method name must not be resolved as paths, or a
    // perfectly correct citation would be reported as a missing file.
    assert!(registry::cited_paths("cargo test -p ncui-core — 35 tests").is_empty());
    assert!(registry::cited_paths("ncui.metrics.query").is_empty());
    assert!(registry::cited_paths("--save-baseline pr").is_empty());
    assert_eq!(
        registry::cited_paths("docs/adr/ADR-005-scheduler.md"),
        vec!["docs/adr/ADR-005-scheduler.md"]
    );
    assert_eq!(
        registry::cited_paths("benches/core.rs and docs/adr/ADR-005-scheduler.md"),
        vec!["benches/core.rs", "docs/adr/ADR-005-scheduler.md"],
        "every path in a citation must be extracted, not only the first"
    );
}

#[test]
fn a_feature_id_must_be_a_well_formed_lowercase_identifier() {
    // Every rule keyed on an id — performance sensitivity, required evidence — is
    // bypassed by a spelling the rules do not recognise.
    assert!(registry::is_valid_feature_id("ipc.binary_framing"));
    assert!(!registry::is_valid_feature_id("IPC.binary_framing"));
    assert!(!registry::is_valid_feature_id("binary_framing"));
    assert!(!registry::is_valid_feature_id("ipc..binary"));
    assert!(!registry::is_valid_feature_id(""));
}

#[test]
fn a_case_different_id_cannot_skip_the_benchmark_requirement() {
    // The original evasion: prefix matching was case-sensitive, so `IPC.binary_framing`
    // avoided the performance rule that `ipc.binary_framing` triggers.
    assert!(!registry::is_valid_feature_id("IPC.binary_framing"));
    let mut feature = complete_feature("IPC.binary_framing");
    feature.evidence.benchmarks = EvidenceItem::absent();
    let result = registry::validate(&registry_of(vec![feature]), None);
    assert!(
        !result.is_clean(),
        "a case-different id must not evade the id rule or the benchmark requirement"
    );
}

#[test]
fn a_citation_that_names_more_than_one_path_must_resolve_every_one() {
    // The original evasion: only the first token was checked, so a citation could name one
    // real file and then assert anything.
    let dir = scratch_with_citations("multi-path");
    fs::write(
        dir.join("real.rs"),
        "// real
",
    )
    .expect("write should succeed");

    let mut feature = complete_feature("core.versioning");
    feature.evidence.unit_tests =
        EvidenceItem::verified("real.rs plus crates/ncui-ipc/src/FICTION.rs");
    let result = registry::validate(&registry_of(vec![feature]), Some(&dir));
    assert!(
        !result.is_clean(),
        "a fabricated path after a real one must still be rejected"
    );
}

#[test]
fn a_citation_may_not_point_outside_the_repository() {
    let dir = scratch_with_citations("escape");
    fs::create_dir_all(dir.join("inner")).expect("temp directory");
    let outside = dir.parent().expect("parent").join("outside-evasion.txt");
    fs::write(&outside, "x").expect("write should succeed");

    let mut feature = complete_feature("core.versioning");
    feature.evidence.unit_tests = EvidenceItem::verified("inner/../../../outside-evasion.txt");
    let result = registry::validate(&registry_of(vec![feature]), Some(&dir));
    assert!(
        !result.is_clean(),
        "a citation resolving outside the root must be rejected"
    );

    let _ = fs::remove_file(&outside);
}

#[test]
fn a_feature_may_not_name_a_crate_that_does_not_exist() {
    // The check is scoped to roots that look like a workspace, so the fixture builds one
    // rather than pointing at an arbitrary directory.
    let dir = scratch_with_citations("workspace");
    fs::create_dir_all(dir.join("crates/ncui-core")).expect("temp directory");
    fs::write(
        dir.join("crates/ncui-core/Cargo.toml"),
        "[package]
name = \"ncui-core\"
",
    )
    .expect("write should succeed");

    let mut feature = complete_feature("core.versioning");
    feature.crates = vec!["ncui-core".to_owned()];
    assert!(
        registry::validate(&registry_of(vec![feature.clone()]), Some(&dir)).is_clean(),
        "a real crate must be accepted"
    );

    feature.crates = vec!["totally-made-up-crate".to_owned()];
    let result = registry::validate(&registry_of(vec![feature]), Some(&dir));
    assert!(!result.is_clean(), "a fictional crate must be rejected");
    assert!(
        result
            .violations
            .iter()
            .any(|v| v.message.contains("not a member"))
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_prose_citation_is_not_evidence() {
    // The evasion this closes: with only a non-empty check, "we wrote some tests, trust
    // us" satisfied `unit_tests` and the feature passed with no artifact behind it.
    let dir = scratch_with_citations("prose");
    let mut feature = complete_feature("core.versioning");
    feature.evidence.unit_tests = EvidenceItem::verified("we wrote some tests, trust us");
    let result = registry::validate(&registry_of(vec![feature]), Some(&dir));
    assert!(!result.is_clean(), "prose is not an artifact");
    assert!(
        result
            .violations
            .iter()
            .any(|v| v.message.contains("cites no file")),
        "got: {:?}",
        result.violations
    );
}

#[test]
fn a_path_buried_behind_prose_is_still_checked() {
    let dir = scratch_with_citations("buried");
    let mut feature = complete_feature("core.versioning");
    feature.evidence.unit_tests =
        EvidenceItem::verified("see crates/ncui-ipc/src/FICTION.rs for the tests");
    let result = registry::validate(&registry_of(vec![feature]), Some(&dir));
    assert!(
        !result.is_clean(),
        "a path after a word of prose must still be resolved and rejected"
    );
}

#[test]
fn a_feature_verified_on_no_platform_is_still_rejected() {
    let mut feature = complete_feature("core.error_model");
    feature.platforms = [
        ("linux".to_owned(), PlatformState::Unverified),
        ("windows".to_owned(), PlatformState::Unverified),
        ("macos".to_owned(), PlatformState::Unverified),
    ]
    .into_iter()
    .collect();
    let result = registry::validate(&registry_of(vec![feature]), None);
    assert!(!result.is_clean());
    assert!(
        result
            .violations
            .iter()
            .any(|v| v.message.contains("verified on no platform"))
    );
}

#[test]
fn the_committed_registry_is_valid() {
    // The registry that ships is held to the same rule as any other input.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("protocols/feature-registry.json");
    let raw = fs::read_to_string(&path).expect("the committed registry should exist");
    let parsed: Registry = serde_json::from_str(&raw).expect("the committed registry should parse");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let result = registry::validate(&parsed, Some(&root));
    assert!(
        result.is_clean(),
        "the committed registry must satisfy its own rules: {:?}",
        result.violations
    );
}

#[test]
fn a_new_subsystem_namespace_cannot_escape_the_requirements() {
    // The escape the pre-gate found: the prefix lists were allowlists, so `runtime.*` and
    // `process.*` — which the shipped registry already used — passed with no benchmark and
    // no telemetry. The lists are now opt-outs, so a namespace is covered by default.
    // `process.*` is deliberately exempt and is covered by its own test below; every other
    // namespace, however unfamiliar, must be required to carry the full set.
    for id in [
        "runtime.anything",
        "brand.new.subsystem",
        "x.y",
        "webview.anything",
    ] {
        let required = registry::required_for(id);
        for must in ["benchmarks", "tracing", "metrics"] {
            assert!(
                required.contains(&must),
                "`{id}` must require `{must}`; an unlisted namespace cannot be exempt"
            );
        }
    }
}

#[test]
fn build_time_tooling_is_exempt_because_it_has_no_runtime_to_observe() {
    // The verification tooling runs once, at build time, over source files. Asking it to
    // emit a runtime metric would be coverage theatre, so it is exempt — and the exemption
    // is explicit rather than the result of nobody having listed it.
    for id in ["process.verification_tooling", "process.anything"] {
        let required = registry::required_for(id);
        assert!(
            !required.contains(&"benchmarks")
                && !required.contains(&"tracing")
                && !required.contains(&"metrics"),
            "`{id}` is build-time tooling and is exempt from runtime observability"
        );
    }
}

#[test]
fn only_the_control_plane_is_exempt_from_a_benchmark() {
    // `agent.*` and `cli.*` are control-plane surfaces: their cost is process start-up and
    // I/O, so a benchmark for them would measure the operating system.
    for id in ["agent.jsonrpc_surface", "cli.truth_layer"] {
        assert!(
            !registry::required_for(id).contains(&"benchmarks"),
            "`{id}` is control-plane and is exempt from a benchmark"
        );
    }
    for id in [
        "core.error_model",
        "state.incremental_updates",
        "workers.native_pool",
    ] {
        assert!(
            registry::required_for(id).contains(&"benchmarks"),
            "`{id}` carries real work and must be benchmarked"
        );
    }
}
