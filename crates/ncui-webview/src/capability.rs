//! Capability isolation: what a page is granted, and what it must not be.
//!
//! Web content is untrusted. A page cannot be given a capability because it asked nicely, and
//! it cannot be given one because the *application* holds it — a capability held by the
//! application and reachable from the page is a capability the page holds.
//!
//! So the grant is explicit, narrow, and default-empty. There is no "grant everything except
//! the list of bad ones" path, because such a path fails the first time someone adds a
//! capability and forgets to add it to the list.

use ncui_core::{Capability, CapabilitySet};

/// What a page is allowed to ask the runtime for.
///
/// Empty by default. Constructing one with a capability is the only way a page gets it, which
/// makes the grant auditable by reading the construction site rather than by auditing a deny
/// list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilityGrant {
    granted: CapabilitySet,
}

impl CapabilityGrant {
    /// A grant of nothing.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// A grant of exactly these capabilities.
    #[must_use]
    pub fn of(capabilities: &[Capability]) -> Self {
        let mut granted = CapabilitySet::empty();
        for capability in capabilities {
            granted = granted.with(*capability);
        }
        Self { granted }
    }

    /// A grant of exactly the capabilities in an existing set.
    #[must_use]
    pub const fn from_set(granted: CapabilitySet) -> Self {
        Self { granted }
    }

    /// The capabilities granted.
    #[must_use]
    pub const fn granted(&self) -> CapabilitySet {
        self.granted
    }

    /// Whether `required` is satisfied by this grant.
    #[must_use]
    pub const fn allows(&self, required: CapabilitySet) -> bool {
        required.is_subset_of(self.granted)
    }

    /// Refuses `required`, or reports what was missing.
    ///
    /// # Errors
    ///
    /// Returns the granted set when the grant does not cover `required`. The message names
    /// what was granted, because "denied" without that is not actionable by an operator
    /// reading a log.
    pub fn require(&self, required: CapabilitySet) -> Result<(), String> {
        if self.allows(required) {
            Ok(())
        } else {
            Err(format!(
                "capability_denied: the page is granted {} and that does not cover {}",
                self.granted, required
            ))
        }
    }

    /// Narrows this grant to a subset, which can only remove.
    ///
    /// Built from `is_subset_of` rather than an intersection because `CapabilitySet` has no
    /// intersection; the result is built by folding over `Capability::ALL`, which keeps the
    /// "can only remove" property checkable rather than asserted.
    #[must_use]
    pub fn narrowed_to(&self, keep: CapabilitySet) -> Self {
        let mut narrowed = CapabilitySet::empty();
        for capability in Capability::ALL {
            if self.granted.contains(*capability) && keep.contains(*capability) {
                narrowed = narrowed.with(*capability);
            }
        }
        Self { granted: narrowed }
    }
}

/// A request a page made of the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRequest {
    /// The action requested, named by the page. It is data, not a dispatch key: the seam maps
    /// it to a capability rather than calling it.
    pub action: String,
    /// What the action requires of the runtime.
    pub requires: CapabilitySet,
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

    #[test]
    fn a_page_is_granted_nothing_by_default() {
        // The property the whole module exists to hold: an unconfigured page can do nothing.
        let grant = CapabilityGrant::none();
        assert!(!grant.allows(CapabilitySet::empty().with(Capability::InspectRuntime)));
        assert!(grant.allows(CapabilitySet::empty()));
    }

    #[test]
    fn a_grant_covers_exactly_what_it_lists() {
        let inspect = CapabilitySet::empty().with(Capability::InspectRuntime);
        let grant = CapabilityGrant::of(&[Capability::InspectRuntime]);
        assert!(grant.allows(inspect));

        // A grant of one capability must not satisfy a request for two.
        let both = inspect.with(Capability::Deploy);
        assert!(!grant.allows(both));
    }

    #[test]
    fn a_refusal_names_what_was_granted() {
        let grant = CapabilityGrant::of(&[Capability::InspectRuntime]);
        let required = CapabilitySet::empty().with(Capability::Gpu);
        let message = grant.require(required).expect_err("gpus are not granted");
        assert!(
            message.contains("capability_denied"),
            "the code must be stable and machine-readable; got {message}"
        );
        assert!(
            !message.contains("covers"),
            "the message must not read as a success; got {message}"
        );
    }

    #[test]
    fn narrowing_can_only_remove() {
        let broad = CapabilityGrant::of(&[
            Capability::InspectRuntime,
            Capability::Gpu,
            Capability::FilesystemWrite,
        ]);
        let narrow = broad.narrowed_to(CapabilitySet::empty().with(Capability::InspectRuntime));

        assert!(narrow.allows(CapabilitySet::empty().with(Capability::InspectRuntime)));
        assert!(!narrow.allows(CapabilitySet::empty().with(Capability::Gpu)));
        // And narrowing below the current grant never *adds*.
        let nothing = narrow.narrowed_to(CapabilitySet::empty());
        assert!(!nothing.allows(CapabilitySet::empty().with(Capability::InspectRuntime)));
    }

    #[test]
    fn a_page_holding_a_capability_still_cannot_reach_a_wider_one() {
        // The escalation this module prevents: a page granted inspection must not thereby be
        // able to ask for control.
        let grant = CapabilityGrant::of(&[Capability::InspectRuntime]);
        let escalate = CapabilityRequest {
            action: "restart_worker".to_owned(),
            requires: CapabilitySet::empty().with(Capability::Deploy),
        };
        assert!(grant.require(escalate.requires).is_err());
    }
}
