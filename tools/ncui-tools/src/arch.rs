//! The architecture boundary checker (spec 46).
//!
//! NC-UI's layering is the mechanism that keeps the runtime from collapsing into a
//! dependency tangle. The intended order, lowest first:
//!
//! ```text
//! ncui-core          (0)  vocabulary: errors, ids, capabilities, limits, versions
//! ncui-observability (1)  ─┐
//! ncui-security      (1)   │  independent subsystems built directly on core
//! ncui-ipc           (1)   │
//! ncui-state         (1)   │
//! ncui-scheduler     (1)   │
//! ncui-a11y          (1)  ─┤
//! ncui-abi           (1)  ─┘
//! ncui-workers       (2)  worker runtime: needs the scheduler and observability
//! ncui-webview       (2)  webview host policy
//! ncui-gpu           (2)  compositing boundary's decision logic
//! ncui-runtime       (3)  composition: wires every subsystem together
//! ncui-cli           (4)  user-facing entry point
//! ncui-bench         (4)  measurement harness
//! ```
//!
//! [`LAYERS`] is the authoritative table; this diagram is a reading of it. `docs/adr/ADR-011`
//! carries the same table and the reasoning behind it.
//!
//! A crate may depend on any crate at a strictly lower layer, never on the same layer
//! and never on a higher one. That single rule prevents most cycles by construction, but
//! the checker also runs an explicit cycle search so that a hand-added cycle is reported
//! as a cycle rather than as a confusing layer complaint.
//!
//! The graph is read from `cargo metadata` rather than by parsing manifests, so it
//! reflects what Cargo will actually build, including workspace inheritance.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

/// The declared layer of each internal crate.
///
/// Adding a crate without adding it here is an error, not a warning: an unlayered crate
/// would silently bypass every boundary rule.
pub const LAYERS: &[(&str, u8)] = &[
    ("ncui-core", 0),
    ("ncui-observability", 1),
    ("ncui-security", 1),
    ("ncui-ipc", 1),
    ("ncui-state", 1),
    ("ncui-a11y", 1),
    ("ncui-abi", 1),
    ("ncui-scheduler", 1),
    ("ncui-workers", 2),
    ("ncui-webview", 2),
    ("ncui-gpu", 2),
    ("ncui-webview-wry", 3),
    ("ncui-runtime", 3),
    ("ncui-cli", 4),
    ("ncui-bench", 4),
];

/// One internal crate and the internal crates it depends on.
#[derive(Debug, Clone)]
pub struct CrateInfo {
    /// Crate name as Cargo knows it.
    pub name: String,
    /// Manifest path.
    pub manifest: String,
    /// Direct dependencies on other `ncui-*` crates.
    pub internal_deps: Vec<String>,
}

/// A rule violation or structural defect found in the graph.
#[derive(Debug, Clone)]
pub struct Violation {
    /// The crate the problem is attributed to.
    pub crate_name: String,
    /// The offending dependency, when the problem involves one.
    pub dependency: Option<String>,
    /// Human-readable explanation.
    pub message: String,
}

/// The result of an architecture check.
#[derive(Debug, Clone)]
pub struct ArchReport {
    /// Every internal crate discovered, sorted.
    pub crates: Vec<CrateInfo>,
    /// Everything that is wrong with the graph.
    pub violations: Vec<Violation>,
}

