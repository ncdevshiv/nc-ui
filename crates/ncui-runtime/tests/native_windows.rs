//! Integration tests for the native window protocol.
//!
//! These drive a real runtime through the real dispatch pipeline, the way an agent does:
//! a window created through `ncui.window.create`, events polled through
//! `ncui.window.native.events`, and commands delivered through
//! `ncui.window.native.command`. The hosts are test doubles wired through the contract's
//! own [`RoutingFactory`], so what is exercised is the runtime's half of the protocol —
//! parsing, validation, routing, and the error shape — rather than a display server.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ncui_core::{Limits, WindowId};
use ncui_runtime::Runtime;
use ncui_runtime::agent::AgentSession;
use ncui_runtime::dispatch::dispatch;
use ncui_runtime::rpc::{Request, Response};
use ncui_security::authz::AgentRole;
use ncui_webview::{
    HostFactory, NativeViewBackend, NativeViewCommand, NativeViewError, NativeViewEvent,
    RoutingFactory,
};
use serde_json::{Value, json};

/// What a test native host and the test itself both see: the events the view would have
/// emitted, the commands it was sent, and the refusal it is configured to answer with.
///
/// Shared rather than polled back out of the registry because the registry owns the host —
/// a test that could not reach in would have no way to stage an event or inspect what a
/// command delivery did, and reaching in is exactly what the shared state replaces.
#[derive(Debug, Default)]
struct NativeState {
    events: Mutex<VecDeque<NativeViewEvent>>,
    commands: Mutex<Vec<NativeViewCommand>>,
    refusal: Mutex<Option<NativeViewError>>,
    /// The cumulative count of events dropped to a full queue, as a real backend counts
    /// it: monotonic, and owed to the drain as a delta rather than restated.
    dropped: Mutex<u64>,
}

impl NativeState {
    fn emit(&self, event: NativeViewEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(event);
    }

    /// Stages `count` more dropped events, the way a full queue dropping its oldest does.
    fn drop_events(&self, count: u64) {
        *self
            .dropped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += count;
    }

    fn refuse_with(&self, refusal: NativeViewError) {
        *self
            .refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(refusal);
    }

