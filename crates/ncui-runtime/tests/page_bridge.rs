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
    /// Window drags the host has asked the engine to start.
    drags: Mutex<Vec<()>>,
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

    /// How many window drags the engine was asked to start.
    fn drag_count(&self) -> usize {
        self.state
            .drags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
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

    fn drag_window(&self) -> bool {
        self.state
            .drags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(());
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
    // Consent for a destructive page command is currently the `consent` bool: absent means
    // withhold (the gate fails closed), present means the page states the user confirmed. This
    // drives the real pump through both. The bool is a documented stopgap — see the E1 entry in
    // `known-gaps.md`: replacing it with an unforgeable token needs the runtime's consent UI,
    // which does not exist yet, and no production path mints a token a page could present.
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

#[test]
fn a_page_asking_for_describe_is_told_the_locale_and_its_direction() {
    // C4's non-GPU half, end to end: the page asks `describe` over the bridge and the answer
    // names the platform's locale and the text direction that locale lays out in. The page
    // never keeps its own table of right-to-left scripts — the runtime derives the direction
    // from the locale it reports, so the two cannot disagree.
    let (mut runtime, factory) = runtime_with_page();
    let _window = open_window(&mut runtime);
    let engine = factory
        .engines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .expect("one engine");

    engine.post("{\"id\":\"call-locale\",\"action\":\"describe\"}");
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);

    let scripts = engine.scripts();
    let answer = scripts
        .iter()
        .find(|script| script.contains("\"id\":\"call-locale\""))
        .expect("the describe answer is delivered under the page's id");
    assert!(
        answer.contains("\"locale\":"),
        "the answer names the platform locale: {answer}"
    );
    assert!(
        answer.contains("\"ltr\"") || answer.contains("\"rtl\""),
        "the answer names a text direction: {answer}"
    );
}

#[test]
fn a_page_registered_command_runs_in_the_page_and_answers_the_page() {
    // The JavaScript half of "a command is not Rust-only": the page registers a handler, the
    // runtime hands the call to it over the bridge, and the handler's answer settles the page's
    // own invoke. This drives the real pump, so it exercises the runtime's interception, the
    // capability gate, and the deferred reply together rather than any one of them in isolation.
    let mut runtime = Runtime::start(Limits::default()).expect("runtime should start");
    runtime.grant_page_capabilities(&[Capability::InspectRuntime]);
    // Registered before the window registry is built, so the command is in the action table the
    // window's seam enforces — the same order an application's startup registers its commands.
    runtime
        .page_commands
        .register_js_command(
            "js.greet",
            CapabilitySet::empty().with(Capability::InspectRuntime),
            "Greet from the page",
        )
        .expect("the js command registers");
    let (mut runtime, factory) = attach_page(runtime);
    let _window = open_window(&mut runtime);
    let engine = factory
        .engines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .expect("one engine");

    // The page invokes its own registered command.
    engine.post(
        "{\"id\":\"call-js-1\",\"action\":\"perform\",\"name\":\"js.greet\",\"payload\":{\"who\":\"page\"}}",
    );
    runtime.pump_pages(Duration::from_secs(0));

    // The runtime did not answer the invoke directly: it handed the command to the page by
    // evaluating a runCommand call carrying a reply id it minted.
    let scripts = engine.scripts();
    let run = scripts
        .iter()
        .find(|script| script.contains("__runCommand"))
        .expect("the runtime hands a page command to the page")
        .clone();
    let reply_id = run
        .split("__runCommand(\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("a reply id")
        .to_owned();
    assert!(
        run.contains("js.greet"),
        "the runCommand names the command it is running: {run}"
    );

    // The page runs its handler and posts the answer back under that reply id.
    engine.post(&format!(
        "{{\"id\":\"{reply_id}\",\"action\":\"command_result\",\"ok\":true,\"payload\":{{\"hello\":\"page\"}}}}"
    ));
    runtime.pump_pages(Duration::from_secs(0));

    // The answer settles the page's ORIGINAL invoke, not the reply id the runtime minted.
    let scripts = engine.scripts();
    let settled = scripts.last().expect("the original invoke is settled");
    assert!(
        settled.contains("\"id\":\"call-js-1\"") && settled.contains("\"hello\":\"page\""),
        "the handler's answer settles the page's own invoke: {settled}"
    );
}

#[test]
fn a_page_command_the_page_was_not_granted_is_refused_and_never_handed_over() {
    // The gate is the same one every command passes: a page that lacks the capability the
    // command requires never reaches its handler, and the runtime hands it nothing to run.
    let runtime = Runtime::start(Limits::default()).expect("runtime should start");
    runtime
        .page_commands
        .register_js_command(
            "js.admin",
            CapabilitySet::empty().with(Capability::Deploy),
            "Admin only",
        )
        .expect("the js command registers");
    let (mut runtime, factory) = attach_page(runtime);
    let _window = open_window(&mut runtime);
    let engine = factory
        .engines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .expect("one engine");

    engine.post("{\"id\":\"call-js-2\",\"action\":\"perform\",\"name\":\"js.admin\"}");
    runtime.pump_pages(Duration::from_secs(0));

    let scripts = engine.scripts();
    assert!(
        scripts
            .iter()
            .any(|script| script.contains("\"code\":\"capability_denied\"")
                && script.contains("\"id\":\"call-js-2\"")),
        "an ungranted page command is refused at the seam: {scripts:?}"
    );
    assert!(
        !scripts.iter().any(|script| script.contains("__runCommand")),
        "the runtime must not hand an ungranted command to the page: {scripts:?}"
    );
}

#[test]
fn a_drag_region_mousedown_reaches_the_window_engine() {
    // The injected drag-region script calls `drag_region` (the declared action
    // name). The seam routes it to `ncui.window.drag.region`, the handler asks
    // the calling window's engine, and the engine records the drag.
    let (mut runtime, factory) = runtime_with_page_and_grant(&[Capability::Window]);
    let _window = open_window(&mut runtime);
    let engine = factory
        .engines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .expect("one engine");

    engine.post("{\"id\":\"drag-1\",\"action\":\"perform\",\"name\":\"drag_region\"}");
    let served = runtime.pump_pages(Duration::from_secs(0));
    assert_eq!(served, 1, "the page's drag request must be served");

    let scripts = engine.scripts();
    assert!(
        scripts
            .iter()
            .any(|s| s.contains("\"id\":\"drag-1\"") && s.contains("\"ok\":true")),
        "a granted drag_region action must be served: {scripts:?}"
    );
    assert_eq!(
        engine.drag_count(),
        1,
        "the drag must reach the window's engine, not be answered from the registry"
    );
}

#[test]
fn an_application_event_is_delivered_to_the_page() {
    // Runtime::emit is the Rust half of ncui.app.emit; the page receives the
    // same __dispatch script either path.
    let (mut runtime, factory) = runtime_with_page();
    let window = open_window(&mut runtime);
    let engine = factory
        .engines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .expect("one engine");

    runtime
        .emit(
            window,
            "zcode:deep-link",
            &json!({ "url": "zcode://open/file.txt" }),
        )
        .expect("the event is delivered to an open window");

    let scripts = engine.scripts();
    assert!(
        scripts
            .iter()
            .any(|s| s.contains("__dispatch") && s.contains("zcode:deep-link")),
        "the event must reach the page through the bridge: {scripts:?}"
    );
}
