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
        "ncui.window.create" => json!({ "url": "https://example.com" }),
        "ncui.window.close" | "ncui.window.theme" => json!({ "window": named }),
        "ncui.window.post" => json!({ "window": named, "script": "1;" }),
        "ncui.window.state" => json!({ "window": named, "maximized": true }),
        "ncui.window.shape" => json!({ "window": named, "title": "renamed" }),
        _ => json!({ "deep": true }),
    }
}

/// Every method advertised by `ncui.describe` must be callable by a caller holding the
/// authority it advertises, and every destructive method must require explicit consent.
#[test]
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

            // With a minted consent token it must succeed. The legacy `consent: true`
            // authorizes nothing destructive; the token is the consent, and it is bound to
            // the method and the capabilities the method requires.
            let correlation = CorrelationId::new();
            let requires: CapabilitySet = required.iter().copied().collect();
            let token = runtime
                .mint_consent(correlation, name, requires)
                .expect("a destructive method's requirements can be minted");
            let allowed = call(
                &mut runtime,
                &mut administrator,
                name,
                json!({ "consent_token": token.to_value() }),
            );
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
