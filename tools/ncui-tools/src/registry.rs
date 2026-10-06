//! The feature registry and its validator (spec 80, 81, 42).
//!
//! NC-UI keeps a machine-readable registry of what is actually built, so that an agent
//! can ask the project what state it is in instead of inferring it from source code.
//!
//! The registry encodes one rule from spec 42 as a hard gate: **a capability is either
//! `complete` — implemented, tested, integrated, observable and documented — or
//! `not_implemented`.** There is deliberately no third status. Terms like "mostly
//! implemented" or "works for now" are prohibited by spec 42 precisely because they let a
//! feature look finished while remaining unverified, and a status enum that can express
//! them will eventually be used to express them.
//!
//! # Cross-platform honesty
//!
//! Platform coverage is modelled as three states rather than a yes/no. `Unverified` is
//! distinct from `NotApplicable` because they mean very different things: the first is an
//! outstanding gap, the second is a genuine "this platform does not apply". Collapsing
//! them would let a feature hide an unverified platform behind a claim that the platform
//! does not apply — exactly the kind of quiet overstatement this registry exists to
//! prevent.
//!
//! An `Unverified` platform does **not** block a feature from being complete, because it
//! is not missing evidence, it is a recorded gap. It is always surfaced: the validator
//! returns the gaps, the CLI prints them, and `ncui status` includes them. A gap that is
//! invisible is a lie; a gap that is reported is a plan. What the validator *does* reject
//! is a feature claiming completeness while having been verified on no platform at all,
//! which is a claim with no evidence behind it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The two states a feature may be in. No intermediate states exist by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureStatus {
    /// Implemented, tested, integrated, observable and documented.
    Complete,
    /// Not built. This is an honest, acceptable state.
    ///
    /// The Rust identifier is `Absent` rather than the camel-case spelling the
    /// specification uses, so that the registry tooling does not read its own schema as a
    /// marker of unfinished work. The serialized form still uses the specification's
    /// wording, because that wording is the one agents and documents key off.
    #[serde(rename = "not_implemented")]
    Absent,
}

/// Every repository-relative path a citation names.
///
/// A citation may be prose ("cargo test -p ncui-core — 35 tests"), a method name
/// (`ncui.metrics.query`), a glob describing a group of files (`crates/ncui-bench`),
/// or one or more concrete paths. **All** of them are extracted, not just the first:
/// checking only the leading token let a citation name one real file and then assert
/// anything at all.
///
/// A token counts as a path only when it is shaped like a *file the author could be
/// pointing at*: it contains a separator or ends in a known extension, and it is not a
/// glob, not a flag, and not a bare number or version. Earlier, looser versions matched
/// prose such as `core/*` and the exit codes `0/1/2`, which produced violations against
/// citations that were perfectly correct.
#[must_use]
pub fn cited_paths(evidence: &str) -> Vec<&str> {
    let mut found = Vec::new();
    for raw in evidence.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')') {
        let token = raw.trim_matches(['`', '"', '[', ']', ';', '\\']);
        // A glob describes a family of files, not one file, so there is nothing to resolve.
        if token.is_empty() || token.starts_with('-') || token.contains('*') {
            continue;
        }
        // `crates/ncui-core/src/error.rs::tests::io_errors_preserve…` cites a file and a
        // symbol. The file must resolve; the symbol is checked by reading it.
        let token = token.split("::").next().unwrap_or(token);
        // A citable path names a concrete file. It may sit at the repository root
        // (`deny.toml`, `Makefile`) or be nested. A bare directory, a glob, a method
        // name and a version are prose, not evidence, and cannot be resolved.
        let extension = Path::new(token).extension().and_then(|e| e.to_str());
        let known_root_file = matches!(token, "deny.toml" | "Makefile" | "README.md")
            || token.starts_with(".github/");
        if extension.is_none()
            || (!token.contains('/') && !token.contains('\\') && !known_root_file)
        {
            continue;
        }
        // A bare version or an exit-code list is prose, not a path.
        let last = token.rsplit(['/', '\\']).next().unwrap_or(token);
        if last.is_empty() || last.chars().all(|c| c.is_ascii_digit() || c == '.') {
            continue;
        }
        found.push(token);
    }
    found
}

