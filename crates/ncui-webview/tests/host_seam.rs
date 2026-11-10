//! Integration tests for the host seam, driven only through the public surface.
//!
//! The unit tests live beside each module and can reach private helpers. These cannot: every
//! type and function here comes through `ncui_webview`'s public API, so the three modules —
//! lifecycle, navigation and capability — are exercised together the way a host actually uses
//! them, and a module that drifts behind a private helper is caught here rather than by a
//! reader who notices the seam no longer means what its docs say.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use ncui_core::{Capability, CapabilitySet};
use ncui_webview::capability::CapabilityRequest;
use ncui_webview::lifecycle::HostState;
use ncui_webview::navigation::{NavigationPolicy, Origin, Scheme};
use ncui_webview::seam::{HostSeam, SeamError, SeamMessage, SeamResponse};
use ncui_webview::{CapabilityGrant, NavigationRequest, WindowLifecycle};
use serde_json::{Value, json};

/// A host that is attached and running, with one action and one allowed origin.
fn running_host() -> HostSeam {
    let lifecycle = WindowLifecycle::new();
    lifecycle.transition(HostState::Created).expect("created");
    lifecycle
        .transition(HostState::Attaching)
        .expect("attaching");
    lifecycle.transition(HostState::Running).expect("running");

    let policy = NavigationPolicy::new(&[
        Origin::new(Scheme::Https, "app.example.com", None),
        Origin::new(Scheme::Https, "cdn.example.com", None),
    ]);

    HostSeam::new(
        lifecycle,
        CapabilityGrant::of(&[Capability::InspectRuntime]),
        policy,
    )
    .with_action(
        "describe_metrics",
        CapabilitySet::empty().with(Capability::InspectRuntime),
    )
    .with_action(
        "purge_logs",
        CapabilitySet::empty().with(Capability::DeleteData),
    )
}

/// Counts dispatcher calls, so a refusal can be shown to have happened *before* dispatch.
fn counting_dispatcher() -> (
    impl FnMut(&CapabilityRequest, Value) -> Result<Value, SeamError>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink = std::sync::Arc::clone(&calls);
    (
        move |_request: &CapabilityRequest, _value: Value| {
            sink.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(json!({ "ok": true }))
        },
        calls,
    )
}

#[test]
fn a_granted_action_reaches_the_dispatcher_and_is_answered() {
    let host = running_host();
    let (mut dispatch, calls) = counting_dispatcher();

    let response = host.handle(
        &SeamMessage::Perform {
            name: "describe_metrics".to_owned(),
            payload: json!({ "probe": 7 }),
        },
        &mut dispatch,
    );

    assert!(response.ok, "a granted action must be served: {response:?}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(response.payload, Some(json!({ "ok": true })));
}

#[test]
fn an_ungranted_action_is_refused_before_the_dispatcher_is_reached() {
    // `purge_logs` is declared but requires `DeleteData`, which this page was never granted.
    // Declaring it makes it *known*, not permitted, which is the distinction the default-empty
    // grant depends on.
    let host = running_host();
    let (mut dispatch, calls) = counting_dispatcher();

    let response = host.handle(
        &SeamMessage::Perform {
            name: "purge_logs".to_owned(),
            payload: Value::Null,
        },
        &mut dispatch,
    );

    assert!(
        !response.ok,
        "an ungranted action must be refused: {response:?}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "the refusal must happen before dispatch, not after"
    );
    assert!(
        response.outcome.contains("capability_denied"),
        "{}",
        response.outcome
    );
}

#[test]
fn an_unknown_action_is_refused_before_the_dispatcher_is_reached() {
    let host = running_host();
    let (mut dispatch, calls) = counting_dispatcher();

    let response = host.handle(
        &SeamMessage::Perform {
            name: "install_dependencies".to_owned(),
            payload: Value::Null,
        },
        &mut dispatch,
    );

    assert!(!response.ok);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a page must not be able to reach an action this host never declared"
    );
}

