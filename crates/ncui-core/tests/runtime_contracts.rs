//! Cross-crate contract tests for `ncui-core`.
//!
//! These assert the properties other crates depend on. If any of them fails, the
//! subsystems built on top of `ncui-core` rest on a broken foundation, so they are
//! stated as tests rather than as prose.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use ncui_core::capability::{Capability, CapabilitySet};
use ncui_core::limits::Limits;
use ncui_core::version::SemVer;
use ncui_core::{ErrorKind, NcError};

#[test]
fn an_error_carries_a_correlation_id_into_its_json_form() {
    let correlation = ncui_core::CorrelationId::new();
    let error = NcError::new(ErrorKind::CapabilityDenied {
        capability: "delete.data".to_owned(),
        subject: "agent-1".to_owned(),
    })
    .operation("agent::authorize")
    .correlated(correlation);

    // The JSON form is what an agent reads. It must carry the machine-readable code and
    // the correlation id, or a failure cannot be joined to its trace.
    let json = serde_json::to_value(&error).expect("errors must be serialisable");
    assert_eq!(json["code"], "capability_denied");
    assert_eq!(json["operation"], "agent::authorize");
    assert_eq!(json["correlation"], correlation.to_string());
    assert_eq!(json["retryable"], false);

    // A defect must be distinguished from bad input, and must not invite a retry: retrying
    // a run that trips an invariant hides the defect.
    let defect = NcError::new(ErrorKind::Internal {
        operation: "runtime::start".to_owned(),
        reason: "invariant violated".to_owned(),
    });
    assert!(defect.is_defect());
    assert!(!defect.is_retryable());
    assert_eq!(defect.code(), "internal");
}

#[test]
fn capability_sets_behave_as_a_partial_order() {
    // This is the predicate the whole authorization model rests on. If it is not a partial
    // order, "you may do X" stops meaning anything.
    let a = CapabilitySet::empty().with(Capability::ReadLogs);
    let b = a.with(Capability::ReadMetrics);
    let c = b.with(Capability::RunTests);

    assert!(a.is_subset_of(a), "reflexive");
    assert!(a.is_subset_of(b) && b.is_subset_of(c), "transitive");
    assert!(!b.is_subset_of(a), "antisymmetric");
    assert_eq!(
        a.intersect(b),
        a,
        "intersection is the greatest lower bound"
    );
    assert_eq!(a.union(b), b, "union is the least upper bound");
}

#[test]
fn every_incoherent_limit_configuration_is_refused_rather_than_clamped() {
    let incoherent: [(&str, Limits); 8] = [
        (
            "request_timeout_ms",
            Limits {
                request_timeout_ms: 0,
                ..Limits::default()
            },
        ),
        (
            "max_workers",
            Limits {
                max_workers: 0,
                ..Limits::default()
            },
        ),
        (
            "max_in_flight_requests",
            Limits {
                max_in_flight_requests: 0,
                ..Limits::default()
            },
        ),
        (
            "max_worker_queue_depth",
            Limits {
                max_worker_queue_depth: 0,
                ..Limits::default()
            },
        ),
        (
            "log_ring_capacity",
            Limits {
                log_ring_capacity: 0,
                ..Limits::default()
            },
        ),
        (
            "max_windows",
            Limits {
                max_windows: 0,
                ..Limits::default()
            },
        ),
        (
            "agent_rate_burst",
            Limits {
                agent_rate_per_second: 100,
                agent_rate_burst: 50,
                ..Limits::default()
            },
        ),
        (
            "max_ipc_frame_bytes",
            Limits {
                max_ipc_frame_bytes: u32::MAX,
                ..Limits::default()
            },
        ),
    ];

    for (field, limits) in incoherent {
        let err = limits
            .validate()
            .expect_err(&format!("{field} must be refused"));
        assert!(
            err.to_string().contains(field),
            "the error for {field} must name the field, got: {err}"
        );
    }

    Limits::default()
        .validate()
        .expect("the defaults must themselves be coherent");
}

#[test]
fn version_negotiation_refuses_what_it_should() {
    // These are the compatibility promises the agent interface and the component ABI make.
    let stable = SemVer::parse("1.2.0").expect("valid");
    assert!(stable.is_compatible_with(&SemVer::parse("1.9.9").expect("valid")));
    assert!(!stable.is_compatible_with(&SemVer::parse("1.1.0").expect("valid")));
    assert!(!stable.is_compatible_with(&SemVer::parse("2.0.0").expect("valid")));
    assert!(
        !stable.is_compatible_with(&SemVer::parse("1.0.0-rc.1").expect("valid")),
        "a stable consumer must not silently bind to a pre-release"
    );
}