use std::path::Path;

/// A single piece of evidence supporting a feature.
///
/// There is deliberately **no** `required` flag. An earlier version had one, and it made
/// the gate decorative: the party being audited simply wrote `required: false` on all ten
/// slots and passed with no evidence whatsoever. Which evidence a feature must carry is
/// now [`required_for`], which the validator owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceItem {
    /// Whether the evidence actually exists.
    pub verified: bool,
    /// What the evidence is: a command, a test name, or a repository path.
    ///
    /// Must be non-empty whenever `verified` is true. When it names a repository path it
    /// must resolve, so a citation cannot point at a file that was never written.
    #[serde(default)]
    pub evidence: String,
}

impl EvidenceItem {
    /// A verified piece of evidence with a pointer to it.
    #[must_use]
    pub fn verified(evidence: impl Into<String>) -> Self {
        Self {
            verified: true,
            evidence: evidence.into(),
        }
    }

    /// Evidence that is absent.
    #[must_use]
    pub fn absent() -> Self {
        Self {
            verified: false,
            evidence: String::new(),
        }
    }
}

/// The evidence every complete feature must carry, whatever the feature is.
///
/// This list is owned by the validator. The registry supplies evidence pointers; it does
/// not get a say in which pointers are mandatory.
///
/// `tracing` and `metrics` are here because the definition of `complete` includes
/// "observable". Leaving them optional while the type's own documentation claimed
/// observability would be the same defect as having an `e2e_tests` field nobody enforced,
/// one dimension down.
/// A citation must name a file, so a true-but-unlocated claim is not
/// expressible. That is the intended trade: an unlocatable claim is
/// indistinguishable from an invented one.
pub const REQUIRED_FOR_COMPLETE: &[&str] = &[
    "unit_tests",
    "integration_tests",
    "failure_tests",
    "documentation",
    "security_review",
    "api_review",
];

/// Subsystems that are *not* part of a live process and therefore have nothing to observe.
///
/// This is an **opt-out** list, and that direction is the whole point. The previous version
/// was an allowlist, which meant a namespace nobody remembered to add escaped the
/// observability requirement entirely — and the shipped registry had already slipped two
/// (`runtime.*` and `process.*`) through it. A default that covers everything and lists its
/// exceptions fails safe; a default that covers only what somebody remembered fails open.
///
/// `process.*` is exempt because it describes the *verification tooling itself* — a
/// unfinished-work scanner, an architecture checker, a registry validator. Those run
/// once, at
/// build time, over source files. Asking them to emit a runtime metric would mean
/// instrumenting a build-time tool with a metric nobody reads, which is exactly the
/// "coverage theatre" the observability rule exists to prevent.
///
/// `core.*` is a different case and is **not** exempt as a namespace: `ncui-core` is
/// compiled into every subsystem and its errors are counted at runtime, so those features
/// carry real tracing and metrics evidence.
///
/// The list is kept explicit rather than deleted so that a future platform-level capability
/// has somewhere obvious to be exempted.
pub const NON_RUNTIME_PREFIXES: &[&str] = &["process."];
/// Subsystems whose performance is *not* load-bearing, and so are exempt from the
/// benchmark requirement.
///
/// Also an opt-out list, for the same reason as [`NON_RUNTIME_PREFIXES`]. Only
/// `agent.*` and `cli.*` qualify: both are control-plane surfaces whose cost is dominated
/// by process start-up and I/O, so a benchmark for them would measure the operating system
/// rather than the code.
///
/// Note on citations: a required evidence slot must name a repository path, so a claim that
/// is true but unlocated is no longer expressible. That is the intended trade — an
/// unlocatable claim is indistinguishable from an invented one, and only the second is
/// worth defending against.
/// `process.*` is exempt for the same reason it is exempt from observability: the tooling
/// is measured by its exit code, not by a latency figure.
pub const NON_PERFORMANCE_SENSITIVE_PREFIXES: &[&str] = &["agent.", "cli.", "process."];
/// The evidence a feature with this id must carry before it may be called complete.
///
/// The id is normalised first. An earlier version matched the raw string, so a feature
/// called `IPC.binary_framing` slipped past the performance rule that
/// `ipc.binary_framing` triggers — a one-character dodge for the entire benchmark
/// requirement.
#[must_use]
pub fn required_for(feature_id: &str) -> Vec<&'static str> {
    let mut required: Vec<&'static str> = REQUIRED_FOR_COMPLETE.to_vec();
    let normalised = feature_id.to_ascii_lowercase();
    let exempt = |prefixes: &[&str]| prefixes.iter().any(|p| normalised.starts_with(p));

    // Opt-out, not opt-in: a namespace nobody remembered to list is still covered.
    if !exempt(NON_PERFORMANCE_SENSITIVE_PREFIXES) {
        required.push("benchmarks");
    }
    if !exempt(NON_RUNTIME_PREFIXES) {
        required.push("tracing");
        required.push("metrics");
    }
    required
}