    fn commands(&self) -> Vec<NativeViewCommand> {
        self.commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// A native test host: the page half answers the way a live engine does, and the native
/// half is a working queue and command sink over the shared state.
#[derive(Debug)]
struct NativeHost {
    state: Arc<NativeState>,
}

impl NativeViewBackend for NativeHost {
    fn poll_native_event(&self) -> Option<NativeViewEvent> {
        self.state
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }

    fn send_view_command(&self, command: NativeViewCommand) -> Result<(), NativeViewError> {
        let refusal = self
            .state
            .refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(refusal) = refusal {
            return Err(refusal);
        }
        self.state
            .commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(command);
        Ok(())
    }

    fn dropped_native_events(&self) -> u64 {
        *self
            .state
            .dropped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ncui_webview::WebViewBackend for NativeHost {
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

    fn native_view(&self) -> Option<&dyn NativeViewBackend> {
        Some(self)
    }
}

/// The factory the routing layer asks for native windows.
#[derive(Debug)]
struct NativeFactory {
    state: Arc<NativeState>,
}

impl HostFactory for NativeFactory {
    fn spawn(
        &self,
        _window: WindowId,
        _request: &ncui_webview::WindowRequest,
        _seam: &Arc<ncui_webview::HostSeam>,
    ) -> Option<Box<dyn ncui_webview::WebViewBackend>> {
        Some(Box::new(NativeHost {
            state: Arc::clone(&self.state),
        }))
    }
}

/// A page test host: alive, and honestly without a native half — which is what makes it a
/// page window for the methods that ask.
#[derive(Debug)]
struct PageHost;

impl ncui_webview::WebViewBackend for PageHost {
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
}

/// The factory the routing layer asks for page windows.
#[derive(Debug)]
struct PageFactory;

impl HostFactory for PageFactory {
    fn spawn(
        &self,
        _window: WindowId,
        _request: &ncui_webview::WindowRequest,
        _seam: &Arc<ncui_webview::HostSeam>,
    ) -> Option<Box<dyn ncui_webview::WebViewBackend>> {
        Some(Box::new(PageHost))
    }
}

/// A runtime whose windows route by content kind: pages to a page host, native content to
/// a native host over the shared state the test stages through.
fn runtime() -> (Runtime, Arc<NativeState>) {
    let mut runtime = Runtime::start(Limits::default()).expect("runtime should start");
    let state = Arc::new(NativeState::default());
    let routing = RoutingFactory::new()
        .with_page_host(Box::new(PageFactory))
        .with_native_host(Box::new(NativeFactory {
            state: Arc::clone(&state),
        }));
    runtime.windows = runtime
        .windows
        .with_registry(ncui_webview::WindowRegistry::new().with_default_navigation(
            ncui_webview::NavigationPolicy::new(&[ncui_webview::Origin::new(
                ncui_webview::Scheme::Https,
                "example.com",
                None,
            )]),
        ))
        .with_factory(Box::new(routing));
    (runtime, state)
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

/// Opens one window through the real dispatch path, returning its identity.
fn create_window(runtime: &mut Runtime, session: &mut AgentSession, params: Value) -> String {
    let response = call(runtime, session, "ncui.window.create", params);
    assert!(
        response.error.is_none(),
        "the window should have opened: {:?}",
        response.error
    );
    response.result.expect("a created window")["id"]
        .as_str()
        .expect("a window id")
        .to_owned()
}

fn create_native_window(runtime: &mut Runtime, session: &mut AgentSession) -> String {
    create_window(
        runtime,
        session,
        json!({
            "content": { "kind": "native", "provider": "test.panel", "props": { "rows": 3 } }
        }),
    )
}

#[test]
fn a_native_window_is_created_through_the_same_create_method() {
    let (mut runtime, _state) = runtime();
    let mut administrator = session(AgentRole::Administrator);

    let id = create_native_window(&mut runtime, &mut administrator);

    // The window is listed with the content it was opened with: an agent asking what is
    // open learns that this window is a native view, and which provider carries it.
    let listed = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.list",
        json!({}),
    );
    let windows = listed.result.expect("window list")["windows"]
        .as_array()
        .expect("windows array")
        .clone();
    let mine = windows
        .iter()
        .find(|window| window["id"] == id)
        .expect("the native window is listed");
    assert_eq!(mine["content"], "native");
    assert_eq!(mine["provider"], "test.panel");
}

#[test]
fn a_native_window_carrying_a_url_is_refused_before_any_host_is_asked() {
    let (mut runtime, _state) = runtime();
    let mut administrator = session(AgentRole::Administrator);

    let response = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.create",
        json!({
            "url": "https://example.com",
            "content": { "kind": "native", "provider": "test.panel" }
        }),
    );
    let error = response
        .error
        .expect("a native window with a URL must be refused");
    assert_eq!(error.data.ncui_code, "validation");
    assert!(
        error.message.contains("URL"),
        "the refusal names the rule the request broke: {}",
        error.message
    );
}

#[test]
fn a_native_window_with_a_bad_provider_is_refused() {
    let (mut runtime, _state) = runtime();
    let mut administrator = session(AgentRole::Administrator);

    for provider in [
        String::new(),
        "Uppercase".to_owned(),
        "has space".to_owned(),
        "a".repeat(129),
    ] {
        let response = call(
            &mut runtime,
            &mut administrator,
            "ncui.window.create",
            json!({
                "content": { "kind": "native", "provider": provider }
            }),
        );
        let error = response
            .error
            .unwrap_or_else(|| panic!("provider `{provider}` must be refused"));
        assert_eq!(
            error.data.ncui_code, "validation",
            "provider `{provider}`: {}",
            error.message
        );
    }
}

#[test]
fn a_native_window_with_oversized_props_is_refused() {
    let (mut runtime, _state) = runtime();
    let mut administrator = session(AgentRole::Administrator);

    let response = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.create",
        json!({
            "content": {
                "kind": "native",
                "provider": "test.panel",
                "props": { "data": "x".repeat(70_000) }
            }
        }),
    );
    let error = response
        .error
        .expect("props past the contract's bound must be refused");
    assert_eq!(error.data.ncui_code, "validation");
}

#[test]
fn native_events_on_a_native_window_answer_the_queued_events_in_the_contracts_shape() {
    let (mut runtime, state) = runtime();
    let mut administrator = session(AgentRole::Administrator);
    let window = create_native_window(&mut runtime, &mut administrator);

    state.emit(NativeViewEvent::new("test.panel", "clicked").with_payload(json!({ "row": 2 })));
    state.emit(NativeViewEvent::new("test.panel", "changed").with_payload(json!({ "text": "hi" })));

    let response = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.native.events",
        json!({ "window": window }),
    );
    let result = response.result.expect("the drain answers");
    // Each event is the contract's own serialization — the runtime carries it rather than
    // reshaping it, so a client written against the contract reads it unchanged — and the
    // drain answers everything queued since the last drain, in the order it was emitted.
    assert_eq!(
        result["events"],
        json!([
            { "source": "test.panel", "action": "clicked", "payload": { "row": 2 } },
            { "source": "test.panel", "action": "changed", "payload": { "text": "hi" } },
        ])
    );
    assert_eq!(result["dropped"], 0);

    // The queue is now honestly empty, and an empty queue is an empty answer, not an error.
    let again = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.native.events",
        json!({ "window": window }),
    );
    let drained = again.result.expect("the second drain answers");
    assert_eq!(drained["events"], json!([]));
    assert_eq!(drained["dropped"], 0);
}

