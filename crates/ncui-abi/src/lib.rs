//! The language-neutral component ABI: wire format, version negotiation, capability declarations.
//!
//! `docs/remaining-work.md` records the split for this capability: "the wire format, version
//! negotiation and capability declarations are testable here; execution is not". This crate is
//! the testable half, and it is the half that decides whether execution would be *safe*, not
//! merely whether it would work.
//!
//! # Why the wire format is the interesting part
//!
//! A component that crosses the boundary carries three things that must agree before anything
//! runs: an ABI version, a set of granted capabilities, and a typed import/export surface. All
//! three are attacker-influenced, so all three are validated against explicit rules here:
//!
//! * **Version.** A host and a component must agree, and disagreement is a refusal rather than
//!   a best-effort parse. A component that asked for an ABI this host cannot provide is not a
//!   component that runs "mostly".
//! * **Capabilities.** Declared by the component, enforced by the host, and *narrowed* — a
//!   component cannot grant itself a capability its host did not offer. This is the property
//!   that keeps a component from being a privilege-escalation path, and it is asserted
//!   directly rather than assumed.
//! * **Imports.** A component's imports are its *requests*. An import the host does not offer
//!   is refused, because a host that resolved it from somewhere else would be inventing a
//!   capability.
//!
//! # What this crate is not
//!
//! It does not execute WASM. Execution needs a component-model runtime, which is a large
//! dependency and cannot be verified here; `docs/known-gaps.md` records that. What this crate
//! provides is the gate every such runtime would sit behind, testable without one.

pub mod manifest;
pub mod negotiation;
pub mod surface;

pub use manifest::{ComponentManifest, HostProfile};
pub use negotiation::{Negotiated, NegotiationError, negotiate};
pub use surface::{Export, ExportKind, Import, ImportKind, Surface};

/// The ABI revision this host implements.
///
/// A breaking change to the wire format is a new value here, and a component built against an
/// older one is refused rather than adapted — adapting an ABI you do not understand is how a
/// runtime ends up executing something it cannot reason about.
pub const HOST_ABI_VERSION: &str = "1.0";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_abi_version_is_the_version_the_negotiator_uses() {
        // Two constants meaning the same thing is how a host ends up refusing every component
        // or accepting every component.
        assert_eq!(HOST_ABI_VERSION, negotiation::HOST_ABI_VERSION);
    }
}
