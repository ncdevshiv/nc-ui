//! What a component declares, and what a host is willing to give it.
//!
//! The rule this module exists to enforce: **a component declares what it wants; the host
//! decides what it gets.** A manifest that both read and wrote would let any component grant
//! itself whatever it liked, and the capability model would be decorative.
//!
//! [`HostProfile::narrow_to`] is therefore the only path from a manifest to a runnable one,
//! and it can only ever remove.

use ncui_core::CapabilitySet;

use crate::surface::{Import, Surface};

/// What a component says about itself.
///
/// Untrusted input: it arrives from the component, which is code this runtime did not write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentManifest {
    /// The ABI version the component was built against.
    pub abi_version: String,
    /// The capabilities the component believes it needs.
    pub requested: CapabilitySet,
    /// The import and export surface.
    pub surface: Surface,
}

impl ComponentManifest {
    /// A manifest declaring `abi_version` and nothing else.
    #[must_use]
    pub fn new(abi_version: &str) -> Self {
        Self {
            abi_version: abi_version.to_owned(),
            requested: CapabilitySet::empty(),
            surface: Surface::new(),
        }
    }

    /// Declares a requested capability.
    #[must_use]
    pub fn requesting(mut self, capabilities: CapabilitySet) -> Self {
        self.requested = self.requested.union(capabilities);
        self
    }

    /// Declares the surface.
    #[must_use]
    pub fn with_surface(mut self, surface: Surface) -> Self {
        self.surface = surface;
        self
    }

    /// Every capability the manifest implies, whether declared or implied by its imports.
    ///
    /// A component that declares nothing but imports `capability_call(read_logs)` has asked
    /// for read access. Reading only the declaration field would miss it — and that is how a
    /// manifest gets to claim nothing and import everything.
    #[must_use]
    pub fn effective_requirements(&self) -> CapabilitySet {
        self.requested.union(self.surface.required_capabilities())
    }
}

/// What this host offers components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostProfile {
    abi_versions: Vec<String>,
    grantable: CapabilitySet,
    allow_instantiation_grants: bool,
}

impl HostProfile {
    /// A profile offering the given ABI versions and capabilities.
    #[must_use]
    pub fn new(abi_versions: &[&str], grantable: CapabilitySet) -> Self {
        Self {
            abi_versions: abi_versions.iter().map(|v| (*v).to_owned()).collect(),
            grantable,
            allow_instantiation_grants: false,
        }
    }

    /// The profile most host implementations should start from: one ABI version, nothing
    /// grantable, and no instantiation-time grants.
    #[must_use]
    pub fn locked_down(abi_version: &str) -> Self {
        Self::new(&[abi_version], CapabilitySet::empty())
    }

    /// A profile that grants everything this runtime has.
    ///
    /// Named `permissive` rather than `default` so that a host reaching for "the easy one" is
    /// visibly choosing the weaker posture.
    #[must_use]
    pub fn permissive(abi_version: &str) -> Self {
        Self::new(&[abi_version], CapabilitySet::all())
    }

    /// Permits resource borrows, which are granted at instantiation rather than per call.
    #[must_use]
    pub const fn with_instantiation_grants(mut self, allow: bool) -> Self {
        self.allow_instantiation_grants = allow;
        self
    }

    /// Whether this profile offers `version`.
    #[must_use]
    pub fn supports_abi(&self, version: &str) -> bool {
        self.abi_versions.iter().any(|v| v == version)
    }

    /// The ABI versions offered, oldest first is not assumed — order is as configured.
    #[must_use]
    pub fn abi_versions(&self) -> &[String] {
        &self.abi_versions
    }

    /// Everything this profile will grant.
    #[must_use]
    pub const fn grantable(&self) -> CapabilitySet {
        self.grantable
    }

    /// Whether this profile permits an import at all, before any capability is considered.
    #[must_use]
    pub const fn permits(&self, import: &Import) -> bool {
        if import.kind.is_granted_at_instantiation() {
            return self.allow_instantiation_grants;
        }
        match import.requires {
            Some(capability) => self.grantable.contains(capability),
            // An import requiring nothing is permitted by any profile, including the locked
            // one: there is nothing to grant. Whether it is *useful* is another question.
            None => true,
        }
    }

