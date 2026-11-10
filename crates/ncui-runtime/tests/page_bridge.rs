//! The page-to-host bridge, end to end.
//!
//! This is the test the whole `ncui-webview` policy was written for. The policy crates
//! were, until now, tested against themselves: a seam that refused an ungranted action,
//! and a registry that opened a window over a factory that was not there. Nothing had ever
//! checked that a *page* — a document the runtime does not control — could reach the
//! runtime's dispatch table and be governed while it did.
//!
//! So the test drives the real pipeline: a page posts a message, the runtime parses it,
//! checks it against the window's grant, dispatches it through the same pipeline an
//! agent's request goes through, and answers the page.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ncui_core::{Capability, CapabilitySet, Limits};
use ncui_runtime::Runtime;
use ncui_runtime::dispatch::dispatch;
use ncui_runtime::rpc::Request;
use ncui_webview::{HostFactory, HostSeam, PageMessage, WebViewBackend};
use serde_json::{Value, json};

/// The state the test's engine shares with the test.
#[derive(Debug, Default)]
struct EngineState {
    /// Messages the page has "sent", waiting to be polled.
    outgoing: Mutex<Vec<String>>,
    /// Scripts the host has asked the engine to evaluate.
    evaluated: Mutex<Vec<String>>,
}

/// A page engine the test drives.
#[derive(Debug, Default, Clone)]
struct PageEngine {
    state: Arc<EngineState>,
}

impl PageEngine {
    /// A fresh engine.
    fn new() -> Self {
        Self::default()
    }

    /// Queues a message as though the page had posted it.
    fn post(&self, body: &str) {
        self.state
            .outgoing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(body.to_owned());
    }

