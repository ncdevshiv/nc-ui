//! Integration tests for the MCP adapter.
//!
//! The point of these is that the adapter adds *no policy*. Every assertion here is about
//! either a translation (name mapping, request/response shape) or an inherited guarantee —
//! a refusal that the JSON-RPC surface already produces must arrive at an MCP client as a
//! refusal, not as a successful tool call.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use ncui_core::{Capability, CapabilitySet, Limits};
use ncui_runtime::mcp::{
    call_tool, handle_envelope, method_for_tool, tool_name_for, tool_table, tools_list_response,
};
use ncui_runtime::rpc::Request;
use ncui_runtime::{Runtime, agent::AgentSession, dispatch};
use ncui_security::authz::AgentRole;
use ncui_security::token::SecretToken;
use serde_json::{Value, json};
use std::time::Duration;

/// A runtime plus an authenticated session, which is the minimum an adapter call needs.
struct Harness {
    runtime: Runtime,
    session: AgentSession,
    table: Vec<ncui_runtime::describe::MethodDescription>,
}

impl Harness {
    fn with_role(role: AgentRole) -> Harness {
        let limits = Limits {
            agent_rate_per_second: 1_000,
            agent_rate_burst: 1_000,
            ..Limits::default()
        };
        let runtime = Runtime::start(limits).expect("runtime");
        let mut session = AgentSession::new("mcp-test", role, 1_000, 1_000).expect("session");
        let token = SecretToken::new(vec![0x11; 32]).expect("token");
        session
            .authenticate(&token, &token)
            .expect("an authenticated session is required before dispatch");
        Harness {
            runtime,
            session,
            table: dispatch::method_table(),
        }
    }

    /// Calls one tool and returns the MCP content block.
    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        call_tool(
            &mut self.runtime,
            &mut self.session,
            tool,
            arguments,
            Duration::from_millis(0),
            None,
            &self.table,
        )
    }
}

#[test]
fn a_tool_name_round_trips_to_the_json_rpc_method_it_stands_for() {
    for method in dispatch::method_table() {
        let tool = tool_name_for(&method.name);
        assert!(
            tool.starts_with("ncui_"),
            "{} must be namespaced to this runtime, got {tool}",
            method.name
        );
        assert!(
            !tool.contains('.'),
            "an MCP tool name must not contain a dot: {tool}"
        );
        assert_eq!(
            method_for_tool(&tool).as_deref(),
            Some(method.name.as_str()),
            "{tool} must map back to {}",
            method.name
        );
    }
}

/// The dots-to-underscores mapping is only lossless while no method segment contains an
/// underscore. If one ever does, its tool name would resolve to a *different* method and no
/// test would notice — so the invariant is asserted here rather than assumed in a comment.
#[test]
fn method_segments_are_reversible() {
    for method in dispatch::method_table() {
        assert!(
            !method.name.contains('_'),
            "{} contains an underscore, so its tool name would be indistinguishable from a \
             method whose segments differ only at a dot; the mapping would resolve to the \
             wrong method",
            method.name
        );
    }
}

#[test]
fn a_tool_name_that_is_not_in_the_table_is_refused_rather_than_dispatched() {
    let mut harness = Harness::with_role(AgentRole::Administrator);
    // Translating is total, but a name with no method behind it must not reach dispatch as a
    // guessed method: the refusal comes from the surface's own lookup.
    for foreign in ["ncui_no_such_method", "some_other_server_tool"] {
        let result = harness.call(foreign, json!({}));
        assert_eq!(
            result["isError"],
            json!(true),
            "{foreign:?} names no method and must be refused, not dispatched: {result}"
        );
    }
}

#[test]
fn every_advertised_method_appears_as_a_tool() {
    let methods = dispatch::method_table();
    let tools = tool_table(&methods);
    assert_eq!(
        tools.len(),
        methods.len(),
        "the adapter must expose every method, not a hand-maintained subset"
    );
    for method in &methods {
        let tool = tools
            .iter()
            .find(|tool| tool.name == tool_name_for(&method.name))
            .unwrap_or_else(|| panic!("{} has no tool", method.name));
        assert_eq!(
            tool.description, method.summary,
            "a tool's description must be the method's own summary"
        );
        assert_eq!(
            tool.destructive, method.destructive,
            "{} must advertise its destructiveness, because that is what tells a client \
             consent is required",
            method.name
        );
    }
}

#[test]
fn a_tool_call_returns_the_result_the_json_rpc_surface_would_return() {
    let mut harness = Harness::with_role(AgentRole::Administrator);
    let over_mcp = harness.call("ncui_describe", json!({}));
    assert_eq!(
        over_mcp["isError"],
        json!(false),
        "an authenticated describe must succeed: {over_mcp}"
    );

    // The same call over the JSON-RPC surface, so the payloads can be compared.
    let request = Request::new(json!(1), "ncui.describe", json!({}));
    let over_jsonrpc = dispatch::dispatch_with(
        &mut harness.runtime,
        &mut harness.session,
        &request,
        Duration::from_millis(0),
        None,
        &harness.table,
    )
    .expect("a response");
    assert_eq!(
        over_mcp["structuredContent"],
        over_jsonrpc.result.expect("a result to translate"),
        "the adapter must translate the result, not reshape it"
    );
}