    /// The capabilities this profile will grant to `manifest`.
    ///
    /// The intersection, which is the whole point: a component that asks for everything gets
    /// only what the host offers, and a component that asks for something undeclared but
    /// implied by its imports still only gets the intersection.
    #[must_use]
    pub fn narrow_to(&self, manifest: &ComponentManifest) -> CapabilitySet {
        // `CapabilitySet` has no intersection, so the result is folded over the declared
        // requirements rather than computed from the host alone — which keeps the answer a
        // subset of both.
        let mut granted = CapabilitySet::empty();
        for capability in CapabilitySet::all().iter() {
            if manifest.effective_requirements().contains(capability)
                && self.grantable.contains(capability)
            {
                granted = granted.with(capability);
            }
        }
        granted
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::ImportKind;

    fn manifest() -> ComponentManifest {
        ComponentManifest::new("1.0")
    }

    fn call_import(name: &str, requires: Option<ncui_core::Capability>) -> Import {
        Import {
            module: "ncui".to_owned(),
            name: name.to_owned(),
            kind: ImportKind::CapabilityCall,
            requires,
        }
    }

    #[test]
    fn a_manifest_that_declares_nothing_but_imports_everything_still_asks_for_it() {
        // The escalation this module prevents: reading only the `requested` field would give
        // this manifest an empty requirement set and let it import anything.
        let manifest = manifest().with_surface(
            Surface::new()
                .import(call_import("read", Some(ncui_core::Capability::ReadLogs)))
                .import(call_import("deploy", Some(ncui_core::Capability::Deploy))),
        );
        let required = manifest.effective_requirements();
        assert!(required.contains(ncui_core::Capability::ReadLogs));
        assert!(required.contains(ncui_core::Capability::Deploy));
    }

    #[test]
    fn narrowing_can_only_reduce_what_the_component_asked_for() {
        let permissive = HostProfile::permissive("1.0");
        let manifest =
            manifest().requesting(CapabilitySet::empty().with(ncui_core::Capability::ReadLogs));
        let granted = permissive.narrow_to(&manifest);
        assert!(
            granted.contains(ncui_core::Capability::ReadLogs),
            "what the host offers and the component asked for is granted"
        );
        assert!(
            !granted.contains(ncui_core::Capability::Deploy),
            "a capability the component did not ask for is not granted, however permissive \
             the host is"
        );
    }

    #[test]
    fn a_component_cannot_ask_for_something_the_host_does_not_offer() {
        // The property the whole module exists for.
        let cautious = HostProfile::new(&["1.0"], CapabilitySet::empty());
        let manifest = manifest().requesting(
            CapabilitySet::empty()
                .with(ncui_core::Capability::Deploy)
                .with(ncui_core::Capability::DeleteData),
        );
        let granted = cautious.narrow_to(&manifest);
        assert!(
            granted.is_empty(),
            "a locked-down host grants nothing to a component that asks for everything"
        );
    }

    #[test]
    fn the_granted_set_is_a_subset_of_both_sides() {
        let host = HostProfile::new(
            &["1.0"],
            CapabilitySet::empty()
                .with(ncui_core::Capability::ReadLogs)
                .with(ncui_core::Capability::Network),
        );
        let manifest = manifest().requesting(
            CapabilitySet::empty()
                .with(ncui_core::Capability::ReadLogs)
                .with(ncui_core::Capability::Gpu),
        );
        let granted = host.narrow_to(&manifest);
        assert!(granted.contains(ncui_core::Capability::ReadLogs));
        assert!(
            !granted.contains(ncui_core::Capability::Gpu),
            "not requested"
        );
        assert!(
            !granted.contains(ncui_core::Capability::Network),
            "not offered"
        );
    }

    #[test]
    fn a_locked_down_profile_permits_no_capability_call() {
        let profile = HostProfile::locked_down("1.0");
        let import = call_import("read", Some(ncui_core::Capability::ReadLogs));
        assert!(!profile.permits(&import));
        // But an import requiring nothing is not a grant, so it is still permitted.
        assert!(profile.permits(&call_import("noop", None)));
    }

    #[test]
    fn a_resource_borrow_is_refused_unless_the_profile_opts_in() {
        let borrow = Import {
            module: "ncui".to_owned(),
            name: "log_handle".to_owned(),
            kind: ImportKind::ResourceBorrow,
            requires: Some(ncui_core::Capability::ReadLogs),
        };
        // Even a permissive profile refuses it by default, because it is granted at
        // instantiation rather than per call.
        let permissive = HostProfile::permissive("1.0");
        assert!(!permissive.permits(&borrow));
        assert!(permissive.with_instantiation_grants(true).permits(&borrow));
    }

    #[test]
    fn a_profile_offers_exactly_the_versions_it_lists() {
        let profile = HostProfile::new(&["1.0", "1.1"], CapabilitySet::empty());
        assert!(profile.supports_abi("1.0"));
        assert!(profile.supports_abi("1.1"));
        assert!(!profile.supports_abi("2.0"));
        assert!(
            !profile.supports_abi(""),
            "an empty version is not version 1.0"
        );
        assert_eq!(profile.abi_versions().len(), 2);
    }
}
