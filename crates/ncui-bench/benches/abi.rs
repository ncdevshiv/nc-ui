//! Scenarios for `ncui-abi`: the cost of deciding whether a component may load.

use ncui_bench::{Repeated, measure};
use ncui_core::{Capability, CapabilitySet};

fn fail(message: &str) -> ! {
    use std::io::Write;
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let _ = writeln!(out, "benchmark failure: {message}");
    std::process::exit(1);
}

/// Records every component-ABI scenario.
pub fn record(scenarios: &mut Vec<Repeated>) {
    use ncui_abi::surface::{Import, ImportKind, Surface};
    use ncui_abi::{ComponentManifest, HostProfile, negotiate};

    let mut surface = Surface::new();
    for (order, capability) in Capability::ALL.iter().enumerate() {
        surface = surface.import(Import {
            module: "ncui".to_owned(),
            name: format!("op-{order}"),
            kind: ImportKind::CapabilityCall,
            requires: Some(*capability),
        });
    }

    let manifest = ComponentManifest::new("1.0")
        .requesting(CapabilitySet::all())
        .with_surface(surface);
    // A profile offering everything, so the negotiation reaches the grant rather than
    // short-circuiting on a forbidden import — the expensive path is the interesting one.
    let profile = HostProfile::new(&["1.0"], CapabilitySet::all());

    scenarios.push(
        measure(
            "abi.negotiate",
            "negotiate a component declaring every capability and importing every capability",
            500,
            20_000,
            || usize::from(negotiate(&manifest, &profile).is_ok()),
        )
        .unwrap_or_else(|e| fail(&e)),
    );

    scenarios.push(
        measure(
            "abi.grant_narrow",
            "compute the granted set for a component requesting everything",
            1_000,
            50_000,
            || {
                let granted = profile
                    .narrow_to(&ComponentManifest::new("1.0").requesting(CapabilitySet::all()));
                usize::from(granted.contains(Capability::Gpu))
            },
        )
        .unwrap_or_else(|e| fail(&e)),
    );
}