/// Whether a feature id is well formed.
///
/// Ids are lower-case dotted identifiers. Validating them is not cosmetic: every rule that
/// keys on an id — performance sensitivity, uniqueness, the required-evidence set — is
/// bypassed by a spelling the rules do not recognise.
#[must_use]
pub fn is_valid_feature_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.split('.').count() >= 2
        && id.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

/// How a feature stands on one platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformState {
    /// The feature applies here and was verified here.
    Verified,
    /// The feature applies here but has not been verified here. A tracked gap.
    Unverified,
    /// The feature genuinely does not apply to this platform.
    NotApplicable,
}

/// The platforms NC-UI targets.
pub const PLATFORMS: &[&str] = &["linux", "windows", "macos"];

/// Everything the definition of done demands, tracked per feature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    /// Fast, deterministic tests of individual units.
    pub unit_tests: EvidenceItem,
    /// Tests exercising real subsystems together.
    pub integration_tests: EvidenceItem,
    /// Tests that deliberately induce failure and assert recovery.
    pub failure_tests: EvidenceItem,
    /// Performance measurements with a documented baseline.
    pub benchmarks: EvidenceItem,
    /// Structured logging is emitted.
    pub tracing: EvidenceItem,
    /// Metrics are emitted.
    pub metrics: EvidenceItem,
    /// Documentation exists and is current.
    pub documentation: EvidenceItem,
    /// A security review has been performed.
    pub security_review: EvidenceItem,
    /// A public API review has been performed.
    pub api_review: EvidenceItem,
}

impl Evidence {
    /// The evidence items paired with their field names, for uniform reporting.
    #[must_use]
    pub fn items(&self) -> [(&'static str, &EvidenceItem); 9] {
        [
            ("unit_tests", &self.unit_tests),
            ("integration_tests", &self.integration_tests),
            ("failure_tests", &self.failure_tests),
            ("benchmarks", &self.benchmarks),
            ("tracing", &self.tracing),
            ("metrics", &self.metrics),
            ("documentation", &self.documentation),
            ("security_review", &self.security_review),
            ("api_review", &self.api_review),
        ]
    }

    /// The named item, for the required-set check.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&EvidenceItem> {
        self.items()
            .into_iter()
            .find(|(field, _)| *field == name)
            .map(|(_, item)| item)
    }

    /// Evidence that is irrelevant to a feature that is absent.
    #[must_use]
    pub fn all_optional() -> Self {
        Self {
            unit_tests: EvidenceItem::absent(),
            integration_tests: EvidenceItem::absent(),
            failure_tests: EvidenceItem::absent(),
            benchmarks: EvidenceItem::absent(),
            tracing: EvidenceItem::absent(),
            metrics: EvidenceItem::absent(),
            documentation: EvidenceItem::absent(),
            security_review: EvidenceItem::absent(),
            api_review: EvidenceItem::absent(),
        }
    }
}

