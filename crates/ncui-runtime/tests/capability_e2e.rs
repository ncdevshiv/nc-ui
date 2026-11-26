//! End-to-end tests for application events, child windows, CDP, and consent binding.
//!
//! Each test drives the real dispatch pipeline — the same pipeline an
//! agent's request or a page's bridge message goes through — and
//! asserts the observable outcome.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::time::Duration;

use ncui_core::{Capability, CapabilitySet, CorrelationId, Limits};
use ncui_runtime::Runtime;
use ncui_runtime::agent::AgentSession;
use ncui_runtime::dispatch::dispatch;
use ncui_runtime::rpc::Request;
use ncui_security::authz::AgentRole;
use ncui_security::token::SecretToken;
use serde_json::{Value, json};

fn runtime() -> Runtime {
    Runtime::start(Limits::default()).expect("runtime")
}

fn test_token() -> SecretToken {
    SecretToken::from_os_entropy(32).expect("token")
}

fn session(role: AgentRole) -> AgentSession {
    let token = test_token();
    let mut s = AgentSession::new("test-agent", role, 100, 100).expect("session");
    s.authenticate(&token, &token).expect("auth");
    s
}

fn call(
    runtime: &mut Runtime,
    session: &mut AgentSession,
    method: &str,
    params: Value,
) -> Option<ncui_runtime::rpc::Response> {
    let request = Request::new(json!(1), method, params);
    dispatch(runtime, session, &request, Duration::from_secs(0), None)
}

#[test]
fn app_emit_refuses_an_unknown_window() {
    // An emit to a window that is not open is an error naming the window, not
    // an `emitted` success the page never received.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);
    let response = call(
        &mut rt,
        &mut admin,
        "ncui.app.emit",
        json!({
            "window": "00000000000000000000000000000001",
            "event": "zcode:deep-link",
            "payload": { "url": "zcode://open/file.txt" }
        }),
    );
    let result = response.expect("response");
    assert!(
        result.error.is_some(),
        "emit to an unknown window must be refused, got: {:?}",
        result.result
    );
}

#[test]
fn app_emit_broadcasts_when_no_window_is_named() {
    // Omitting `window` (or passing "all") emits to every open window; with
    // none open, the event reaches nobody and the answer says so.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);
    let response = call(
        &mut rt,
        &mut admin,
        "ncui.app.emit",
        json!({
            "event": "zcode:updater",
            "payload": { "version": "1.2.3" }
        }),
    );
    let result = response.expect("response");
    assert!(
        result.error.is_none(),
        "a broadcast with no windows must succeed: {:?}",
        result.error
    );
    let payload = result.result.expect("payload");
    assert_eq!(payload["emitted"], 0);
}

#[test]
fn a_client_cannot_mint_its_own_consent() {
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);
    let response = call(
        &mut rt,
        &mut admin,
        "ncui.consent.mint",
        json!({
            "method": "ncui.logs.purge",
            "capability": "read.logs"
        }),
    );
    let error = response.expect("response").error.expect("an error");
    assert_eq!(error.data.ncui_code, "not_found");
    assert!(
        ncui_runtime::dispatch::method_table()
            .iter()
            .all(|method| method.name != "ncui.consent.mint"),
        "a self-service mint method must not be advertised"
    );
}

#[test]
fn a_host_issued_consent_token_is_bound_to_its_subject_and_operation() {
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);
    let params = json!({});
    let token = rt
        .mint_consent(
            CorrelationId::new(),
            "test-agent",
            "ncui.logs.purge",
            CapabilitySet::empty().with(Capability::ReadLogs),
            &params,
        )
        .expect("mint");

    let refused = call(
        &mut rt,
        &mut admin,
        "ncui.logs.purge",
        json!({ "reason": "not approved", "consent_token": token.to_value() }),
    );
    assert_eq!(
        refused
            .expect("response")
            .error
            .expect("altered parameters must be refused")
            .data
            .ncui_code,
        "capability_denied"
    );

    let mut allowed_params = params;
    allowed_params["consent_token"] = token.to_value();
    let allowed = call(&mut rt, &mut admin, "ncui.logs.purge", allowed_params);
    assert!(
        allowed.expect("response").error.is_none(),
        "the refused alteration must not spend the exact operation's token"
    );
}

#[test]
fn window_create_child_requires_window_capability() {
    // ncui.window.create.child should be refused without Capability::Window.
    let mut rt = runtime();
    let mut observer = session(AgentRole::Observer);
    let response = call(
        &mut rt,
        &mut observer,
        "ncui.window.create.child",
        json!({
            "url": "https://example.com"
        }),
    );
    let result = response.expect("response");
    assert!(
        result.error.is_some(),
        "observer should be denied window.create.child"
    );
}

#[test]
fn cdp_send_requires_cdp_capability() {
    // ncui.cdp.send should be refused without Capability::Cdp.
    let mut rt = runtime();
    let mut observer = session(AgentRole::Observer);
    let response = call(
        &mut rt,
        &mut observer,
        "ncui.cdp.send",
        json!({
            "window": "00000000000000000000000000000001",
            "method": "Page.navigate",
            "params": { "url": "https://example.com" }
        }),
    );
    let result = response.expect("response");
    assert!(result.error.is_some(), "observer should be denied cdp.send");
}