#[test]
fn native_events_report_what_the_queue_dropped_since_the_last_drain() {
    let (mut runtime, state) = runtime();
    let mut administrator = session(AgentRole::Administrator);
    let window = create_native_window(&mut runtime, &mut administrator);

    // The backend's counter is cumulative: five events were dropped to a full queue before
    // the first drain, and the first drain is owed all of them.
    state.drop_events(5);
    let first = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.native.events",
        json!({ "window": window }),
    );
    assert_eq!(first.result.expect("the first drain answers")["dropped"], 5);

    // Two more drops since — the second drain is owed the delta, not the cumulative count,
    // because one loss counted twice is a metric that lies.
    state.drop_events(2);
    let second = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.native.events",
        json!({ "window": window }),
    );
    assert_eq!(
        second.result.expect("the second drain answers")["dropped"],
        2
    );

    // And a quiet interval is owed zero rather than the count restated.
    let third = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.native.events",
        json!({ "window": window }),
    );
    assert_eq!(third.result.expect("the third drain answers")["dropped"], 0);
}

#[test]
fn native_events_on_a_page_window_is_a_structured_refusal() {
    let (mut runtime, _state) = runtime();
    let mut administrator = session(AgentRole::Administrator);
    let page = create_window(
        &mut runtime,
        &mut administrator,
        json!({ "url": "https://example.com" }),
    );

    let response = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.native.events",
        json!({ "window": page }),
    );
    let error = response
        .error
        .expect("a page window has no native events to poll");
    assert_eq!(error.data.ncui_code, "conflict");
    assert!(
        error.message.contains("is not a native window"),
        "the refusal says what the window is: {}",
        error.message
    );
}

#[test]
fn native_events_on_a_window_that_is_not_open_is_not_found() {
    let (mut runtime, _state) = runtime();
    let mut administrator = session(AgentRole::Administrator);

    let response = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.native.events",
        json!({ "window": "00000000000000000000000000000001" }),
    );
    let error = response.error.expect("a missing window is not found");
    assert_eq!(error.data.ncui_code, "not_found");
}

#[test]
fn a_native_command_reaches_the_view() {
    let (mut runtime, state) = runtime();
    let mut administrator = session(AgentRole::Administrator);
    let window = create_native_window(&mut runtime, &mut administrator);

    let response = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.native.command",
        json!({
            "window": window,
            "action": "select_row",
            "payload": { "row": 4 }
        }),
    );
    assert!(
        response.error.is_none(),
        "the command should have been delivered: {:?}",
        response.error
    );
    let result = response.result.expect("a delivered command");
    assert_eq!(result["delivered"], true);

    assert_eq!(
        state.commands(),
        vec![NativeViewCommand::Invoke {
            action: "select_row".to_owned(),
            payload: json!({ "row": 4 }),
        }],
        "the view received exactly the command the caller sent"
    );
}

#[test]
fn an_out_of_bounds_command_never_reaches_the_view() {
    let (mut runtime, state) = runtime();
    let mut administrator = session(AgentRole::Administrator);
    let window = create_native_window(&mut runtime, &mut administrator);

    let response = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.native.command",
        json!({
            "window": window,
            "action": "select_row",
            "payload": { "data": "x".repeat(70_000) }
        }),
    );
    let error = response
        .error
        .expect("a command past the contract's bound must be refused");
    assert_eq!(error.data.ncui_code, "validation");
    assert!(
        error.message.contains("native_out_of_bounds"),
        "the refusal carries the stable code: {}",
        error.message
    );
    assert!(
        state.commands().is_empty(),
        "the command was refused before the view was asked"
    );
}