    /// The scripts the host sent to the page.
    fn scripts(&self) -> Vec<String> {
        self.state
            .evaluated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl WebViewBackend for PageEngine {
    fn last_error(&self) -> Option<String> {
        None
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn poll_page_message(&self) -> Option<PageMessage> {
        let body = self
            .state
            .outgoing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()?;
        Some(PageMessage::new(body))
    }

    fn evaluate_script(&self, script: &str) -> bool {
        self.state
            .evaluated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(script.to_owned());
        true
    }
}

/// A factory that hands out one page engine and remembers it.
#[derive(Debug, Default, Clone)]
struct PageFactory {
    engines: Arc<Mutex<Vec<PageEngine>>>,
}

impl HostFactory for PageFactory {
    fn spawn(
        &self,
        _window: ncui_core::WindowId,
        _request: &ncui_webview::WindowRequest,
        _seam: &Arc<HostSeam>,
    ) -> Option<Box<dyn WebViewBackend>> {
        let engine = PageEngine::new();
        self.engines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(engine.clone());
        Some(Box::new(engine))
    }
}

/// A runtime with a page, and the engine behind it.
fn runtime_with_page() -> (Runtime, PageFactory) {
    attach_page(Runtime::start(Limits::default()).expect("runtime should start"))
}

/// A runtime whose page grant includes `capabilities` before the window registry is built.
fn runtime_with_page_and_grant(capabilities: &[Capability]) -> (Runtime, PageFactory) {
    let mut runtime = Runtime::start(Limits::default()).expect("runtime should start");
    runtime.grant_page_capabilities(capabilities);
    attach_page(runtime)
}

fn attach_page(mut runtime: Runtime) -> (Runtime, PageFactory) {
    let factory = PageFactory::default();
    let actions = runtime.page_commands.actions();
    let registry = ncui_runtime::window::hosted_registry(
        &[ncui_webview::Origin::new(
            ncui_webview::Scheme::Https,
            "app.example.com",
            None,
        )],
        &actions,
        runtime.page_grant(),
    );
    runtime.windows = runtime
        .windows
        .with_registry(registry)
        .with_factory(Box::new(factory.clone()));
    (runtime, factory)
}

fn open_window(runtime: &mut Runtime) -> ncui_core::WindowId {
    // The session must be authenticated: the pipeline refuses an unauthenticated caller
    // before it reaches any handler, and a page's window is opened by an agent.
    let token = ncui_security::token::SecretToken::new(vec![0x11; 32]).expect("token");
    let mut session = ncui_runtime::agent::AgentSession::new(
        "agent-under-test",
        ncui_security::authz::AgentRole::Administrator,
        1_000,
        1_000,
    )
    .expect("session");
    session
        .authenticate(&token, &token)
        .expect("the test session authenticates");
    let response = dispatch(
        runtime,
        &mut session,
        &Request::new(
            json!(1),
            "ncui.window.create",
            json!({ "url": "https://app.example.com/" }),
        ),
        Duration::from_secs(0),
        None,
    )
    .expect("response");
    let id = response
        .result
        .and_then(|value| {
            value
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .expect("a window id");
    id.parse().expect("a window id")
}

#[test]
fn a_page_message_reaches_the_runtime_and_the_answer_reaches_the_page() {
    let (mut runtime, factory) = runtime_with_page();
    let _window = open_window(&mut runtime);
    let engine = factory
        .engines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .expect("one engine");

    engine.post("{\"id\":\"call-1\",\"action\":\"perform\",\"name\":\"describe_metrics\"}");

    let served = runtime.pump_pages(Duration::from_secs(0));
    assert_eq!(served, 1, "the page's message must be served");

    let scripts = engine.scripts();
    assert_eq!(scripts.len(), 1, "the answer must go back to the page");
    let answer = scripts[0].clone();
    assert!(
        answer.contains("window.ncui.__dispatch("),
        "the answer must be delivered through the defined bridge entry point: {answer}"
    );
    assert!(
        answer.contains("\"id\":\"call-1\""),
        "the answer must echo the request id: {answer}"
    );
    assert!(
        answer.contains("\"ok\":true"),
        "a granted action must be served: {answer}"
    );
}

#[test]
fn a_page_cannot_reach_an_action_the_host_did_not_declare() {
    let (mut runtime, factory) = runtime_with_page();
    let _window = open_window(&mut runtime);
    let engine = factory
        .engines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .expect("one engine");

    // `ncui.logs.purge` is a real method, and a page that could name it would be able to
    // destroy the log ring. The seam only knows the actions the host declared.
    engine.post("{\"id\":\"call-2\",\"action\":\"perform\",\"name\":\"purge_logs\"}");

    let served = runtime.pump_pages(Duration::from_secs(0));
    assert_eq!(served, 1);
    let scripts = engine.scripts();
    assert_eq!(scripts.len(), 1);
    assert!(
        scripts[0].contains("\"id\":\"call-2\"") && scripts[0].contains("capability_denied"),
        "an undeclared action must be refused under its id: {}",
        scripts[0]
    );
}

#[test]
fn a_malformed_page_message_is_answered_rather_than_dropped() {
    let (mut runtime, factory) = runtime_with_page();
    let _window = open_window(&mut runtime);
    let engine = factory
        .engines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .expect("one engine");

    engine.post("this is not json");

    let served = runtime.pump_pages(Duration::from_secs(0));
    assert_eq!(served, 1, "a malformed message is still a message");
    let scripts = engine.scripts();
    assert_eq!(scripts.len(), 1, "the page must be told, not left waiting");
    assert!(
        scripts[0].contains("malformed"),
        "the refusal must carry a stable code: {}",
        scripts[0]
    );
}

#[test]
fn a_page_message_is_audited_as_the_page_that_sent_it() {
    let (mut runtime, factory) = runtime_with_page();
    let window = open_window(&mut runtime);
    let engine = factory
        .engines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .expect("one engine");

    let before = runtime.audit.len();
    engine.post("{\"id\":\"call-3\",\"action\":\"perform\",\"name\":\"describe_metrics\"}");
    runtime.pump_pages(Duration::from_secs(0));

    let events = runtime.audit.to_vec();
    assert!(
        events.len() > before,
        "a page's request must be audited like any other"
    );
    let recorded = events
        .iter()
        .find(|event| event.agent_id == format!("page:{window}"))
        .expect("the audit trail must name the page");
    assert_eq!(recorded.action, "ncui.metrics.query");
}

#[test]
fn an_application_command_runs_through_the_page_bridge() {
    let (mut runtime, factory) = runtime_with_page();
    let _registration = runtime
        .register_page_command(
            "add_numbers",
            CapabilitySet::empty().with(Capability::InspectRuntime),
            "Add two integers",
            |payload| {
                let a = payload.get("a").and_then(Value::as_i64).ok_or_else(|| {
                    ncui_webview::SeamError::new("invalid_params", "`a` is required")
                })?;
                let b = payload.get("b").and_then(Value::as_i64).ok_or_else(|| {
                    ncui_webview::SeamError::new("invalid_params", "`b` is required")
                })?;
                Ok(json!({ "sum": a + b }))
            },
        )
        .expect("the command's capability is already in the page grant");
    let _window = open_window(&mut runtime);
    let engine = factory
        .engines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .expect("one engine");

    engine.post(
        "{\"id\":\"call-4\",\"action\":\"perform\",\"name\":\"add_numbers\",\"payload\":{\"a\":2,\"b\":3}}",
    );
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);

    let scripts = engine.scripts();
    assert_eq!(scripts.len(), 1);
    assert!(
        scripts[0].contains("\"id\":\"call-4\"") && scripts[0].contains("\"sum\":5"),
        "the application command must answer under the request id: {}",
        scripts[0]
    );
}

#[test]
fn a_command_needing_an_ungranted_capability_is_refused_at_registration() {
    let mut runtime = Runtime::start(Limits::default()).expect("runtime should start");
    let error = runtime
        .register_page_command(
            "copy_text",
            CapabilitySet::empty().with(Capability::Clipboard),
            "Copy text",
            |_| Ok(Value::Null),
        )
        .expect_err("the page has not been granted the clipboard");
    assert!(
        error.to_string().contains("clipboard"),
        "the refusal must name the missing capability: {error}"
    );

    runtime.grant_page_capabilities(&[Capability::Clipboard]);
    let _ = runtime
        .register_page_command(
            "copy_text",
            CapabilitySet::empty().with(Capability::Clipboard),
            "Copy text",
            |_| Ok(Value::Null),
        )
        .expect("the grant now covers the command");
}

#[test]
fn a_destructive_application_command_needs_consent() {
    let (mut runtime, factory) = runtime_with_page_and_grant(&[Capability::FilesystemWrite]);
    let _registration = runtime
        .register_page_command(
            "write_note",
            CapabilitySet::empty().with(Capability::FilesystemWrite),
            "Write a note",
            |_| Ok(json!({ "written": true })),
        )
        .expect("register");
    let _window = open_window(&mut runtime);
    let engine = factory
        .engines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .expect("one engine");

    engine.post("{\"id\":\"call-5\",\"action\":\"perform\",\"name\":\"write_note\"}");
    runtime.pump_pages(Duration::from_secs(0));
    let refused = engine.scripts();
    assert!(
        refused.last().is_some_and(|script| {
            script.contains("\"code\":\"capability_denied\"")
                && script.contains("\"id\":\"call-5\"")
        }),
        "a destructive command without consent must be refused: {refused:?}"
    );

    engine.post(
        "{\"id\":\"call-6\",\"action\":\"perform\",\"name\":\"write_note\",\"payload\":{\"consent\":true}}",
    );
    runtime.pump_pages(Duration::from_secs(0));
    let allowed = engine.scripts();
    assert!(
        allowed
            .last()
            .is_some_and(|script| script.contains("\"written\":true")
                && script.contains("\"id\":\"call-6\"")),
        "consent must permit the destructive command: {allowed:?}"
    );
}

#[test]
fn a_page_cannot_reach_a_capability_the_window_was_not_granted() {
    // The window's grant is derived from the declared actions, so a page holds exactly
    // what the host offered it. This asserts the derivation is real rather than nominal.
    let (mut runtime, _factory) = runtime_with_page();
    let window = open_window(&mut runtime);
    let granted = runtime
        .windows
        .grant_for(window)
        .expect("the window is open")
        .expect("the window has a grant");
    assert!(granted.contains(Capability::ReadMetrics));
    assert!(granted.contains(Capability::InspectRuntime));
    assert!(
        !granted.contains(Capability::DeleteData),
        "a page must not hold a destructive authority"
    );
    assert!(
        !granted.contains(Capability::Deploy),
        "a page must not hold a deployment authority"
    );
}