impl ArchReport {
    /// Whether the graph satisfies every architectural rule.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

fn layer_of(name: &str) -> Option<u8> {
    LAYERS.iter().find(|(n, _)| *n == name).map(|(_, l)| *l)
}

/// Reads the workspace graph by invoking `cargo metadata`.
///
/// # Errors
///
/// Returns a readable message when Cargo cannot be run, exits non-zero, or emits
/// output that is not valid metadata. A checker that silently reported "clean" when
/// it could not read anything would be worse than useless, so every failure to read is
/// an error rather than an empty graph.
pub fn load_crates(workspace_root: &Path) -> Result<Vec<CrateInfo>, String> {
    let output = Command::new(cargo_binary())
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(workspace_root)
        .output()
        .map_err(|e| format!("failed to execute cargo metadata: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("unparsable metadata: {e}"))?;

    let packages = json
        .get("packages")
        .and_then(|p| p.as_array())
        .ok_or_else(|| "cargo metadata contained no packages array".to_owned())?;

    let mut crates = Vec::new();
    for package in packages {
        let Some(name) = package.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        if !name.starts_with("ncui-") {
            continue;
        }
        let manifest = package
            .get("manifest_path")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_owned();

        let mut internal_deps = Vec::new();
        if let Some(deps) = package.get("dependencies").and_then(|d| d.as_array()) {
            for dep in deps {
                let dep_name = dep.get("name").and_then(|n| n.as_str()).unwrap_or("");
                if dep_name.starts_with("ncui-") && dep_name != name {
                    internal_deps.push(dep_name.to_owned());
                }
            }
        }
        internal_deps.sort();

        crates.push(CrateInfo {
            name: name.to_owned(),
            manifest,
            internal_deps,
        });
    }
    crates.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(crates)
}

/// Resolves the Cargo executable, preferring `CARGO` so the checker uses the same
/// toolchain that invoked it.
fn cargo_binary() -> std::ffi::OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

/// Applies every architectural rule to a loaded graph.
#[must_use]
pub fn check(crates: &[CrateInfo]) -> ArchReport {
    let mut violations = Vec::new();

    for info in crates {
        let Some(layer) = layer_of(&info.name) else {
            violations.push(Violation {
                crate_name: info.name.clone(),
                dependency: None,
                message: "crate is not declared in the architecture layer table; \
                          add it to LAYERS with an explicit layer"
                    .to_owned(),
            });
            continue;
        };

        for dep in &info.internal_deps {
            // The loader already excludes self-references, and Cargo never reports
            // one; skipping here keeps the checker's contract identical to the
            // loader's so the two cannot disagree.
            if dep == &info.name {
                continue;
            }
            match layer_of(dep) {
                None => violations.push(Violation {
                    crate_name: info.name.clone(),
                    dependency: Some(dep.clone()),
                    message: format!(
                        "depends on `{dep}`, which is not declared in the layer table"
                    ),
                }),
                Some(dep_layer) if dep_layer >= layer => violations.push(Violation {
                    crate_name: info.name.clone(),
                    dependency: Some(dep.clone()),
                    message: format!(
                        "layer {layer} depends on `{dep}` at layer {dep_layer}; \
                         a crate may only depend on strictly lower layers"
                    ),
                }),
                Some(_) => {}
            }
        }
    }

    for cycle in find_cycles(crates) {
        violations.push(Violation {
            crate_name: cycle.join(" -> "),
            dependency: None,
            message: "dependency cycle detected".to_owned(),
        });
    }

    ArchReport {
        crates: crates.to_vec(),
        violations,
    }
}

/// Finds cycles in the internal dependency graph using an iterative depth-first search.
fn find_cycles(crates: &[CrateInfo]) -> Vec<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Unvisited,
        InProgress,
        Done,
    }

    let graph: BTreeMap<&str, Vec<&str>> = crates
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.internal_deps
                    .iter()
                    .map(String::as_str)
                    .filter(|d| **d != c.name && crates.iter().any(|t| t.name == **d))
                    .collect(),
            )
        })
        .collect();

    let mut marks: BTreeMap<&str, Mark> = graph.keys().map(|k| (*k, Mark::Unvisited)).collect();
    let mut cycles: BTreeSet<Vec<String>> = BTreeSet::new();

    // Iterative DFS with an explicit stack of (node, next dependency index) so that a
    // deep graph cannot overflow the call stack.
    for start in graph.keys() {
        if marks.get(start) != Some(&Mark::Unvisited) {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        let mut path: Vec<&str> = vec![start];
        marks.insert(start, Mark::InProgress);

        while let Some((node, index)) = stack.pop() {
            let neighbours = graph.get(node).map(Vec::as_slice).unwrap_or_default();
            if index < neighbours.len() {
                stack.push((node, index + 1));
                let next = neighbours[index];
                match marks.get(next) {
                    Some(Mark::Done) => {}
                    Some(Mark::InProgress) => {
                        // Found a back edge: extract the cycle from the current path.
                        if let Some(start_index) = path.iter().position(|n| *n == next) {
                            let mut cycle: Vec<String> = path[start_index..]
                                .iter()
                                .map(|s| (*s).to_owned())
                                .collect();
                            cycle.push(next.to_owned());
                            cycles.insert(cycle);
                        }
                    }
                    _ => {
                        marks.insert(next, Mark::InProgress);
                        path.push(next);
                        stack.push((next, 0));
                    }
                }
            } else {
                marks.insert(node, Mark::Done);
                if path.last() == Some(&node) {
                    path.pop();
                }
            }
        }
    }

    cycles.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, deps: &[&str]) -> CrateInfo {
        CrateInfo {
            name: name.to_owned(),
            manifest: format!("{name}/Cargo.toml"),
            internal_deps: deps.iter().map(|d| (*d).to_owned()).collect(),
        }
    }

    fn healthy_graph() -> Vec<CrateInfo> {
        vec![
            info("ncui-core", &[]),
            info("ncui-ipc", &["ncui-core"]),
            info("ncui-scheduler", &["ncui-core"]),
            info("ncui-observability", &["ncui-core"]),
            info(
                "ncui-workers",
                &["ncui-core", "ncui-scheduler", "ncui-observability"],
            ),
            info("ncui-runtime", &["ncui-core", "ncui-ipc", "ncui-scheduler"]),
            info("ncui-cli", &["ncui-core", "ncui-runtime"]),
        ]
    }

    #[test]
    fn a_correctly_layered_graph_is_clean() {
        assert!(check(&healthy_graph()).is_clean());
    }

    #[test]
    fn a_backward_dependency_is_rejected() {
        let mut graph = healthy_graph();
        // ncui-core must not depend on ncui-ipc: that would invert the layering.
        graph[0].internal_deps.push("ncui-ipc".to_owned());
        let report = check(&graph);
        assert!(!report.is_clean());
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.dependency.as_deref() == Some("ncui-ipc")),
            "expected an inverted dependency to be reported"
        );
    }

    #[test]
    fn same_layer_dependencies_are_rejected() {
        // ncui-ipc and ncui-state are both layer 1 and must not depend on each other.
        let graph = vec![
            info("ncui-core", &[]),
            info("ncui-ipc", &["ncui-core", "ncui-state"]),
            info("ncui-state", &["ncui-core"]),
        ];
        let report = check(&graph);
        assert!(!report.is_clean());
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.message.contains("strictly lower layers"))
        );
    }

    #[test]
    fn an_unlayered_crate_is_rejected() {
        let graph = vec![
            info("ncui-core", &[]),
            info("ncui-experimental", &["ncui-core"]),
        ];
        let report = check(&graph);
        assert!(!report.is_clean());
        assert!(report.violations.iter().any(|v| {
            v.message
                .contains("not declared in the architecture layer table")
        }));
    }

    #[test]
    fn cycles_are_detected() {
        let graph = vec![
            info("ncui-core", &[]),
            info("ncui-ipc", &["ncui-state"]),
            info("ncui-state", &["ncui-ipc"]),
        ];
        let cycles = find_cycles(&graph);
        assert!(
            !cycles.is_empty(),
            "expected the ipc <-> state cycle to be found"
        );
    }

    #[test]
    fn check_surfaces_a_cycle_as_a_violation_rather_than_a_layer_complaint() {
        // ADR-011: "a hand-introduced cycle is reported as a cycle rather than as a confusing
        // layer complaint". Testing `find_cycles` alone does not establish that — `check` is
        // the entry point `ncui-arch` calls, and its cycle branch was reachable from no test.
        let graph = vec![
            info("ncui-core", &[]),
            info("ncui-ipc", &["ncui-state"]),
            info("ncui-state", &["ncui-ipc"]),
        ];
        let report = check(&graph);
        assert!(!report.is_clean(), "a graph with a cycle is not clean");
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.message.contains("dependency cycle detected")),
            "check must report the cycle by name, got {:?}",
            report.violations
        );
        assert!(
            report.violations.iter().any(|v| {
                v.crate_name.contains("ncui-ipc") && v.crate_name.contains("ncui-state")
            }),
            "the cycle violation must name the crates involved, got {:?}",
            report.violations
        );
    }

    #[test]
    fn an_acyclic_diamond_is_not_reported_as_a_cycle() {
        assert!(find_cycles(&healthy_graph()).is_empty());
    }

    #[test]
    fn self_dependencies_are_ignored() {
        let graph = vec![info("ncui-core", &["ncui-core"])];
        assert!(find_cycles(&graph).is_empty());
        assert!(check(&graph).is_clean());
    }
}