#[test]
fn a_page_cannot_ask_the_runtime_for_a_method_by_name() {
    // The seam's central claim: a page names an *action*, and the seam decides what it
    // requires. If a page could dispatch by runtime method name it would be the JSON-RPC
    // agent surface with the capability checks removed.
    let host = running_host();
    let (mut dispatch, calls) = counting_dispatcher();

    for probe in [
        "ncui.logs.purge",
        "dispatch_with",
        "ncui.describe",
        "runtime.shutdown",
    ] {
        let response = host.handle(
            &SeamMessage::Perform {
                name: probe.to_owned(),
                payload: Value::Null,
            },
            &mut dispatch,
        );
        assert!(!response.ok, "`{probe}` was accepted as an action");
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn describing_advertises_the_grant_and_nothing_more() {
    let host = running_host();
    let response = host.handle(&SeamMessage::Describe, &mut |_, _| Ok(Value::Null));
    assert!(response.ok);

    let payload = response.payload.expect("a payload");
    let names: Vec<&str> = payload["actions"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|a| a["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        names,
        vec!["describe_metrics", "purge_logs"],
        "both declared actions are advertised; a page learns what exists, and separately \
         whether it may use it"
    );
}

#[test]
fn a_navigation_probe_agrees_with_the_policy_for_a_real_url_with_a_path() {
    // The path, query and fragment must not become part of the host. An earlier version
    // compared `app.example.com/next` against an allow-list entry of `app.example.com`,
    // which refused every real page.
    let host = running_host();
    for url in [
        "https://app.example.com",
        "https://app.example.com/",
        "https://app.example.com/next",
        "https://app.example.com/next?a=1#frag",
    ] {
        let response = host.handle(
            &SeamMessage::ProbeNavigation {
                url: url.to_owned(),
            },
            &mut |_, _| Ok(Value::Null),
        );
        assert!(
            response.ok,
            "{url} should be allowed; got {}",
            response.outcome
        );
    }
}

#[test]
fn a_navigation_probe_refuses_what_the_policy_refuses() {
    let host = running_host();
    for url in [
        "https://evil.test/steal",
        "javascript://app.example.com/alert(1)",
        "file://c:/windows/system32",
        "data:text/html,<script>alert(1)</script>",
        "http://app.example.com",
    ] {
        let response = host.handle(
            &SeamMessage::ProbeNavigation {
                url: url.to_owned(),
            },
            &mut |_, _| Ok(Value::Null),
        );
        assert!(
            !response.ok,
            "{url} should be refused; got {:?}",
            response.payload
        );
    }
}

#[test]
fn a_malformed_url_is_refused_rather_than_guessed_at() {
    let host = running_host();
    for url in ["", "no-scheme", "https://", "://app.example.com"] {
        let response = host.handle(
            &SeamMessage::ProbeNavigation {
                url: url.to_owned(),
            },
            &mut |_, _| Ok(Value::Null),
        );
        assert!(!response.ok, "{url:?} should be refused");
    }
}

#[test]
fn a_crashed_host_answers_nothing_at_all() {
    // ADR-001 calls the crash a real failure mode. The enforcement is that a dead host serves
    // nothing — not that it serves stale data, and not that it answers from a page that is
    // gone.
    let lifecycle = WindowLifecycle::new();
    lifecycle.transition(HostState::Created).expect("created");
    lifecycle
        .transition(HostState::Attaching)
        .expect("attaching");
    lifecycle.transition(HostState::Running).expect("running");
    lifecycle.transition(HostState::Crashed).expect("crash");

    let host = HostSeam::new(
        lifecycle,
        CapabilityGrant::of(&[Capability::InspectRuntime]),
        NavigationPolicy::new(&[Origin::new(Scheme::Https, "app.example.com", None)]),
    )
    .with_action(
        "describe_metrics",
        CapabilitySet::empty().with(Capability::InspectRuntime),
    );

    let (mut dispatch, calls) = counting_dispatcher();
    for message in [
        SeamMessage::Describe,
        SeamMessage::Perform {
            name: "describe_metrics".to_owned(),
            payload: Value::Null,
        },
        SeamMessage::ProbeNavigation {
            url: "https://app.example.com/".to_owned(),
        },
    ] {
        let response = host.handle(&message, &mut dispatch);
        assert!(!response.ok, "a crashed host served {message:?}");
        assert!(
            response.outcome.contains("unavailable"),
            "{}",
            response.outcome
        );
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn a_dispatch_failure_is_surfaced_rather_than_swallowed() {
    // A dispatcher that fails is a real answer the page must see; turning it into `ok` would
    // tell the page the work happened.
    let host = running_host();
    let response = host.handle(
        &SeamMessage::Perform {
            name: "describe_metrics".to_owned(),
            payload: Value::Null,
        },
        &mut |_, _| Err(SeamError::dispatch("the metrics registry is saturated")),
    );
    assert!(!response.ok);
    assert!(
        response.outcome.contains("dispatch_failed"),
        "{}",
        response.outcome
    );
    assert!(
        response.outcome.contains("saturated"),
        "the reason must survive: {}",
        response.outcome
    );
}

#[test]
fn page_json_round_trips_through_the_seam() {
    // The seam is the wire format for page content, so a message a page can send has to
    // survive serialisation rather than only working when constructed in Rust.
    let host = running_host();
    let (mut dispatch, calls) = counting_dispatcher();

    let raw = json!({
        "action": "perform",
        "name": "describe_metrics",
    });
    let message = SeamMessage::parse(&serde_json::to_string(&raw).expect("serialise"))
        .expect("a well-formed message parses");
    let response = host.handle(&message, &mut dispatch);

    assert!(response.ok, "{response:?}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn narrowing_a_page_grant_takes_effect_at_the_seam() {
    // The grant is per-host policy, so narrowing it must actually remove reachability rather
    // than only change what `describe` reports.
    let lifecycle = WindowLifecycle::new();
    lifecycle.transition(HostState::Created).expect("created");
    lifecycle
        .transition(HostState::Attaching)
        .expect("attaching");
    lifecycle.transition(HostState::Running).expect("running");

    let broad = CapabilityGrant::of(&[Capability::InspectRuntime, Capability::DeleteData]);
    let narrow = broad.narrowed_to(CapabilitySet::empty().with(Capability::InspectRuntime));

    let host = HostSeam::new(lifecycle, narrow, NavigationPolicy::new(&[])).with_action(
        "purge_logs",
        CapabilitySet::empty().with(Capability::DeleteData),
    );

    let (mut dispatch, calls) = counting_dispatcher();
    let response = host.handle(
        &SeamMessage::Perform {
            name: "purge_logs".to_owned(),
            payload: Value::Null,
        },
        &mut dispatch,
    );
    assert!(!response.ok, "narrowing the grant must remove reachability");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn a_seam_built_over_a_dead_lifecycle_refuses_from_the_first_call() {
    // A host that was destroyed rather than crashed is refused the same way, so the reason a
    // page is not served does not leak whether the engine died or was closed deliberately.
    let lifecycle = WindowLifecycle::new();
    lifecycle.transition(HostState::Created).expect("created");
    lifecycle
        .transition(HostState::Attaching)
        .expect("attaching");
    lifecycle.transition(HostState::Running).expect("running");
    lifecycle
        .transition(HostState::Destroying)
        .expect("tear down");
    lifecycle
        .transition(HostState::Destroyed)
        .expect("destroyed");

    let host = HostSeam::new(
        lifecycle,
        CapabilityGrant::none(),
        NavigationPolicy::new(&[]),
    );
    let response: SeamResponse = host.handle(&SeamMessage::Describe, &mut |_, _| Ok(Value::Null));
    assert!(!response.ok);
    assert!(
        response.outcome.contains("unavailable"),
        "{}",
        response.outcome
    );
}

/// The navigation policy is public and directly usable, so its behaviour is asserted here as
/// well as in the module's own tests — this is the type a host author actually reaches for.
#[test]
fn the_public_navigation_policy_refuses_the_escalation_shapes() {
    let policy = NavigationPolicy::new(&[Origin::new(Scheme::Https, "app.example.com", None)]);
    let top = |target: Origin| NavigationRequest {
        target,
        top_level: true,
    };

    assert!(
        policy
            .evaluate(&top(Origin::new(Scheme::Https, "app.example.com", None)))
            .is_allowed()
    );
    for hostile in [
        Origin::new(Scheme::JavaScript, "app.example.com", None),
        Origin::new(Scheme::Data, "app.example.com", None),
        Origin::new(Scheme::File, "c:/windows", None),
        Origin::new(Scheme::Http, "app.example.com", None),
        Origin::new(Scheme::Https, "notapp.example.com", None),
        Origin::new(Scheme::Https, "app.example.com.evil.test", None),
    ] {
        assert!(
            !policy.evaluate(&top(hostile.clone())).is_allowed(),
            "{hostile} was allowed"
        );
    }
}
