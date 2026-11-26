//! Integration tests for the agent-facing surface.
//!
//! These drive a real runtime through the real dispatch pipeline rather than calling
//! handlers directly, so they exercise the composition that the crate exists to provide:
//! a request entering, being authorized, running, and leaving with a correlated result.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use core::time::Duration;

use ncui_core::{Capability, CapabilitySet, CorrelationId, Limits, WindowId};
use ncui_runtime::Runtime;
use ncui_runtime::agent::AgentSession;
use ncui_runtime::dispatch::dispatch;
use ncui_runtime::rpc::{Request, Response};
use ncui_security::authz::AgentRole;
use ncui_webview::HostFactory;
use ncui_webview_child::ChildWebviewRequest;
use serde_json::{Value, json};

/// A test factory whose hosts always come up alive.
#[derive(Debug, Default)]
struct AlwaysReady;

impl HostFactory for AlwaysReady {
    fn spawn(
        &self,
        _window: WindowId,
        _request: &ncui_webview::WindowRequest,
        _seam: &std::sync::Arc<ncui_webview::HostSeam>,
    ) -> Option<Box<dyn ncui_webview::WebViewBackend>> {
        Some(Box::new(AlwaysAlive))
    }
}

#[derive(Debug)]
struct AlwaysAlive;

impl ncui_webview::WebViewBackend for AlwaysAlive {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn last_error(&self) -> Option<String> {
        None
    }
    fn is_alive(&self) -> bool {
        true
    }
    fn poll_page_message(&self) -> Option<ncui_webview::PageMessage> {
        None
    }
    fn evaluate_script(&self, _script: &str) -> bool {
        true
    }
    // A real engine asks the platform; this one answers the way a windowed, unfocused window does,
    // so the presentation and shape methods reach their handlers rather than being refused for an
    // engine that cannot speak.
    fn window_state(&self) -> Option<ncui_webview::WindowState> {
        Some(ncui_webview::WindowState::default())
    }
    fn set_window_state(
        &self,
        _change: ncui_webview::WindowChange,
    ) -> Option<ncui_webview::WindowState> {
        Some(ncui_webview::WindowState::default())
    }
    // Answered for the same reason as the presentation above: an engine that stayed silent would
    // turn an authorized request into a refusal, and this fixture exists to reach the handler.
    fn window_theme(&self) -> Option<ncui_webview::Theme> {
        Some(ncui_webview::Theme::Light)
    }
    fn set_window_shape(&self, _shape: &ncui_webview::WindowShape) -> bool {
        true
    }
    fn set_zoom(&self, factor: f64) -> bool {
        (0.25..=5.0).contains(&factor)
    }
    fn open_devtools(&self) -> bool {
        true
    }
    fn close_devtools(&self) -> bool {
        true
    }
    fn is_devtools_open(&self) -> Option<bool> {
        Some(false)
    }
    fn cdp_send(&self, _command: &Value) -> Result<Value, ncui_webview::CdpError> {
        // A capture answer carries the payload the method promises, so the window methods
        // that ride this transport see an honest engine answer.
        Ok(serde_json::json!({ "result": { "data": "QUJD" } }))
    }
    fn drag_window(&self) -> bool {
        true
    }

    fn create_child_webview(
        &self,
        _request: &ChildWebviewRequest,
        _seam: &std::sync::Arc<ncui_webview::HostSeam>,
        ipc_tx: std::sync::mpsc::SyncSender<ncui_webview::ChildPageMessage>,
    ) -> Result<u64, String> {
        drop(ipc_tx);
        Ok(1)
    }

    fn set_child_webview_bounds(
        &self,
        _child: u64,
        _request: &ChildWebviewRequest,
    ) -> Result<(), String> {
        Ok(())
    }

    fn close_child_webview(&self, _child: u64) -> Result<(), String> {
        Ok(())
    }
}

fn runtime() -> Runtime {
    let mut runtime = Runtime::start(Limits::default()).expect("runtime should start");
    runtime.windows = runtime
        .windows
        .with_registry(ncui_webview::WindowRegistry::new().with_default_navigation(
            ncui_webview::NavigationPolicy::new(&[ncui_webview::Origin::new(
                ncui_webview::Scheme::Https,
                "example.com",
                None,
            )]),
        ))
        .with_factory(Box::new(AlwaysReady));
    runtime
}

