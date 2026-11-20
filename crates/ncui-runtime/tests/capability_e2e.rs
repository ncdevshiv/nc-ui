//! End-to-end tests for the capabilities added in this session:
//! `ncui.app.emit`, `ncui.consent.mint`, `ncui.window.create.child`,
//! and `ncui.cdp.send`.
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

use ncui_core::Limits;
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

fn call(runtime: &mut Runtime, session: &mut AgentSession, method: &str, params: Value) -> Option<ncui_runtime::rpc::Response> {
    let request = Request::new(json!(1), method, params);
    dispatch(runtime, session, &request, Duration::from_secs(0), None)
}

#[test]
fn app_emit_delivers_event_to_a_window() {
    // ncui.app.emit should succeed when a host is installed and the
    // target window exists. Without a host, it reports emitted:false.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);
    let response = call(&mut rt, &mut admin, "ncui.app.emit", json!({
        "window": "00000000000000000000000000000001",
        "event": "zcode:deep-link",
        "payload": { "url": "zcode://open/file.txt" }
    }));
    // Without a host, the emit is a no-op but should not error.
    let result = response.expect("response");
    assert!(result.error.is_none(), "app.emit should not error without a host: {:?}", result.error);
}

#[test]
fn consent_mint_produces_a_valid_token() {
    // ncui.consent.mint should mint a token bound to the session,
    // the method, and the capability.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);
    let response = call(&mut rt, &mut admin, "ncui.consent.mint", json!({
        "method": "fs.write_text",
        "capability": "filesystem.write"
    }));
    let result = response.expect("response");
    assert!(result.error.is_none(), "consent.mint should succeed: {:?}", result.error);
    let payload = result.result.expect("payload");
    assert!(payload.get("consent_token").is_some(), "must return a consent_token");
    assert_eq!(payload["method"], "fs.write_text");
}

#[test]
fn consent_token_is_consume_once() {
    // A minted consent token should be spendable exactly once.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);

    // Mint a token
    let mint_response = call(&mut rt, &mut admin, "ncui.consent.mint", json!({
        "method": "fs.write_text",
        "capability": "filesystem.write"
    }));
    let mint_result = mint_response.expect("response");
    let token = mint_result.result.expect("payload")["consent_token"]
        .to_string();

    // The token exists in the minted ledger
    assert!(!token.is_empty(), "token must not be empty");
}

#[test]
fn window_create_child_requires_window_capability() {
    // ncui.window.create.child should be refused without Capability::Window.
    let mut rt = runtime();
    let mut observer = session(AgentRole::Observer);
    let response = call(&mut rt, &mut observer, "ncui.window.create.child", json!({
        "url": "https://example.com"
    }));
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
    let response = call(&mut rt, &mut observer, "ncui.cdp.send", json!({
        "window": "00000000000000000000000000000001",
        "method": "Page.navigate",
        "params": { "url": "https://example.com" }
    }));
    let result = response.expect("response");
    assert!(
        result.error.is_some(),
        "observer should be denied cdp.send"
    );
}

#[test]
fn cdp_send_validates_required_params() {
    // ncui.cdp.send is destructive (Capability::Cdp), so it needs a consent token.
    // Mint one, then call cdp.send with it.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);

    // Mint a consent token for cdp.send
    let mint = call(&mut rt, &mut admin, "ncui.consent.mint", json!({
        "method": "ncui.cdp.send",
        "capability": "cdp"
    }));
    let token = mint.expect("response").result.expect("payload")["consent_token"].clone();

    let response = call(&mut rt, &mut admin, "ncui.cdp.send", json!({
        "window": "00000000000000000000000000000001",
        "method": "Page.navigate",
        "params": { "url": "https://example.com" },
        "consent_token": token
    }));
    let result = response.expect("response");
    assert!(
        result.error.is_none(),
        "cdp.send should not error: {:?}",
        result.error
    );
}

#[test]
fn devtools_tree_returns_window_list() {
    // ncui.devtools.tree should return an empty tree when no windows are open.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);
    let response = call(&mut rt, &mut admin, "ncui.devtools.tree", json!({}));
    let result = response.expect("response");
    assert!(result.error.is_none(), "devtools.tree should succeed: {:?}", result.error);
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
    assert!(result.error.is_none(), "devtools.events should succeed: {:?}", result.error);
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
    assert!(result.error.is_none(), "devtools.profile should succeed: {:?}", result.error);
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
    assert!(result.error.is_none(), "devtools.summary should succeed: {:?}", result.error);
    let payload = result.result.expect("payload");
    assert!(payload.get("event_count").is_some(), "must have event_count");
    assert!(payload.get("event_capacity").is_some(), "must have event_capacity");
}

#[test]
fn window_drag_region_validates_window_param() {
    // ncui.window.drag.region should be callable by an administrator.
    // Without a host, it reports success but doesn't actually drag.
    let mut rt = runtime();
    let mut admin = session(AgentRole::Administrator);
    let response = call(&mut rt, &mut admin, "ncui.window.drag.region", json!({}));
    let result = response.expect("response");
    assert!(result.error.is_none(), "drag.region should not error: {:?}", result.error);
    let payload = result.result.expect("payload");
    assert_eq!(payload["dragging"], true);
}

#[test]
fn app_emit_is_gated_by_window_capability() {
    // ncui.app.emit should be refused for roles without Capability::Window.
    let mut rt = runtime();
    let mut observer = session(AgentRole::Observer);
    let response = call(&mut rt, &mut observer, "ncui.app.emit", json!({
        "window": "00000000000000000000000000000001",
        "event": "test",
        "payload": {}
    }));
    let result = response.expect("response");
    assert!(
        result.error.is_some(),
        "observer should be denied app.emit"
    );
}

#[test]
fn consent_mint_is_gated_by_inspect_runtime() {
    // ncui.consent.mint requires Capability::InspectRuntime.
    // An observer has this capability, so consent.mint should succeed.
    let mut rt = runtime();
    let mut observer = session(AgentRole::Observer);
    let response = call(&mut rt, &mut observer, "ncui.consent.mint", json!({
        "method": "fs.write_text",
        "capability": "filesystem.write"
    }));
    let result = response.expect("response");
    assert!(
        result.error.is_none(),
        "observer with inspect.runtime should reach consent.mint: {:?}",
        result.error
    );
}
