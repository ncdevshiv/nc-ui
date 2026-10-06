//! Version negotiation: when a host and a component agree, and what they agree on.
//!
//! The ABI version is the first thing checked and the least negotiable. A component built for
//! an ABI this host cannot provide is refused rather than adapted, because "adapting" an ABI
//! you do not understand is how a runtime ends up executing something it cannot reason about.
//!
//! What negotiation *does* settle is the grant: the intersection of what the host offers and
//! what the component asked for, with any import the profile forbids removed. That result is
//! the only thing a host may instantiate the component with.

use ncui_core::CapabilitySet;

use crate::manifest::{ComponentManifest, HostProfile};

/// The ABI revision this host implements.
///
/// Must equal [`crate::HOST_ABI_VERSION`]; a test asserts they are the same value so the two
/// cannot drift into a host that refuses every component.
pub const HOST_ABI_VERSION: &str = "1.0";

/// Why a component could not be negotiated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegotiationError {
    /// The component asks for an ABI this host does not offer.
    UnsupportedAbi {
        /// The version the component was built against.
        requested: String,
        /// The versions the host offers.
        offered: Vec<String>,
    },
    /// The component's manifest has no version at all.
    MissingAbi,
    /// The surface imports something the host will not provide.
    ForbiddenImport {
        /// The import that was refused.
        import: String,
        /// Why.
        reason: String,
    },
}

impl core::fmt::Display for NegotiationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedAbi { requested, offered } => write!(
                f,
                "component needs ABI {requested}, host offers {}",
                offered.join(", ")
            ),
            Self::MissingAbi => f.write_str("the manifest declares no ABI version"),
            Self::ForbiddenImport { import, reason } => {
                write!(f, "`{import}` is not permitted: {reason}")
            }
        }
    }
}

/// The result of a successful negotiation.
///
/// This is the complete set of things a host learned from a component. Anything not in it is
/// not granted, which is why it carries no defaults and no "otherwise" case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    /// The ABI version agreed, which is one the host offered.
    pub abi_version: String,
    /// The capabilities the component may use: the intersection of what it asked for and what
    /// the host grants.
    pub granted: CapabilitySet,
    /// How many of the manifest's imports the host will resolve.
    pub resolvable_imports: usize,
}

impl Negotiated {
    /// Whether the component is permitted to use `capability`.
    #[must_use]
    pub const fn permits(&self, capability: ncui_core::Capability) -> bool {
        self.granted.contains(capability)
    }
}