fn session(role: AgentRole) -> AgentSession {
    let token = ncui_security::token::SecretToken::new(vec![0x11; 32]).expect("token");
    let mut session = AgentSession::new("integration-agent", role, 1_000, 1_000).expect("session");
    session
        .authenticate(&token, &token)
        .expect("test session authenticates");
    session
}

fn call(
    runtime: &mut Runtime,
    session: &mut AgentSession,
    method: &str,
    params: Value,
) -> Response {
    let request = Request::new(json!(1), method, params);
    dispatch(runtime, session, &request, Duration::from_secs(0), None)
        .expect("a call with an id is never a notification")
}

/// The error model must survive the whole pipeline: a failure raised deep inside a
/// handler must arrive at the client as a structured, correlated, machine-readable error
/// rather than as a string.
#[test]
fn an_error_raised_inside_a_handler_survives_the_pipeline_intact() {
    let mut runtime = runtime();
    let mut session = session(AgentRole::Administrator);
    let request = Request::new(json!(42), "ncui.not_a_method", json!({}));
    let response = dispatch(
        &mut runtime,
        &mut session,
        &request,
        Duration::from_secs(0),
        None,
    )
    .expect("response");

    assert_eq!(response.id, json!(42), "the request id must be echoed");
    let error = response.error.clone().expect("an unknown method must fail");
    assert_eq!(error.data.ncui_code, "not_found");
    assert!(
        !error.data.retryable,
        "a not-found failure must not invite a retry"
    );
    assert!(
        error.data.correlation.is_some(),
        "a failure must carry the correlation id that joins it to its trace"
    );

    // The error must round-trip as JSON: an agent reads it, it is not decorative.
    let text = serde_json::to_string(&response).expect("responses must be serialisable");
    let back: Response = serde_json::from_str(&text).expect("responses must round-trip");
    assert_eq!(back, response);
}

/// The payload one advertised method is called with, when the method needs a window to act on.
///
/// Split out of the walk so the walk reads as the rule it checks; a method that acts on a window
/// needs the window that was opened earlier in the walk, and one that is called before any window
/// exists names a window that is not there so the refusal is the not-found path rather than a
/// malformed payload.
fn params_for(name: &str, created_window: Option<WindowId>) -> Value {
    let named = created_window.map_or_else(
        || "00000000000000000000000000000001".to_owned(),
        |id| id.to_string(),
    );
    match name {
        "ncui.window.create" | "ncui.window.create.child" => {
            json!({ "url": "https://example.com" })
        }
        "ncui.window.close"
        | "ncui.window.theme"
        | "ncui.window.drag.region"
        | "ncui.window.devtools.open"
        | "ncui.window.devtools.close"
        | "ncui.window.devtools.state"
        | "ncui.window.child.close" => {
            json!({ "window": named, "child": "child-0000000000000001" })
        }
        "ncui.window.child.create" => json!({
            "window": named,
            "url": "https://example.com",
            "x": 10,
            "y": 10,
            "width": 400,
            "height": 300
        }),
        "ncui.window.child.bounds" => json!({
            "window": named,
            "child": "child-0000000000000001",
            "x": 20,
            "y": 20,
            "width": 320,
            "height": 240
        }),
        "ncui.window.child.list" => json!({}),
        "ncui.window.zoom" => json!({ "window": named, "factor": 1.25 }),
        "ncui.window.print.pdf" | "ncui.window.screenshot" => json!({ "window": named }),
        "ncui.window.post" => json!({ "window": named, "script": "1;" }),
        "ncui.app.emit" => json!({ "window": named, "event": "test" }),
        "ncui.window.state" => json!({ "window": named, "maximized": true }),
        "ncui.window.shape" => json!({ "window": named, "title": "renamed" }),
        "ncui.cdp.send" => json!({ "window": named, "method": "Browser.getVersion" }),
        _ => json!({ "deep": true }),
    }
}