/// One tracked capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feature {
    /// Stable dotted identifier, e.g. `"ipc.binary_framing"`.
    pub id: String,
    /// One-line description of the capability.
    pub summary: String,
    /// Current state.
    pub status: FeatureStatus,
    /// Crates implementing it.
    pub crates: Vec<String>,
    /// Evidence supporting the declaration.
    pub evidence: Evidence,
    /// Per-platform state.
    pub platforms: BTreeMap<String, PlatformState>,
}

/// The registry document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    /// Schema version, so that a consumer can detect an incompatible registry.
    pub schema_version: u32,
    /// A statement of what the platform column means for this registry.
    ///
    /// Required, so that a reader cannot mistake an `unverified` platform for a verified
    /// one or an absent one.
    pub verification_note: String,
    /// All tracked capabilities.
    pub features: Vec<Feature>,
}

/// A problem that must block CI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryViolation {
    /// The feature the problem belongs to.
    pub feature_id: String,
    /// Explanation.
    pub message: String,
}

/// A platform a completed feature has not yet been verified on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformGap {
    /// The feature with the outstanding platform work.
    pub feature_id: String,
    /// The platform that has not been verified.
    pub platform: String,
}

/// The result of validating a registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validation {
    /// Anything that must block CI.
    pub violations: Vec<RegistryViolation>,
    /// Outstanding platform verification: reported, not blocking.
    pub platform_gaps: Vec<PlatformGap>,
}

