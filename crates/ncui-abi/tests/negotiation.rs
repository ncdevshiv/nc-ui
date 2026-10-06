//! End-to-end ABI negotiation, driven through the public surface.
//!
//! The properties here are the ones a component author and a host operator both depend on, and
//! none of them are visible from a single module: a component is a manifest *plus* a surface
//! *plus* a host profile, and the guarantee is about how those three meet.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use ncui_abi::surface::{Import, ImportKind, Surface};
use ncui_abi::{ComponentManifest, HostProfile, negotiate};
use ncui_core::{Capability, CapabilitySet};

fn call(name: &str, requires: Capability) -> Import {
    Import {
        module: "ncui".to_owned(),
        name: name.to_owned(),
        kind: ImportKind::CapabilityCall,
        requires: Some(requires),
    }
}

fn borrow(name: &str, requires: Capability) -> Import {
    Import {
        module: "ncui".to_owned(),
        name: name.to_owned(),
        kind: ImportKind::ResourceBorrow,
        requires: Some(requires),
    }
}

/// The property the whole crate exists for: a component cannot obtain a capability neither it
/// requested nor the host offered, by any combination of declaration and import.
#[test]
fn a_component_cannot_obtain_a_capability_through_the_import_surface() {
    // The component declares *nothing* and imports everything. Reading only the declaration
    // field would conclude it needs nothing and grant it everything the surface mentions.
    let sneaky = ComponentManifest::new("1.0").with_surface(
        Surface::new()
            .import(call("read_logs", Capability::ReadLogs))
            .import(call("deploy", Capability::Deploy))
            .import(call("delete", Capability::DeleteData)),
    );
    let cautious = HostProfile::new(&["1.0"], CapabilitySet::empty().with(Capability::ReadLogs));

    // It is refused outright, because an import the host cannot resolve is not a narrowed
    // grant — the component was built against a surface the host does not have.
    assert!(
        negotiate(&sneaky, &cautious).is_err(),
        "a component importing what the host cannot provide must be refused"
    );
}

/// And when the host *can* resolve them, the grant is exactly the offer.
#[test]
fn the_grant_is_exactly_what_both_sides_agree_on() {
    let host = HostProfile::new(
        &["1.0"],
        CapabilitySet::empty()
            .with(Capability::ReadLogs)
            .with(Capability::InspectRuntime),
    );
    let component = ComponentManifest::new("1.0")
        .requesting(CapabilitySet::empty().with(Capability::ReadLogs))
        .with_surface(Surface::new().import(call("read_logs", Capability::ReadLogs)));

    let negotiated = negotiate(&component, &host).expect("negotiates");
    assert!(negotiated.permits(Capability::ReadLogs));
    assert!(
        !negotiated.permits(Capability::InspectRuntime),
        "offered but not asked for, so not granted"
    );
}

/// An ABI this host does not implement is refused even when every capability is offered.
#[test]
fn an_unsupported_abi_is_refused_before_any_capability_is_considered() {
    // Ordering matters: a version mismatch means the wire format may not be what this host
    // thinks it is, so reasoning about capabilities first would be reasoning about a format
    // that might not exist.
    let generous = HostProfile::permissive("1.0");
    assert!(
        negotiate(&ComponentManifest::new("2.0"), &generous).is_err(),
        "a generous host still refuses an ABI it does not implement"
    );
    assert!(
        negotiate(&ComponentManifest::new("1.0"), &generous).is_ok(),
        "and accepts the one it does"
    );
}

/// A resource borrow is refused by default, because it is granted once rather than per call.
#[test]
fn a_borrow_is_refused_unless_the_host_opts_in_explicitly() {
    let component = ComponentManifest::new("1.0")
        .requesting(CapabilitySet::empty().with(Capability::ReadLogs))
        .with_surface(Surface::new().import(borrow("log_handle", Capability::ReadLogs)));

    let default_host = HostProfile::permissive("1.0");
    assert!(
        negotiate(&component, &default_host).is_err(),
        "the default posture refuses an instantiation-time grant"
    );

    let opted_in = default_host.with_instantiation_grants(true);
    let negotiated = negotiate(&component, &opted_in).expect("opted in");
    assert!(negotiated.permits(Capability::ReadLogs));
}

