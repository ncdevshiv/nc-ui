//! The language-neutral component ABI: wire format, version negotiation, capability declarations.
//!
//! This crate decides whether execution would be *safe*; `ncui-component` is the crate that
//! executes. The split is deliberate: the rules below are security-relevant and must be
//! testable without an interpreter in the dependency graph, and the interpreter must not be
//! able to make a policy decision of its own.
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
//! This crate does not execute WASM: it is the gate every such runtime sits behind, and the
//! rules that gate applies. `ncui-component` is the executor — it parses a module, reads the
//! manifest out of the artifact through [`wire`], calls [`negotiate`] and only then links the
//! imports the negotiation allowed. Splitting the two keeps the security-relevant decision
//! testable without an interpreter in the dependency graph, and keeps the interpreter from
//! being able to make a decision of its own.

pub mod manifest;
pub mod negotiation;
pub mod surface;
pub mod wire;

pub use manifest::{ComponentManifest, HostProfile};
pub use negotiation::{Negotiated, NegotiationError, negotiate};
pub use surface::{Export, ExportKind, Import, ImportKind, Surface};
pub use wire::{HOST_MODULE, WireError, WireExport, WireImport, WireManifest};

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