/// Negotiates `manifest` against `profile`.
///
/// # Errors
///
/// Returns [`NegotiationError`] when the ABI is unsupported or the surface imports something
/// the profile forbids. A *narrowed* grant is not an error: a component asking for more than
/// the host offers is instantiated with less, which is the normal case and the safe one.
pub fn negotiate(
    manifest: &ComponentManifest,
    profile: &HostProfile,
) -> Result<Negotiated, NegotiationError> {
    if manifest.abi_version.trim().is_empty() {
        return Err(NegotiationError::MissingAbi);
    }
    if !profile.supports_abi(&manifest.abi_version) {
        return Err(NegotiationError::UnsupportedAbi {
            requested: manifest.abi_version.clone(),
            offered: profile.abi_versions().to_vec(),
        });
    }

    let forbidden = manifest.surface.forbidden_by(profile);
    if let Some(import) = forbidden.first() {
        return Err(NegotiationError::ForbiddenImport {
            import: format!("{}::{}", import.module, import.name),
            // Matched rather than checked-then-`expect`ed, so the reason cannot disagree
            // with the branch it describes.
            reason: match import.requires {
                None => "this host grants nothing it did not offer".to_owned(),
                Some(capability) => format!(
                    "{} requires {}, which this host does not grant",
                    import.kind.as_str(),
                    capability.as_str()
                ),
            },
        });
    }

    Ok(Negotiated {
        abi_version: manifest.abi_version.clone(),
        granted: profile.narrow_to(manifest),
        resolvable_imports: manifest.surface.imports().len(),
    })
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
    use crate::surface::{Import, ImportKind, Surface};

    fn manifest() -> ComponentManifest {
        ComponentManifest::new("1.0")
    }

    fn call_import(name: &str, requires: ncui_core::Capability) -> Import {
        Import {
            module: "ncui".to_owned(),
            name: name.to_owned(),
            kind: ImportKind::CapabilityCall,
            requires: Some(requires),
        }
    }

    #[test]
    fn a_matching_version_negotiates() {
        let profile = HostProfile::new(&["1.0"], CapabilitySet::empty());
        let negotiated = negotiate(&manifest(), &profile).expect("1.0 is offered");
        assert_eq!(negotiated.abi_version, "1.0");
        assert!(negotiated.granted.is_empty());
    }

    #[test]
    fn an_unsupported_version_is_refused_rather_than_adapted() {
        let profile = HostProfile::new(&["1.0"], CapabilitySet::all());
        let result = negotiate(&ComponentManifest::new("2.0"), &profile);
        assert_eq!(
            result.expect_err("2.0 is not offered"),
            NegotiationError::UnsupportedAbi {
                requested: "2.0".to_owned(),
                offered: vec!["1.0".to_owned()],
            }
        );
    }

    #[test]
    fn a_manifest_with_no_version_is_refused() {
        let profile = HostProfile::permissive("1.0");
        assert_eq!(
            negotiate(&ComponentManifest::new("   "), &profile).expect_err("blank"),
            NegotiationError::MissingAbi
        );
    }

    #[test]
    fn asking_for_more_than_the_host_offers_narrows_rather_than_failing() {
        // This is the ordinary case, and treating it as an error would mean a host could
        // never load a broadly-written component.
        let profile = HostProfile::new(&["1.0"], CapabilitySet::empty());
        let manifest = manifest().requesting(CapabilitySet::all());
        let negotiated = negotiate(&manifest, &profile).expect("over-requesting is not a failure");
        assert!(
            negotiated.granted.is_empty(),
            "the host granted nothing because it offers nothing"
        );
    }

    #[test]
    fn the_grant_is_the_intersection_of_offer_and_request() {
        let profile = HostProfile::new(
            &["1.0"],
            CapabilitySet::empty().with(ncui_core::Capability::ReadLogs),
        );
        let manifest = manifest().requesting(
            CapabilitySet::empty()
                .with(ncui_core::Capability::ReadLogs)
                .with(ncui_core::Capability::Gpu),
        );
        let negotiated = negotiate(&manifest, &profile).expect("negotiates");
        assert!(negotiated.permits(ncui_core::Capability::ReadLogs));
        assert!(!negotiated.permits(ncui_core::Capability::Gpu));
    }

    #[test]
    fn an_import_the_host_does_not_grant_is_refused() {
        // Unlike a narrowed *declaration*, an import the host cannot resolve is a refusal: the
        // component asked for a function the host does not have, and quietly dropping the
        // import would hand it a surface that differs from what it was built against.
        let profile = HostProfile::new(&["1.0"], CapabilitySet::empty());
        let manifest = manifest().with_surface(
            Surface::new().import(call_import("read_logs", ncui_core::Capability::ReadLogs)),
        );
        let result = negotiate(&manifest, &profile);
        assert!(
            matches!(result, Err(NegotiationError::ForbiddenImport { .. })),
            "got {result:?}"
        );
    }

    #[test]
    fn the_refusal_names_the_import_and_the_reason() {
        let profile = HostProfile::new(&["1.0"], CapabilitySet::empty());
        let manifest = manifest().with_surface(
            Surface::new().import(call_import("read_logs", ncui_core::Capability::ReadLogs)),
        );
        let message = negotiate(&manifest, &profile)
            .expect_err("refused")
            .to_string();
        assert!(message.contains("ncui::read_logs"), "{message}");
        assert!(message.contains("read_logs"), "{message}");
    }

    #[test]
    fn a_resource_borrow_is_refused_by_default_even_from_a_permissive_host() {
        let profile = HostProfile::permissive("1.0");
        let manifest = manifest().with_surface(Surface::new().import(Import {
            module: "ncui".to_owned(),
            name: "log_handle".to_owned(),
            kind: ImportKind::ResourceBorrow,
            requires: Some(ncui_core::Capability::ReadLogs),
        }));
        assert!(
            negotiate(&manifest, &profile).is_err(),
            "an instantiation-time grant must be opted into explicitly"
        );
    }

    #[test]
    fn every_resolvable_import_is_counted_so_the_host_knows_the_surface_size() {
        let profile = HostProfile::permissive("1.0");
        let manifest = manifest().with_surface(
            Surface::new()
                .import(call_import("a", ncui_core::Capability::ReadLogs))
                .import(call_import("b", ncui_core::Capability::InspectRuntime)),
        );
        let negotiated = negotiate(&manifest, &profile).expect("both are grantable");
        assert_eq!(negotiated.resolvable_imports, 2);
    }

    #[test]
    fn a_component_that_declares_nothing_gets_nothing_even_from_a_permissive_host() {
        let profile = HostProfile::permissive("1.0");
        let negotiated = negotiate(&manifest(), &profile).expect("negotiates");
        assert!(
            negotiated.granted.is_empty(),
            "permissive about what it offers, not about granting it unasked"
        );
    }

    #[test]
    fn errors_render_readably() {
        // A refusal the operator cannot read is a refusal they cannot act on.
        let profile = HostProfile::new(&["1.0", "1.1"], CapabilitySet::empty());
        let message = negotiate(&ComponentManifest::new("9.9"), &profile)
            .expect_err("refused")
            .to_string();
        assert!(message.contains("9.9"), "{message}");
        assert!(message.contains("1.0, 1.1"), "{message}");
    }
}