#[test]
fn a_malformed_command_is_refused_as_validation() {
    let (mut runtime, _state) = runtime();
    let mut administrator = session(AgentRole::Administrator);
    let window = create_native_window(&mut runtime, &mut administrator);

    // Neither half of the either/or, both halves at once, a verb that is not a verb,
    // props that are not an object, and a payload with no verb to carry it: each is a
    // caller mistake rather than a command, and each is refused before the view is asked.
    for params in [
        json!({ "window": window }),
        json!({ "window": window, "props": { "rows": 3 }, "action": "focus" }),
        json!({ "window": window, "action": "" }),
        json!({ "window": window, "action": 3 }),
        json!({ "window": window, "props": "rows" }),
        json!({ "window": window, "props": { "rows": 3 }, "payload": { "row": 1 } }),
    ] {
        let response = call(
            &mut runtime,
            &mut administrator,
            "ncui.window.native.command",
            params.clone(),
        );
        let error = response
            .error
            .unwrap_or_else(|| panic!("command {params} must be refused"));
        assert_eq!(
            error.data.ncui_code, "validation",
            "command {params}: {}",
            error.message
        );
    }
}

#[test]
fn a_command_on_a_page_window_is_a_structured_refusal() {
    let (mut runtime, _state) = runtime();
    let mut administrator = session(AgentRole::Administrator);
    let page = create_window(
        &mut runtime,
        &mut administrator,
        json!({ "url": "https://example.com" }),
    );

    let response = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.native.command",
        json!({
            "window": page,
            "action": "focus"
        }),
    );
    let error = response
        .error
        .expect("a page window has no native view to command");
    assert_eq!(error.data.ncui_code, "conflict");
    assert!(
        error.message.contains("is not a native window"),
        "the refusal says what the window is: {}",
        error.message
    );
}

#[test]
fn a_refusal_from_the_view_carries_its_stable_code() {
    let (mut runtime, state) = runtime();
    let mut administrator = session(AgentRole::Administrator);
    let window = create_native_window(&mut runtime, &mut administrator);

    for (refusal, ncui_code, stable) in [
        (
            NativeViewError::Refused {
                reason: "no such verb".to_owned(),
            },
            "conflict",
            "native_view_refused",
        ),
        (
            NativeViewError::WindowGone,
            "conflict",
            "native_window_gone",
        ),
        (
            NativeViewError::EngineUnavailable {
                reason: "the engine thread is gone".to_owned(),
            },
            "conflict",
            "native_engine_unavailable",
        ),
        (
            NativeViewError::OutOfBounds {
                reason: "the payload is too large".to_owned(),
            },
            "validation",
            "native_out_of_bounds",
        ),
    ] {
        state.refuse_with(refusal);
        let response = call(
            &mut runtime,
            &mut administrator,
            "ncui.window.native.command",
            json!({
                "window": window,
                "action": "focus"
            }),
        );
        let error = response
            .error
            .unwrap_or_else(|| panic!("the refusal {stable} must surface"));
        assert_eq!(
            error.data.ncui_code, ncui_code,
            "{stable} is classified as {ncui_code}"
        );
        assert!(
            error.message.contains(stable),
            "the stable code survives into the wire error: {}",
            error.message
        );
    }
}

#[test]
fn updating_props_is_delivered_in_the_contracts_own_shape() {
    let (mut runtime, state) = runtime();
    let mut administrator = session(AgentRole::Administrator);
    let window = create_native_window(&mut runtime, &mut administrator);

    let response = call(
        &mut runtime,
        &mut administrator,
        "ncui.window.native.command",
        json!({
            "window": window,
            "props": { "rows": 9 }
        }),
    );
    assert!(
        response.error.is_none(),
        "the props update should have been delivered: {:?}",
        response.error
    );
    assert_eq!(
        state.commands(),
        vec![NativeViewCommand::UpdateProps {
            props: json!({ "rows": 9 }),
        }]
    );
}

#[test]
fn a_page_window_is_still_created_exactly_as_before() {
    let (mut runtime, _state) = runtime();
    let mut administrator = session(AgentRole::Administrator);

    // The default content is a page, so a payload written before native windows existed
    // means exactly what it always meant — and an explicit `{"kind": "page"}` is the same
    // request said out loud.
    for params in [
        json!({ "url": "https://example.com" }),
        json!({ "url": "https://example.com", "content": { "kind": "page" } }),
    ] {
        let id = create_window(&mut runtime, &mut administrator, params);
        let listed = call(
            &mut runtime,
            &mut administrator,
            "ncui.window.list",
            json!({}),
        );
        let windows = listed.result.expect("window list")["windows"]
            .as_array()
            .expect("windows array")
            .clone();
        let mine = windows
            .iter()
            .find(|window| window["id"] == id)
            .expect("the page window is listed");
        assert_eq!(mine["content"], "page");
    }
}
