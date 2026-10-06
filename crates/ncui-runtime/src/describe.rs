//! The `ncui.describe` payload.
//!
//! An agent that cannot discover what it may do is a liability: it will guess, and a guess
//! that happens to be wrong is how an agent ends up deleting data or spinning a thread
//! pool. This module produces a single self-describing document that answers:
//!
//! * what runtime am I talking to, and on what platform?
//! * which methods exist, and what does each one require?
//! * which capabilities and roles exist?
//!
//! It is deliberately cheap to build, because a client is expected to call it on every
//! connection.

use serde::{Deserialize, Serialize};

use ncui_core::{Capability, CapabilityScope, CapabilitySet};
use ncui_security::authz::AgentRole;

/// One method in the surface, with the authority it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodDescription {
    /// The method name, e.g. `"ncui.health"`.
    pub name: String,
    /// One-line summary.
    pub summary: String,
    /// Capabilities the caller must hold. Empty means any authenticated agent may call it.
    pub requires: CapabilitySet,
    /// Whether the method changes state in a way that cannot be undone.
    pub destructive: bool,
}

/// One role, with the grant it carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleDescription {
    /// The role's stable wire name.
    pub name: String,
    /// What the role is for.
    pub summary: String,
    /// The standing grant.
    pub capabilities: CapabilitySet,
}

/// The full description document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Description {
    /// Runtime crate version.
    pub runtime_version: String,
    /// The API version this surface implements.
    pub api_version: String,
    /// Target operating system.
    pub platform: String,
    /// Target CPU architecture.
    pub architecture: String,
    /// The methods available.
    pub methods: Vec<MethodDescription>,
    /// The agent roles available.
    pub roles: Vec<RoleDescription>,
    /// The capability catalogue, grouped by scope.
    pub capabilities: Vec<ScopeDescription>,
    /// The caller's own grants, so a client can render only what it may do.
    pub granted: CapabilitySet,
}

/// One capability scope and the capabilities within it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeDescription {
    /// The scope's stable name.
    pub scope: String,
    /// What the scope grants.
    pub capabilities: CapabilitySet,
}

/// Builds the description for a caller holding `granted`.
///
/// Every field is derived from the compile-time tables, so the description is a pure
/// function of its arguments. An earlier version took `&Runtime` and discarded it as a
/// forward-compatibility hook; speculative parameters that do nothing are noise, so it is
/// gone.
#[must_use]
pub fn describe(granted: CapabilitySet, methods: &[MethodDescription]) -> Description {
    Description {
        runtime_version: crate::RUNTIME_VERSION.to_owned(),
        api_version: crate::rpc::API_VERSION.to_owned(),
        platform: std::env::consts::OS.to_owned(),
        architecture: std::env::consts::ARCH.to_owned(),
        methods: methods.to_vec(),
        roles: AgentRole::ALL
            .iter()
            .map(|role| RoleDescription {
                name: role.as_str().to_owned(),
                summary: role_summary(*role).to_owned(),
                capabilities: role.capabilities(),
            })
            .collect(),
        capabilities: [
            CapabilityScope::Runtime,
            CapabilityScope::Workspace,
            CapabilityScope::Agent,
        ]
        .iter()
        .map(|scope| ScopeDescription {
            scope: scope.as_str().to_owned(),
            capabilities: CapabilitySet::all().in_scope(*scope),
        })
        .collect(),
        granted,
    }
}

fn role_summary(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Observer => "Read-only diagnostics: logs, metrics, runtime and GPU inspection.",
        AgentRole::Debugger => "Observer plus filesystem reads and debugger attachment.",
        AgentRole::Tester => "Debugger plus artifact writes and mediated test/benchmark execution.",
        AgentRole::Developer => "Tester plus source, dependency and network authority.",
        AgentRole::Administrator => {
            "Every capability, still gated per call for destructive actions."
        }
        // A role added to `ncui-security` after this table was written.
        _ => "Unrecognised role; inspect its capability set directly.",
    }
}

/// The capability names in a scope, for callers that want a flat list.
#[must_use]
pub fn capability_names(scope: CapabilityScope) -> Vec<&'static str> {
    CapabilitySet::all()
        .in_scope(scope)
        .iter()
        .map(Capability::as_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_covers_every_method_role_and_capability() {
        let methods = vec![MethodDescription {
            name: "ncui.health".to_owned(),
            summary: "health".to_owned(),
            requires: CapabilitySet::empty(),
            destructive: false,
        }];
        let description = describe(CapabilitySet::empty(), &methods);

        assert_eq!(description.roles.len(), AgentRole::ALL.len());
        assert_eq!(description.capabilities.len(), 3, "one entry per scope");
        assert_eq!(description.methods, methods);
        assert!(!description.runtime_version.is_empty());
    }

    #[test]
    fn the_capability_catalogue_is_complete_and_disjoint() {
        let description = describe(CapabilitySet::empty(), &[]);

        let total: usize = description
            .capabilities
            .iter()
            .map(|scope| scope.capabilities.len() as usize)
            .sum();
        assert_eq!(
            total,
            Capability::ALL.len(),
            "every capability must appear in exactly one scope"
        );
    }

    #[test]
    fn an_observer_is_described_as_holding_no_destructive_authority() {
        let description = describe(CapabilitySet::empty(), &[]);
        let observer = description
            .roles
            .iter()
            .find(|role| role.name == "observer")
            .expect("observer must be described");
        assert!(
            !observer.capabilities.has_destructive(),
            "an observer must not be described as holding destructive authority"
        );
    }

    #[test]
    fn the_description_serialises_to_json() {
        let value =
            serde_json::to_value(describe(CapabilitySet::all(), &[])).expect("serializable");
        assert!(value["methods"].is_array());
        assert!(value["roles"].is_array());
        assert!(value["platform"].is_string());
    }
}