/// Every method advertised by `ncui.describe` must be callable by a caller holding the
/// authority it advertises, and every destructive method must require explicit consent.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one sweep drives every advertised method with the parameters it needs; splitting it would hide the ordering the sweep depends on"
)]
fn the_advertised_surface_matches_the_enforced_surface() {
    let mut runtime = runtime();
    let mut administrator = session(AgentRole::Administrator);
    let registry = json!({"schema_version": 1, "features": []});

    let described = call(&mut runtime, &mut administrator, "ncui.describe", json!({}))
        .result
        .expect("describe result");
    let methods = described["methods"]
        .as_array()
        .expect("methods array")
        .clone();
    assert!(!methods.is_empty(), "the surface must not be empty");

    let mut destructive_seen = false;
    let mut created_window: Option<WindowId> = None;

    for method in &methods {
        let name = method["name"].as_str().expect("a method name");
        let destructive = method["destructive"].as_bool().expect("destructive flag");
        let required: Vec<Capability> = method["requires"]
            .as_array()
            .expect("requires")
            .iter()
            .filter_map(Value::as_str)
            .map(|text| text.parse::<Capability>().expect("known capability"))
            .collect();

        for capability in &required {
            assert!(
                AgentRole::Administrator
                    .capabilities()
                    .contains(*capability),
                "`{name}` advertises {capability}, which no role grants"
            );
        }

        // Without consent, a destructive method must be refused even by an
        // administrator who holds the capability.
        if destructive {
            destructive_seen = true;
            let refused = call(&mut runtime, &mut administrator, name, json!({}));
            assert_eq!(
                refused.error.expect("error").data.ncui_code,
                "capability_denied",
                "`{name}` is advertised as destructive but ran without consent"
            );

            // Mint against the complete operation before adding the token transport field.
            let mut consent_params = params_for(name, created_window);
            let correlation = CorrelationId::new();
            let requires: CapabilitySet = required.iter().copied().collect();
            let token = runtime
                .mint_consent(
                    correlation,
                    "integration-agent",
                    name,
                    requires,
                    &consent_params,
                )
                .expect("a destructive method's requirements can be minted");
            consent_params["consent_token"] = token.to_value();
            let allowed = call(&mut runtime, &mut administrator, name, consent_params);
            assert!(
                allowed.error.is_none(),
                "`{name}` was refused despite consent: {:?}",
                allowed.error
            );
            continue;
        }

        if name == "ncui.test.run" || name == "ncui.benchmark.run" {
            // Subprocess-based; environment-dependent. Covered by their handlers existing.
            continue;
        }
        // `ncui.window.close` is exercised on a window of its own, so the window the
        // later window methods name is still open: closing the shared window here would
        // make every later window method report a window that is genuinely gone, which
        // is the method's honest answer and not a defect in it.
        if name == "ncui.window.close" {
            let created = call(
                &mut runtime,
                &mut administrator,
                "ncui.window.create",
                json!({ "url": "https://example.com" }),
            );
            let throwaway = created
                .result
                .as_ref()
                .and_then(|value| value.get("id").and_then(|value| value.as_str()))
                .and_then(|text| text.parse::<WindowId>().ok())
                .expect("a window id");
            let closed = call(
                &mut runtime,
                &mut administrator,
                "ncui.window.close",
                json!({ "window": throwaway.to_string() }),
            );
            assert!(
                closed.error.is_none(),
                "`ncui.window.close` failed on its own window: {:?}",
                closed.error
            );
            continue;
        }
        let params = params_for(name, created_window);
        let allowed = dispatch(
            &mut runtime,
            &mut administrator,
            &Request::new(json!(1), name, params),
            Duration::from_secs(0),
            Some(&registry),
        )
        .expect("response");
        assert!(
            allowed.error.is_none(),
            "an administrator was refused `{name}`: {:?}",
            allowed.error
        );
        if name == "ncui.window.create"
            && let Some(id) = allowed
                .result
                .as_ref()
                .and_then(|value| value.get("id").and_then(|value| value.as_str()))
                .and_then(|text| text.parse::<WindowId>().ok())
        {
            created_window = Some(id);
        }
    }

    assert!(
        destructive_seen,
        "the built-in surface has no destructive method, so the consent gate is never          exercised by the surface agents actually see"
    );
}