/// A component that declares nothing and imports nothing gets nothing, however permissive the
/// host is. Permissive is about what a host *offers*, not about granting it unasked.
#[test]
fn a_permissive_host_is_permissive_about_offers_not_grants() {
    let silent = ComponentManifest::new("1.0");
    let negotiated = negotiate(&silent, &HostProfile::permissive("1.0")).expect("negotiates");
    assert!(
        negotiated.granted.is_empty(),
        "a component that asked for nothing receives nothing"
    );
}

/// The ordinary case is covered too: a well-behaved component against a host that offers what
/// it asks for gets exactly that, with nothing refused.
#[test]
fn a_well_behaved_component_loads_on_a_host_that_offers_what_it_needs() {
    use ncui_abi::surface::{Export, ExportKind};

    let component = ComponentManifest::new("1.0")
        .requesting(CapabilitySet::empty().with(Capability::ReadLogs))
        .with_surface(
            Surface::new()
                .import(call("read_logs", Capability::ReadLogs))
                .export(Export {
                    name: "summarise".to_owned(),
                    kind: ExportKind::Function,
                }),
        );

    let host = HostProfile::new(&["1.0"], CapabilitySet::empty().with(Capability::ReadLogs));
    let negotiated = negotiate(&component, &host).expect("a matching component loads");
    assert_eq!(negotiated.abi_version, "1.0");
    assert!(negotiated.permits(Capability::ReadLogs));
    assert_eq!(negotiated.resolvable_imports, 1);
}

/// Negotiation is deterministic: the same three inputs always give the same answer.
#[test]
fn negotiation_is_deterministic() {
    let host = HostProfile::new(&["1.0", "1.1"], CapabilitySet::all());
    let component = ComponentManifest::new("1.1")
        .requesting(CapabilitySet::empty().with(Capability::Network))
        .with_surface(Surface::new().import(call("net", Capability::Network)));

    let first = negotiate(&component, &host).expect("negotiates");
    for _ in 0..8 {
        assert_eq!(negotiate(&component, &host).expect("negotiates"), first);
    }
}

/// Every refusal is readable enough to act on.
#[test]
fn every_refusal_names_what_was_asked_for_and_what_is_available() {
    let host = HostProfile::new(&["1.0"], CapabilitySet::empty());

    let abi_message = negotiate(&ComponentManifest::new("3.0"), &host)
        .expect_err("refused")
        .to_string();
    assert!(abi_message.contains("3.0"), "{abi_message}");
    assert!(abi_message.contains("1.0"), "{abi_message}");

    let import_message = negotiate(
        &ComponentManifest::new("1.0")
            .with_surface(Surface::new().import(call("read_logs", Capability::ReadLogs))),
        &host,
    )
    .expect_err("refused")
    .to_string();
    assert!(
        import_message.contains("ncui::read_logs"),
        "{import_message}"
    );
}

/// A locked-down host refuses everything a component could want, and the refusals are specific
/// enough to tell an operator what to change.
#[test]
fn a_locked_down_host_refuses_everything_and_says_what() {
    let host = HostProfile::locked_down("1.0");
    let component = ComponentManifest::new("1.0").with_surface(
        Surface::new()
            .import(call("a", Capability::ReadLogs))
            .import(call("b", Capability::Deploy)),
    );

    let refusal = negotiate(&component, &host)
        .expect_err("refused")
        .to_string();
    assert!(refusal.contains("ncui::a"), "{refusal}");
    assert!(
        !refusal.contains("ncui::b"),
        "one refusal names one import, so the report stays readable: {refusal}"
    );
}

/// The surface's size is reported, so a host can bound what it is about to instantiate.
#[test]
fn the_resolved_surface_size_is_reported() {
    let host = HostProfile::permissive("1.0");
    let component = ComponentManifest::new("1.0").with_surface(
        Surface::new()
            .import(call("a", Capability::ReadLogs))
            .import(call("b", Capability::Network))
            .import(call("c", Capability::InspectRuntime)),
    );

    let negotiated = negotiate(&component, &host).expect("all grantable");
    assert_eq!(negotiated.resolvable_imports, 3);
}