#[test]
fn a_refusal_arrives_as_a_tool_error_rather_than_a_successful_call() {
    let mut harness = Harness::with_role(AgentRole::Observer);

    // `ncui.logs.purge` is destructive: the capability is denied for an Observer, and the
    // adapter must not present that denial as a successful tool invocation.
    let result = harness.call("ncui_logs_purge", json!({}));
    assert_eq!(
        result["isError"],
        json!(true),
        "a refused call must be reported as an error, not a success: {result}"
    );
    let code = &result["structuredContent"]["ncui_code"];
    assert_eq!(
        code, "capability_denied",
        "the stable error code must survive translation: {result}"
    );
    // And the human-readable content names the same code, so a client reading only text is
    // not told a different story than one reading the structured payload.
    let text = result["content"][0]["text"].as_str().expect("text content");
    assert!(
        text.contains("capability_denied"),
        "the text content must name the same code as the structured payload: {text}"
    );
}

#[test]
fn a_destructive_call_without_consent_is_refused_through_the_adapter_too() {
    let mut harness = Harness::with_role(AgentRole::Administrator);
    let without = harness.call("ncui_logs_purge", json!({}));
    assert_eq!(
        without["isError"],
        json!(true),
        "consent is required for a destructive operation: {without}"
    );
    // And a minted consent token serves it, which is what proves the argument reached the
    // gate rather than being dropped by the translation. The legacy `consent: true`
    // authorizes nothing destructive; the token is the consent.
    let correlation = ncui_core::CorrelationId::new();
    let token = harness
        .runtime
        .mint_consent(
            correlation,
            "ncui.logs.purge",
            CapabilitySet::empty().with(Capability::ReadLogs),
        )
        .expect("mint");
    let with = harness.call(
        "ncui_logs_purge",
        json!({ "consent_token": token.to_value() }),
    );
    assert_eq!(
        with["isError"],
        json!(false),
        "an explicit consent must be honoured: {with}"
    );
}

#[test]
fn the_tools_list_envelope_names_the_protocol_and_every_tool() {
    let methods = dispatch::method_table();
    let listed = tools_list_response(&methods);
    assert_eq!(
        listed["protocolVersion"],
        json!("2024-11-05"),
        "an MCP client negotiates against this revision"
    );
    assert_eq!(
        listed["tools"].as_array().expect("array").len(),
        methods.len()
    );
}

#[test]
fn the_envelope_dispatches_tools_list_and_tools_call() {
    let mut harness = Harness::with_role(AgentRole::Administrator);

    let list = Request::new(json!(1), "tools/list", json!({}));
    let listed = handle_envelope(
        &mut harness.runtime,
        &mut harness.session,
        &list,
        Duration::from_millis(0),
        None,
        &harness.table,
    )
    .expect("a response");
    assert!(
        listed.error.is_none(),
        "tools/list must succeed: {:?}",
        listed.error
    );
    assert!(
        !listed.result.expect("result")["tools"]
            .as_array()
            .expect("array")
            .is_empty()
    );

    let call = Request::new(
        json!(2),
        "tools/call",
        json!({ "name": "ncui_describe", "arguments": {} }),
    );
    let called = handle_envelope(
        &mut harness.runtime,
        &mut harness.session,
        &call,
        Duration::from_millis(0),
        None,
        &harness.table,
    )
    .expect("a response");
    assert!(called.error.is_none(), "tools/call must succeed");
    assert_eq!(
        called.result.expect("result")["isError"],
        json!(false),
        "the envelope must carry the same error flag the direct call reports"
    );
}

#[test]
fn an_unknown_envelope_method_is_a_structured_error() {
    let mut harness = Harness::with_role(AgentRole::Administrator);
    let request = Request::new(json!(1), "tools/teleport", json!({}));
    let response = handle_envelope(
        &mut harness.runtime,
        &mut harness.session,
        &request,
        Duration::from_millis(0),
        None,
        &harness.table,
    )
    .expect("a response");
    let error = response.error.expect("an unknown method must be refused");
    assert_eq!(
        error.data.ncui_code, "validation",
        "an unknown method is a structured validation error, not a bare string"
    );
}

#[test]
fn an_unknown_tool_is_refused_rather_than_silently_translated() {
    let mut harness = Harness::with_role(AgentRole::Administrator);
    // A name with our prefix but no method behind it. The adapter must not invent a method
    // and dispatch it, because that would turn a typo into whatever it happened to name.
    let result = harness.call("ncui_no_such_method", json!({}));
    assert_eq!(
        result["isError"],
        json!(true),
        "an unknown tool must be refused: {result}"
    );
}

#[test]
fn an_adapter_call_produces_an_audit_event() {
    // The adapter's whole safety argument is that it adds no policy of its own, so a call it
    // makes must be audited exactly as if the JSON-RPC client had made it directly.
    let mut harness = Harness::with_role(AgentRole::Administrator);
    let before = harness.runtime.audit.len();
    let _ = harness.call("ncui_describe", json!({}));
    assert!(
        harness.runtime.audit.len() > before,
        "an adapter call must leave an audit trail; without it the adapter would be an \
         unaudited side door into the runtime"
    );
}

#[test]
fn the_adapter_is_usable_from_several_threads_at_once() {
    // A translation layer with no shared mutable state should be callable concurrently; this
    // is what proves it holds no hidden per-call state.
    let handles: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                let mut harness = Harness::with_role(AgentRole::Administrator);
                harness.call("ncui_describe", json!({}))["isError"].clone()
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(
            handle.join().expect("thread").clone(),
            json!(false),
            "a concurrent adapter call must still succeed"
        );
    }
}

/// The adapter must not need a running server, which is what lets it be tested at all.
#[test]
fn an_adapter_call_needs_no_socket() {
    let mut harness = Harness::with_role(AgentRole::Administrator);
    let result = harness.call("ncui_health", json!({}));
    assert_eq!(result["isError"], json!(false), "{result}");
}