#[test]
fn cdp_send_refuses_an_unknown_window() {
    // ncui.cdp.send is destructive (Capability::Cdp), so it needs a consent token.
    // A command aimed at a window that is not open is a named refusal, not a
    // `sent: true` that nothing received.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);

    let mut params = json!({
        "window": "00000000000000000000000000000001",
        "method": "Page.navigate",
        "params": { "url": "https://example.com" }
    });
    let token = rt
        .mint_consent(
            CorrelationId::new(),
            "test-agent",
            "ncui.cdp.send",
            CapabilitySet::empty().with(Capability::Cdp),
            &params,
        )
        .expect("mint for the exact command");
    params["consent_token"] = token.to_value();

    let result = call(&mut rt, &mut admin, "ncui.cdp.send", params).expect("response");
    assert_eq!(
        result
            .error
            .expect("cdp.send to an unknown window must be refused")
            .data
            .ncui_code,
        "not_found"
    );
}

#[test]
fn cdp_send_requires_window_and_method() {
    // The window and the CDP method are both required: a silent default window
    // or a default `Page.navigate` would send a command the caller never asked
    // for, to a window the caller never named.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);

    for mut params in [
        json!({ "method": "Page.navigate" }),
        json!({ "window": "00000000000000000000000000000001" }),
    ] {
        let token = rt
            .mint_consent(
                CorrelationId::new(),
                "test-agent",
                "ncui.cdp.send",
                CapabilitySet::empty().with(Capability::Cdp),
                &params,
            )
            .expect("mint for this exact malformed operation");
        params["consent_token"] = token.to_value();
        let result = call(&mut rt, &mut admin, "ncui.cdp.send", params).expect("response");
        assert_eq!(
            result
                .error
                .expect("cdp.send without window and method must be refused")
                .data
                .ncui_code,
            "validation"
        );
    }
}

#[test]
fn devtools_tree_returns_window_list() {
    // ncui.devtools.tree should return an empty tree when no windows are open.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);
    let response = call(&mut rt, &mut admin, "ncui.devtools.tree", json!({}));
    let result = response.expect("response");
    assert!(
        result.error.is_none(),
        "devtools.tree should succeed: {:?}",
        result.error
    );
    let payload = result.result.expect("payload");
    assert!(payload.get("windows").is_some(), "must have windows array");
    assert!(payload.get("uptime_ms").is_some(), "must have uptime");
}

#[test]
fn devtools_events_returns_empty_ring_when_no_events() {
    // ncui.devtools.events should return an empty list when no events have been recorded.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);
    let response = call(&mut rt, &mut admin, "ncui.devtools.events", json!({}));
    let result = response.expect("response");
    assert!(
        result.error.is_none(),
        "devtools.events should succeed: {:?}",
        result.error
    );
    let payload = result.result.expect("payload");
    assert!(payload.get("events").is_some(), "must have events array");
}

#[test]
fn devtools_profile_returns_empty_when_not_profiling() {
    // ncui.devtools.profile should return empty data when profiling is not active.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);
    let response = call(&mut rt, &mut admin, "ncui.devtools.profile", json!({}));
    let result = response.expect("response");
    assert!(
        result.error.is_none(),
        "devtools.profile should succeed: {:?}",
        result.error
    );
    let payload = result.result.expect("payload");
    assert!(payload.get("methods").is_some(), "must have methods array");
    assert_eq!(payload["total_calls"], 0, "no profiling calls yet");
}

#[test]
fn devtools_summary_reports_subsystem_state() {
    // ncui.devtools.summary should report event count, capacity, and profile stats.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);
    let response = call(&mut rt, &mut admin, "ncui.devtools.summary", json!({}));
    let result = response.expect("response");
    assert!(
        result.error.is_none(),
        "devtools.summary should succeed: {:?}",
        result.error
    );
    let payload = result.result.expect("payload");
    assert!(
        payload.get("event_count").is_some(),
        "must have event_count"
    );
    assert!(
        payload.get("event_capacity").is_some(),
        "must have event_capacity"
    );
}

#[test]
fn window_drag_region_refuses_when_there_is_nothing_to_drag() {
    // ncui.window.drag.region targets the calling window. With no window open
    // there is nothing to drag, and the refusal is reported rather than
    // answered with a drag that never happened.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);
    let response = call(&mut rt, &mut admin, "ncui.window.drag.region", json!({}));
    let result = response.expect("response");
    assert!(
        result.error.is_some(),
        "drag.region with no window must be refused, got: {:?}",
        result.result
    );
}

#[test]
fn app_emit_is_gated_by_window_capability() {
    // ncui.app.emit should be refused for roles without Capability::Window.
    let mut rt = runtime();
    let mut observer = session(AgentRole::Observer);
    let response = call(
        &mut rt,
        &mut observer,
        "ncui.app.emit",
        json!({
            "window": "00000000000000000000000000000001",
            "event": "test",
            "payload": {}
        }),
    );
    let result = response.expect("response");
    assert!(result.error.is_some(), "observer should be denied app.emit");
}

#[test]
fn a_consent_token_cannot_move_to_another_agent() {
    let mut rt = runtime();
    let mut owner = session(AgentRole::Administrator);
    let token = rt
        .mint_consent(
            CorrelationId::new(),
            "test-agent",
            "ncui.logs.purge",
            CapabilitySet::empty().with(Capability::ReadLogs),
            &json!({}),
        )
        .expect("mint");
    let credential = test_token();
    let mut other =
        AgentSession::new("other-agent", AgentRole::Administrator, 100, 100).expect("session");
    other
        .authenticate(&credential, &credential)
        .expect("authenticate");

    let refused = call(
        &mut rt,
        &mut other,
        "ncui.logs.purge",
        json!({ "consent_token": token.to_value() }),
    );
    assert_eq!(
        refused
            .expect("response")
            .error
            .expect("another subject must be refused")
            .data
            .ncui_code,
        "capability_denied"
    );
    let allowed = call(
        &mut rt,
        &mut owner,
        "ncui.logs.purge",
        json!({ "consent_token": token.to_value() }),
    );
    assert!(
        allowed.expect("response").error.is_none(),
        "the failed transfer must not spend the owner's token"
    );
}