/// A failure path must leave the runtime usable: a rejected request is not allowed to
/// corrupt state or exhaust the session's budget for later requests.
#[test]
fn a_rejected_request_does_not_disturb_the_session() {
    let mut runtime = runtime();
    let mut session = session(AgentRole::Observer);
    let registry = json!({"schema_version": 1, "features": []});

    // Observer holds no destructive authority, so purging the log ring must be refused
    // even though Observer may read logs.
    let refused = call(&mut runtime, &mut session, "ncui.logs.purge", json!({}));
    assert!(refused.error.is_some(), "expected a refusal");

    // The session must still work for a method it is entitled to.
    let allowed = dispatch(
        &mut runtime,
        &mut session,
        &Request::new(json!(2), "ncui.health", json!({})),
        Duration::from_secs(0),
        Some(&registry),
    )
    .expect("response");
    assert!(allowed.error.is_none(), "{:?}", allowed.error);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one pipeline test proves every child method and both outcomes without fixture shortcuts"
)]
fn child_operations_emit_bounded_method_and_outcome_metrics() {
    let mut runtime = runtime();
    let mut administrator = session(AgentRole::Administrator);
    let created = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.create",
        json!({ "url": "https://example.com" }),
    );
    let window = created.result.expect("window result")["id"]
        .as_str()
        .expect("window id")
        .to_owned();

    let invalid = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.child.create",
        json!({ "window": window, "url": "", "x": 0, "y": 0, "width": 400, "height": 300 }),
    );
    assert!(
        invalid.error.is_some(),
        "an empty child URL must be refused"
    );

    let created = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.child.create",
        json!({ "window": window, "url": "https://example.com/child", "x": 10, "y": 20, "width": 400, "height": 300 }),
    );
    let child = created.result.expect("child result")["child"]
        .as_str()
        .expect("child id")
        .to_owned();
    assert!(
        call(
            &mut runtime,
            &mut administrator,
            "ncui.window.child.list",
            json!({}),
        )
        .error
        .is_none(),
        "listing a live child must succeed"
    );
    assert!(
        call(
            &mut runtime,
            &mut administrator,
            "ncui.window.child.bounds",
            json!({ "window": window, "child": child, "x": -1, "y": 40, "width": 320, "height": 240 }),
        )
        .error
        .is_some(),
        "a negative child coordinate must be refused"
    );
    assert!(
        call(
            &mut runtime,
            &mut administrator,
            "ncui.window.child.bounds",
            json!({ "window": window, "child": child, "x": 30, "y": 40, "width": 320, "height": 240 }),
        )
        .error
        .is_none(),
        "moving a live child must succeed"
    );
    let close = json!({ "window": window, "child": child });
    assert!(
        call(
            &mut runtime,
            &mut administrator,
            "ncui.window.child.close",
            close.clone(),
        )
        .error
        .is_none(),
        "closing a live child must succeed"
    );
    assert!(
        call(
            &mut runtime,
            &mut administrator,
            "ncui.window.child.close",
            close,
        )
        .error
        .is_some(),
        "closing the same child twice must be refused"
    );

    let mut observer = session(AgentRole::Observer);
    assert!(
        call(
            &mut runtime,
            &mut observer,
            "ncui.window.child.list",
            json!({}),
        )
        .error
        .is_some(),
        "an observer must not enumerate child webviews"
    );

    let snapshot = call(
        &mut runtime,
        &mut administrator,
        "ncui.metrics.query",
        json!({}),
    )
    .result
    .expect("metrics result");
    let child_series: Vec<&Value> = snapshot["series"]
        .as_array()
        .expect("metric series")
        .iter()
        .filter(|series| series["name"] == "window.child_operations")
        .collect();
    assert_eq!(
        child_series.len(),
        8,
        "four methods and two outcomes must create exactly eight bounded series"
    );
    for (method, outcome, expected) in [
        ("create", "allowed", 1),
        ("create", "refused", 1),
        ("bounds", "allowed", 1),
        ("bounds", "refused", 1),
        ("close", "allowed", 1),
        ("close", "refused", 1),
        ("list", "allowed", 1),
        ("list", "refused", 1),
    ] {
        let matching: Vec<_> = child_series
            .iter()
            .filter(|series| {
                series["labels"]["method"] == method && series["labels"]["outcome"] == outcome
            })
            .collect();
        assert_eq!(matching.len(), 1, "{method}/{outcome} must be unique");
        assert_eq!(
            matching[0]["labels"]
                .as_object()
                .map_or(0, serde_json::Map::len),
            2
        );
        let value = matching[0]["value"].as_u64().expect("counter value");
        assert_eq!(value, expected, "{method}/{outcome}");
    }
}
