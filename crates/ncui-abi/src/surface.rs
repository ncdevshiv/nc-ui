//! A component's import and export surface.
//!
//! Imports are what a component *asks the host to do* — they are requests, not declarations
//! of fact, and the host resolves each one against what it actually offers. Exports are what
//! it offers back.
//!
//! The distinction matters because a surface that treats imports as declarations gives every
//! component the union of everything any component has ever asked for.

use core::fmt;

/// What a component imports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    /// A capability check the component will make at run time.
    CapabilityCheck,
    /// A call into a capability the host exposes.
    CapabilityCall,
    /// A handle to a host-owned resource.
    ResourceBorrow,
}

impl ImportKind {
    /// The stable wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CapabilityCheck => "capability_check",
            Self::CapabilityCall => "capability_call",
            Self::ResourceBorrow => "resource_borrow",
        }
    }

    /// Whether this kind grants anything without a per-call check.
    ///
    /// A borrow hands the component a *reference* to a host resource, so it is granted at
    /// instantiation rather than checked per call. That is what makes it the kind worth
    /// looking at hardest, and why the host profile can forbid it outright.
    #[must_use]
    pub const fn is_granted_at_instantiation(self) -> bool {
        matches!(self, Self::ResourceBorrow)
    }
}

/// One import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// The module it comes from.
    pub module: String,
    /// The name within that module.
    pub name: String,
    /// What kind of import it is.
    pub kind: ImportKind,
    /// The capability it requires, if any.
    pub requires: Option<ncui_core::Capability>,
}

/// What a component exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    /// A function the host may call.
    Function,
    /// A resource the host may construct.
    Resource,
    /// A value the host may read.
    Value,
}

impl ExportKind {
    /// The stable wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Resource => "resource",
            Self::Value => "value",
        }
    }
}

/// One export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    /// The name the host calls it by.
    pub name: String,
    /// What kind of export it is.
    pub kind: ExportKind,
}

/// A component's complete surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Surface {
    imports: Vec<Import>,
    exports: Vec<Export>,
}

impl Surface {
    /// An empty surface.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an import.
    #[must_use]
    pub fn import(mut self, import: Import) -> Self {
        if !self.imports.contains(&import) {
            self.imports.push(import);
        }
        self
    }

    /// Adds an export.
    #[must_use]
    pub fn export(mut self, export: Export) -> Self {
        if !self.exports.contains(&export) {
            self.exports.push(export);
        }
        self
    }

    /// The imports, in declaration order.
    #[must_use]
    pub fn imports(&self) -> &[Import] {
        &self.imports
    }

    /// The exports, in declaration order.
    #[must_use]
    pub fn exports(&self) -> &[Export] {
        &self.exports
    }

    /// The union of every capability the surface's imports require.
    #[must_use]
    pub fn required_capabilities(&self) -> ncui_core::CapabilitySet {
        let mut required = ncui_core::CapabilitySet::empty();
        for import in &self.imports {
            if let Some(capability) = import.requires {
                required = required.with(capability);
            }
        }
        required
    }

    /// Imports of a kind the host profile forbids.
    #[must_use]
    pub fn forbidden_by(&self, profile: &super::HostProfile) -> Vec<&Import> {
        self.imports
            .iter()
            .filter(|import| !profile.permits(import))
            .collect()
    }
}

impl fmt::Display for Surface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} import(s), {} export(s)",
            self.imports.len(),
            self.exports.len()
        )
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
    use ncui_core::Capability;

    fn import(name: &str, kind: ImportKind, requires: Option<Capability>) -> Import {
        Import {
            module: "ncui".to_owned(),
            name: name.to_owned(),
            kind,
            requires,
        }
    }

    #[test]
    fn required_capabilities_are_the_union_of_the_imports() {
        let surface = Surface::new()
            .import(import(
                "read",
                ImportKind::CapabilityCall,
                Some(Capability::ReadLogs),
            ))
            .import(import(
                "inspect",
                ImportKind::CapabilityCall,
                Some(Capability::InspectRuntime),
            ))
            .import(import("noop", ImportKind::CapabilityCheck, None));

        let required = surface.required_capabilities();
        assert!(required.contains(Capability::ReadLogs));
        assert!(required.contains(Capability::InspectRuntime));
        assert_eq!(
            surface.imports().len(),
            3,
            "an import requiring nothing is still an import"
        );
    }

    #[test]
    fn a_duplicate_import_is_recorded_once() {
        // A repeated import is not a stronger claim, and counting it twice would make the
        // surface's size — which appears in a diagnostic — misleading.
        let surface = Surface::new()
            .import(import(
                "read",
                ImportKind::CapabilityCall,
                Some(Capability::ReadLogs),
            ))
            .import(import(
                "read",
                ImportKind::CapabilityCall,
                Some(Capability::ReadLogs),
            ));
        assert_eq!(surface.imports().len(), 1);
    }

    #[test]
    fn the_same_name_under_a_different_kind_is_two_imports() {
        // Not a duplicate: borrowing and calling are different grants.
        let surface = Surface::new()
            .import(import(
                "log",
                ImportKind::CapabilityCall,
                Some(Capability::ReadLogs),
            ))
            .import(import(
                "log",
                ImportKind::ResourceBorrow,
                Some(Capability::ReadLogs),
            ));
        assert_eq!(surface.imports().len(), 2);
    }

    #[test]
    fn a_resource_borrow_is_granted_at_instantiation_rather_than_per_call() {
        // This is the kind that matters: it hands over a reference rather than asking per
        // call, so a host that forbids it is refusing the whole borrow, not one operation.
        assert!(ImportKind::ResourceBorrow.is_granted_at_instantiation());
        assert!(!ImportKind::CapabilityCall.is_granted_at_instantiation());
        assert!(!ImportKind::CapabilityCheck.is_granted_at_instantiation());
    }

    #[test]
    fn wire_names_are_stable() {
        assert_eq!(ImportKind::CapabilityCall.as_str(), "capability_call");
        assert_eq!(ExportKind::Function.as_str(), "function");
    }

    #[test]
    fn an_empty_surface_requires_nothing() {
        let surface = Surface::new();
        assert!(surface.required_capabilities().is_empty());
        assert!(surface.imports().is_empty());
        assert!(
            surface
                .forbidden_by(&super::super::HostProfile::permissive("1.0"))
                .is_empty()
        );
    }
}