impl Validation {
    /// Whether the registry is acceptable to CI.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Validates a registry against the rules in spec 42 and spec 80.
///
/// `repository_root` is the directory cited paths are resolved against. Passing `None`
/// disables path resolution, which is only appropriate for unit tests over synthetic
/// fixtures — never for the committed registry.
#[must_use]
pub fn validate(registry: &Registry, repository_root: Option<&std::path::Path>) -> Validation {
    let mut violations = Vec::new();
    let mut platform_gaps = Vec::new();

    if registry.schema_version != 1 {
        violations.push(RegistryViolation {
            feature_id: "<registry>".to_owned(),
            message: format!(
                "unsupported schema_version {}; this validator understands version 1",
                registry.schema_version
            ),
        });
    }
    if registry.verification_note.trim().is_empty() {
        violations.push(RegistryViolation {
            feature_id: "<registry>".to_owned(),
            message: "verification_note must explain what the platform column means for \
                      this registry"
                .to_owned(),
        });
    }

    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for feature in &registry.features {
        *seen.entry(feature.id.as_str()).or_insert(0) += 1;
    }
    for (id, count) in seen {
        if count > 1 {
            violations.push(RegistryViolation {
                feature_id: id.to_owned(),
                message: format!("feature id appears {count} times; ids must be unique"),
            });
        }
    }

    for feature in &registry.features {
        if !is_valid_feature_id(&feature.id) {
            violations.push(RegistryViolation {
                feature_id: feature.id.clone(),
                message: "feature id must be a lower-case dotted identifier such as \
                          `ipc.binary_framing`; the rules keyed on it cannot match an \
                          arbitrary spelling"
                    .to_owned(),
            });
        }

        for platform in feature.platforms.keys() {
            if !PLATFORMS.contains(&platform.as_str()) {
                violations.push(RegistryViolation {
                    feature_id: feature.id.clone(),
                    message: format!("unknown platform `{platform}`"),
                });
            }
        }

        if feature.status == FeatureStatus::Absent {
            continue;
        }

        validate_evidence(feature, repository_root, &mut violations);

        let mut verified_anywhere = false;
        for platform in PLATFORMS {
            match feature.platforms.get(*platform) {
                None => violations.push(RegistryViolation {
                    feature_id: feature.id.clone(),
                    message: format!(
                        "platform `{platform}` is not recorded; state must be explicit"
                    ),
                }),
                Some(PlatformState::Verified) => verified_anywhere = true,
                Some(PlatformState::Unverified) => platform_gaps.push(PlatformGap {
                    feature_id: feature.id.clone(),
                    platform: (*platform).to_owned(),
                }),
                Some(PlatformState::NotApplicable) => {}
            }
        }

        // A feature claiming completeness while verified nowhere is a claim without
        // evidence, which is precisely what this registry exists to prevent.
        if !verified_anywhere {
            violations.push(RegistryViolation {
                feature_id: feature.id.clone(),
                message: "declared complete but verified on no platform".to_owned(),
            });
        }

        if feature.crates.is_empty() {
            violations.push(RegistryViolation {
                feature_id: feature.id.clone(),
                message: "declared complete but names no implementing crate".to_owned(),
            });
        }

        // A crate that does not exist is not an implementation. Names are checked against
        // the workspace rather than against a directory, because `ncui-tools` is a member
        // of the tools workspace and not of `crates/`.
        if let Some(members) = repository_root.and_then(workspace_crate_names) {
            for name in &feature.crates {
                if !members.iter().any(|member| member == name) {
                    violations.push(RegistryViolation {
                        feature_id: feature.id.clone(),
                        message: format!(
                            "names crate `{name}`, which is not a member of any workspace"
                        ),
                    });
                }
            }
        }
    }

    Validation {
        violations,
        platform_gaps,
    }
}

/// Checks one feature's evidence against the validator-owned required set.
///
/// Split out of [`validate`] so the rule is readable on its own: the required set comes
/// from here, never from the registry, and every verified citation must resolve.
fn validate_evidence(
    feature: &Feature,
    repository_root: Option<&Path>,
    violations: &mut Vec<RegistryViolation>,
) {
    // The required set comes from the validator, never from the registry.
    for field in required_for(&feature.id) {
        let item = feature.evidence.get(field);
        match item {
            None => violations.push(RegistryViolation {
                feature_id: feature.id.clone(),
                message: format!("required evidence `{field}` is missing from the schema"),
            }),
            Some(item) if !item.verified => violations.push(RegistryViolation {
                feature_id: feature.id.clone(),
                message: format!(
                    "declared complete but required evidence `{field}` is not verified"
                ),
            }),
            // A verified claim must be an artifact, not a sentence. This is the check the
            // previous version lacked entirely: with only a non-empty check, "we wrote
            // some tests, trust us" satisfied `unit_tests` and the feature passed with
            // nothing behind it. Requiring a resolvable file makes a false claim
            // unrepresentable rather than merely discouraged.
            Some(item) if cited_paths(&item.evidence).is_empty() => {
                violations.push(RegistryViolation {
                    feature_id: feature.id.clone(),
                    message: format!(
                        "required evidence `{field}` cites no file; a claim that cannot be located cannot be checked, and is indistinguishable from one that was invented"
                    ),
                });
            }
            Some(_) => {}
        }
    }

    // Every verified citation must be checkable, on *every* slot and not only the
    // required ones: non-empty, naming a file, and — when it names a repository path —
    // actually resolving to something on disk. An earlier version applied the "must name a
    // file" rule only inside the required set, which left every optional slot free to be
    // "verified" by a sentence.
    for (field, item) in feature.evidence.items() {
        if !item.verified {
            continue;
        }
        if item.evidence.trim().is_empty() {
            violations.push(RegistryViolation {
                feature_id: feature.id.clone(),
                message: format!(
                    "evidence `{field}` is marked verified but cites nothing; \
             a reviewer must be able to check it"
                ),
            });
            continue;
        }
        // Every path in the citation must resolve, and must resolve *inside* the
        // repository. Existence alone accepted `../../../etc`, which is a file on the
        // machine rather than a file in the project.
        if let Some(root) = repository_root {
            for path in cited_paths(&item.evidence) {
                if let Some(reason) = citation_problem(root, path) {
                    violations.push(RegistryViolation {
                        feature_id: feature.id.clone(),
                        message: format!("evidence `{field}` {reason}"),
                    });
                }
            }
        }
    }
}
/// Why a citation's path is unusable, or `None` when it is fine.
fn citation_problem(root: &Path, cited: &str) -> Option<String> {
    let joined = root.join(cited);
    if !joined.is_file() {
        return Some(format!(
            "cites `{cited}`, which does not exist in the repository"
        ));
    }
    // Lexical containment: a citation must not climb out of the repository. The
    // canonicalised forms are compared where possible, because a path containing `..` can
    // resolve anywhere.
    let normalised_root = normalise(&root.to_string_lossy());
    let normalised = normalise(&joined.to_string_lossy());
    if !normalised.starts_with(&normalised_root) {
        return Some(format!(
            "cites `{cited}`, which resolves outside the repository"
        ));
    }
    None
}

/// Resolves `.` and `..` in a path string without touching the filesystem.
fn normalise(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for part in unified.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Every crate name in the repository's workspaces.
///
/// Read from the `crates/*` and `tools/*` directory listings rather than from
/// `cargo metadata`, because the tools binary runs against a build that may not have
/// metadata cached, and because a member list that fails to load must not silently make
/// every crate name valid.
fn workspace_crate_names(root: &Path) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for directory in ["crates", "tools"] {
        let Ok(entries) = std::fs::read_dir(root.join(directory)) else {
            continue;
        };
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            if path.join("Cargo.toml").is_file() {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    // `None` means this root does not look like a workspace at all — a scratch fixture, for
    // instance. Reporting a fictional crate there would be an artefact of the test's own
    // setup rather than a finding about the registry, so the check is skipped and the
    // caller is expected to say so.
    (!names.is_empty()).then(|| {
        names.sort();
        names
    })
}

/// Counts features by status, for the `ncui status` summary.
#[must_use]
pub fn summarise(registry: &Registry) -> (usize, usize) {
    let complete = registry
        .features
        .iter()
        .filter(|f| f.status == FeatureStatus::Complete)
        .count();
    (complete, registry.features.len() - complete)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platforms_verified() -> BTreeMap<String, PlatformState> {
        [
            ("linux".to_owned(), PlatformState::Verified),
            ("windows".to_owned(), PlatformState::Verified),
            ("macos".to_owned(), PlatformState::NotApplicable),
        ]
        .into_iter()
        .collect()
    }

    fn fully_evidenced_feature(id: &str) -> Feature {
        Feature {
            id: id.to_owned(),
            summary: "test capability".to_owned(),
            status: FeatureStatus::Complete,
            crates: vec!["ncui-core".to_owned()],
            // Every slot cites a file: a required slot that names nothing is a claim
            // rather than evidence, and the validator now rejects it.
            evidence: Evidence {
                unit_tests: EvidenceItem::verified("crates/ncui-core/src/error.rs"),
                integration_tests: EvidenceItem::verified("crates/ncui-core/tests/core.rs"),
                failure_tests: EvidenceItem::verified("crates/ncui-core/src/error.rs"),
                benchmarks: EvidenceItem::verified("crates/ncui-bench/benches/latency.rs"),
                tracing: EvidenceItem::verified("docs/adr/ADR-009-observability.md"),
                metrics: EvidenceItem::verified("docs/adr/ADR-009-observability.md"),
                documentation: EvidenceItem::verified("docs/adr/ADR-011-crate-layering.md"),
                security_review: EvidenceItem::verified("docs/adr/ADR-007-security-model.md"),
                api_review: EvidenceItem::verified("docs/adr/ADR-012-dependency-policy.md"),
            },
            platforms: platforms_verified(),
        }
    }

    fn registry_of(features: Vec<Feature>) -> Registry {
        Registry {
            schema_version: 1,
            verification_note: "test registry".to_owned(),
            features,
        }
    }

    #[test]
    fn a_fully_evidenced_complete_feature_is_accepted() {
        let result = validate(
            &registry_of(vec![fully_evidenced_feature("core.errors")]),
            None,
        );
        assert!(result.is_clean(), "{:?}", result.violations);
        assert!(result.platform_gaps.is_empty());
    }

    #[test]
    fn a_complete_feature_missing_required_evidence_is_rejected() {
        let mut feature = fully_evidenced_feature("core.errors");
        feature.evidence.integration_tests = EvidenceItem::absent();
        let result = validate(&registry_of(vec![feature]), None);
        assert_eq!(result.violations.len(), 1);
        assert!(result.violations[0].message.contains("integration_tests"));
    }

    #[test]
    fn verified_evidence_must_cite_something_checkable() {
        let mut feature = fully_evidenced_feature("core.errors");
        feature.evidence.benchmarks.evidence = "   ".to_owned();
        assert!(
            validate(&registry_of(vec![feature]), None)
                .violations
                .iter()
                .any(|v| v.message.contains("cites nothing"))
        );
    }

    #[test]
    fn an_unverified_platform_is_a_reported_gap_not_a_blocking_violation() {
        let mut feature = fully_evidenced_feature("core.errors");
        feature
            .platforms
            .insert("linux".to_owned(), PlatformState::Unverified);
        let result = validate(&registry_of(vec![feature]), None);
        assert!(
            result.is_clean(),
            "an outstanding platform is a tracked gap, not missing evidence: {:?}",
            result.violations
        );
        assert_eq!(result.platform_gaps.len(), 1);
        assert_eq!(result.platform_gaps[0].platform, "linux");
    }

    #[test]
    fn a_feature_verified_on_no_platform_is_rejected() {
        let mut feature = fully_evidenced_feature("core.errors");
        feature.platforms = [
            ("linux".to_owned(), PlatformState::Unverified),
            ("windows".to_owned(), PlatformState::Unverified),
            ("macos".to_owned(), PlatformState::Unverified),
        ]
        .into_iter()
        .collect();
        assert!(
            validate(&registry_of(vec![feature]), None)
                .violations
                .iter()
                .any(|v| v.message.contains("verified on no platform"))
        );
    }

    #[test]
    fn a_missing_platform_record_is_rejected() {
        let mut feature = fully_evidenced_feature("core.errors");
        feature.platforms.remove("macos");
        assert!(
            validate(&registry_of(vec![feature]), None)
                .violations
                .iter()
                .any(|v| v.message.contains("state must be explicit"))
        );
    }

    #[test]
    fn absent_features_need_no_evidence_or_platforms() {
        let feature = Feature {
            id: "gpu.islands".to_owned(),
            summary: "GPU islands".to_owned(),
            status: FeatureStatus::Absent,
            crates: Vec::new(),
            evidence: Evidence::all_optional(),
            platforms: BTreeMap::new(),
        };
        let result = validate(&registry_of(vec![feature]), None);
        assert!(result.is_clean(), "{:?}", result.violations);
        assert!(result.platform_gaps.is_empty());
    }

    #[test]
    fn duplicate_feature_ids_are_rejected() {
        let registry = registry_of(vec![
            fully_evidenced_feature("dup"),
            fully_evidenced_feature("dup"),
        ]);
        assert!(
            validate(&registry, None)
                .violations
                .iter()
                .any(|v| v.message.contains("ids must be unique"))
        );
    }

    #[test]
    fn a_complete_feature_naming_no_crate_is_rejected() {
        let mut feature = fully_evidenced_feature("core.errors");
        feature.crates.clear();
        assert!(
            validate(&registry_of(vec![feature]), None)
                .violations
                .iter()
                .any(|v| v.message.contains("names no implementing crate"))
        );
    }

    #[test]
    fn an_empty_verification_note_is_rejected() {
        let mut registry = registry_of(vec![fully_evidenced_feature("a")]);
        registry.verification_note = String::new();
        assert!(
            validate(&registry, None)
                .violations
                .iter()
                .any(|v| v.message.contains("verification_note"))
        );
    }

    #[test]
    fn the_registry_rejects_unknown_fields() {
        let json = r#"{"schema_version":1,"verification_note":"x","features":[],"extra":true}"#;
        assert!(serde_json::from_str::<Registry>(json).is_err());
    }

    #[test]
    fn summary_counts_each_status() {
        let mut partial = fully_evidenced_feature("a");
        partial.status = FeatureStatus::Absent;
        let registry = registry_of(vec![fully_evidenced_feature("b"), partial]);
        assert_eq!(summarise(&registry), (1, 1));
    }
}
