//! The window actions a page can perform, driven through the real pipeline.
//!
//! A page that is granted `window` may open a window and close *the one it is in*. The
//! tests here drive the page's request through the seam, the grant, the dispatch table and
//! the registry, so what is asserted is the behaviour a page would actually get.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ncui_core::{Capability, Limits, WindowId};
use ncui_runtime::Runtime;
use ncui_runtime::dispatch::dispatch;
use ncui_runtime::rpc::Request;
use ncui_webview::{HostFactory, HostSeam, PageMessage, WebViewBackend};
use serde_json::{Value, json};

/// The state the test's engine shares with the test.
#[derive(Debug, Default)]
struct EngineState {
    outgoing: Mutex<Vec<String>>,
    evaluated: Mutex<Vec<String>>,
    /// Where the test says this engine's window is, when it says anything at all.
    ///
    /// A real host reports the window's geometry from the platform; here the test decides, so a
    /// place can be written down without a display.
    geometry: Mutex<Option<ncui_webview::WindowGeometry>>,
    /// How the test's engine says its window is presented, and what it applies changes to.
    ///
    /// A real host asks the platform; here the engine holds the answer, so a change and the report
    /// that follows it can be observed without a display.
    presentation: Mutex<ncui_webview::WindowState>,
}

/// A page engine the test drives.
#[derive(Debug, Default, Clone)]
struct PageEngine {
    state: Arc<EngineState>,
}

impl PageEngine {
    fn post(&self, body: &str) {
        self.state
            .outgoing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(body.to_owned());
    }

    fn scripts(&self) -> Vec<String> {
        self.state
            .evaluated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Says this engine's window sits at `geometry`, as the platform would report it.
    fn place_at(&self, geometry: ncui_webview::WindowGeometry) {
        *self
            .state
            .geometry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(geometry);
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

    fn geometry(&self) -> Option<ncui_webview::WindowGeometry> {
        *self
            .state
            .geometry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn window_state(&self) -> Option<ncui_webview::WindowState> {
        Some(
            *self
                .state
                .presentation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn set_window_state(
        &self,
        change: ncui_webview::WindowChange,
    ) -> Option<ncui_webview::WindowState> {
        let mut state = self
            .state
            .presentation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match change {
            ncui_webview::WindowChange::Fullscreen(value) => state.fullscreen = value,
            ncui_webview::WindowChange::Maximized(value) => state.maximized = value,
            ncui_webview::WindowChange::Minimized(value) => state.minimized = value,
            ncui_webview::WindowChange::Visible(value) => state.visible = value,
            ncui_webview::WindowChange::Focus => state.focused = true,
        }
        Some(*state)
    }

    fn set_window_shape(&self, _shape: &ncui_webview::WindowShape) -> bool {
        true
    }
}

/// A factory that hands out one page engine per window and remembers them.
#[derive(Debug, Default, Clone)]
struct PageFactory {
    engines: Arc<Mutex<Vec<PageEngine>>>,
}

impl PageFactory {
    fn latest(&self) -> PageEngine {
        self.engines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
            .expect("at least one engine")
    }
}

impl HostFactory for PageFactory {
    fn spawn(
        &self,
        _window: WindowId,
        _request: &ncui_webview::WindowRequest,
        _seam: &Arc<HostSeam>,
    ) -> Option<Box<dyn WebViewBackend>> {
        let engine = PageEngine::default();
        self.engines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(engine.clone());
        Some(Box::new(engine))
    }
}

/// A runtime with one page, and the factory behind it.
fn runtime_with_page(capabilities: &[Capability]) -> (Runtime, PageFactory) {
    let mut runtime = Runtime::start(Limits::default()).expect("runtime should start");
    runtime.grant_page_capabilities(capabilities);
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

/// Opens the first window, as an agent would.
fn open_window(runtime: &mut Runtime) -> WindowId {
    let token = ncui_security::token::SecretToken::new(vec![0x22; 32]).expect("token");
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
    response
        .result
        .and_then(|value| {
            value
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .expect("a window id")
        .parse()
        .expect("a window id")
}

/// Posts `create_window` from the page and returns the parsed answer.
fn page_asks_to_create(runtime: &mut Runtime, engine: &PageEngine, payload: &Value) -> Value {
    let request = json!({
        "id": "create-1",
        "action": "perform",
        "name": "create_window",
        "payload": payload.clone(),
    });
    engine.post(&serde_json::to_string(&request).expect("a serialisable request"));
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    answer(engine)
}

/// The answer the host delivered to the page.
fn answer(engine: &PageEngine) -> Value {
    let script = engine
        .scripts()
        .pop()
        .expect("an answer must reach the page");
    let encoded = script
        .trim_start_matches("window.ncui && window.ncui.__dispatch(")
        .trim_end_matches(");");
    serde_json::from_str(encoded).expect("the answer is JSON")
}

#[test]
fn the_default_page_grant_does_not_include_the_window_capability() {
    // The read-only actions are granted without asking; opening and closing windows is
    // not. An application that wants it names `window` in its manifest.
    let grant = ncui_runtime::window::default_page_grant(ncui_runtime::window::SEAM_ACTIONS);
    let granted = grant.granted();
    assert!(
        granted.contains(Capability::InspectRuntime),
        "the read-only actions must stay available by default"
    );
    assert!(
        !granted.contains(Capability::Window),
        "a page must not be able to open windows unless its application says so"
    );
}

#[test]
fn describe_tells_the_page_the_window_actions_and_what_they_require() {
    let (mut runtime, factory) = runtime_with_page(&[]);
    let _first = open_window(&mut runtime);
    let engine = factory.latest();

    engine.post("{\"id\":\"d1\",\"action\":\"describe\"}");
    runtime.pump_pages(Duration::from_secs(0));
    let answer = answer(&engine);
    assert_eq!(answer["ok"], true, "{answer}");
    let actions = answer["payload"]["actions"]
        .as_array()
        .expect("the action list");
    let create = actions
        .iter()
        .find(|action| action["name"] == "create_window")
        .expect("the action must be advertised");
    assert_eq!(
        create["requires"],
        json!(["window"]),
        "a page must be able to see what the action costs before calling it"
    );
    assert!(
        create["summary"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "an advertised action must say what it does"
    );
    let close = actions
        .iter()
        .find(|action| action["name"] == "close_window")
        .expect("the close action must be advertised");
    assert_eq!(close["requires"], json!(["window"]));
}

#[test]
fn a_page_without_the_capability_is_refused_before_the_registry_is_reached() {
    let (mut runtime, factory) = runtime_with_page(&[]);
    let _first = open_window(&mut runtime);
    let engine = factory.latest();

    let answer = page_asks_to_create(
        &mut runtime,
        &engine,
        &json!({ "url": "https://app.example.com/second" }),
    );
    assert_eq!(answer["ok"], false, "the call must be refused: {answer}");
    assert_eq!(
        answer["error"]["code"], "capability_denied",
        "the refusal must name the reason: {answer}"
    );
    assert_eq!(
        runtime.windows.summaries().expect("the surface").len(),
        1,
        "no window may exist after a refusal"
    );
}

#[test]
fn a_page_with_the_capability_opens_a_window_at_an_allowed_url() {
    let (mut runtime, factory) = runtime_with_page(&[Capability::Window]);
    let _first = open_window(&mut runtime);
    let engine = factory.latest();

    let answer = page_asks_to_create(
        &mut runtime,
        &engine,
        &json!({
            "url": "https://app.example.com/second",
            "title": "Second",
            "width": 640,
            "height": 480
        }),
    );
    assert_eq!(answer["ok"], true, "the call must be allowed: {answer}");

    let summaries = runtime.windows.summaries().expect("the surface");
    assert_eq!(summaries.len(), 2, "the page's window must exist");
    let second = summaries
        .iter()
        .find(|summary| summary.title == "Second")
        .expect("the window the page asked for");
    assert_eq!(second.url, "https://app.example.com/second");
    assert_eq!((second.width, second.height), (640, 480));

    let labels = ncui_observability::metrics::Labels::new();
    assert!(
        runtime
            .metrics
            .counter_value("window.created", &labels)
            .is_some_and(|count| count >= 2),
        "every window that was opened must be counted, including the agent's first one"
    );
}

#[test]
fn a_page_cannot_open_a_window_at_a_url_the_policy_refuses() {
    // The engine permits a window's own initial load, so the registry is the only place
    // this can be caught.
    let (mut runtime, factory) = runtime_with_page(&[Capability::Window]);
    let _first = open_window(&mut runtime);
    let engine = factory.latest();

    for url in [
        "file:///C:/Windows/win.ini",
        "javascript:alert(1)",
        "https://evil.test/",
    ] {
        let answer = page_asks_to_create(&mut runtime, &engine, &json!({ "url": url }));
        assert_eq!(answer["ok"], false, "{url} must be refused: {answer}");
    }
    assert_eq!(
        runtime.windows.summaries().expect("the surface").len(),
        1,
        "no refused URL may have produced a window"
    );
}

#[test]
fn closing_a_window_closes_the_callers_window_whatever_the_page_names() {
    // A page may close the window it is in, and must not be able to name another one. The
    // page here names the *other* window; if the id were taken from the payload, the wrong
    // window would close.
    let (mut runtime, factory) = runtime_with_page(&[Capability::Window]);
    let first = open_window(&mut runtime);
    let engine = factory.latest();
    let answer = page_asks_to_create(
        &mut runtime,
        &engine,
        &json!({ "url": "https://app.example.com/second" }),
    );
    assert_eq!(answer["ok"], true);
    let summaries = runtime.windows.summaries().expect("the surface");
    let second = summaries
        .iter()
        .map(|summary| summary.id)
        .find(|id| *id != first)
        .expect("the second window");

    let request = json!({
        "id": "close-1",
        "action": "perform",
        "name": "close_window",
        "payload": { "window": second.to_string() },
    });
    engine.post(&serde_json::to_string(&request).expect("a serialisable request"));
    runtime.pump_pages(Duration::from_secs(0));

    let remaining: Vec<WindowId> = runtime
        .windows
        .summaries()
        .expect("the surface")
        .iter()
        .map(|summary| summary.id)
        .collect();
    assert_eq!(
        remaining,
        vec![second],
        "the caller's window must close and the named one must survive"
    );
}

#[test]
fn a_page_cannot_widen_the_grant_of_the_window_it_opens() {
    let (mut runtime, factory) = runtime_with_page(&[Capability::Window]);
    let _first = open_window(&mut runtime);
    let engine = factory.latest();

    let answer = page_asks_to_create(
        &mut runtime,
        &engine,
        &json!({
            "url": "https://app.example.com/second",
            "grant": ["deploy", "filesystem.write"]
        }),
    );
    assert_eq!(answer["ok"], true, "the URL and title are fine: {answer}");

    let summaries = runtime.windows.summaries().expect("the surface");
    let second = summaries
        .iter()
        .find(|summary| summary.url == "https://app.example.com/second")
        .expect("the new window");
    assert!(
        !second.granted.contains(Capability::Deploy),
        "a page must not be able to hand a new window an authority it does not hold"
    );
}

/// A places file in the system's own scratch directory, which this test owns.
fn scratch_places() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "ncui-window-actions-places-{}-{}.json",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Posts `perform` from the page with `payload` and pumps the host once.
fn page_performs(runtime: &mut Runtime, engine: &PageEngine, name: &str, payload: &Value) {
    let request = json!({
        "id": "perform-1",
        "action": "perform",
        "name": name,
        "payload": payload.clone(),
    });
    engine.post(&serde_json::to_string(&request).expect("a serialisable request"));
    let _ = runtime.pump_pages(Duration::from_secs(0));
}

#[test]
fn a_window_a_page_names_is_remembered_and_reopened_where_it_was() {
    // The whole remembered-window loop, driven by a page: the runtime is told where places live
    // and what the file held, the page opens a window under a name, the window is created at the
    // remembered place, and closing it writes the place the platform reported back.
    let path = scratch_places();
    std::fs::write(
        &path,
        br#"{"notes":{"x":120,"y":80,"width":640,"height":480}}"#,
    )
    .expect("the places file is written");

    let (mut runtime, factory) = runtime_with_page(&[Capability::Window]);
    runtime.windows.remember_windows_in(&path);
    let _first = open_window(&mut runtime);
    let engine = factory.latest();

    let answer = page_asks_to_create(
        &mut runtime,
        &engine,
        &json!({ "url": "https://app.example.com/notes", "remember": "notes" }),
    );
    assert_eq!(answer["ok"], true, "the call must be allowed: {answer}");

    let summaries = runtime.windows.summaries().expect("the surface");
    let remembered = summaries
        .iter()
        .find(|summary| summary.url == "https://app.example.com/notes")
        .expect("the window the page named");
    assert_eq!(
        (
            remembered.x,
            remembered.y,
            remembered.width,
            remembered.height
        ),
        (Some(120), Some(80), 640, 480),
        "a named window opens where it was, not where the request defaulted to"
    );
    let labels = ncui_observability::metrics::Labels::new();
    assert!(
        runtime
            .metrics
            .counter_value("window.remembered", &labels)
            .is_some_and(|count| count >= 1),
        "a window that will be put back where it was left must be visible as a count"
    );

    // The platform now says the window moved: the place written down must be this one, because a
    // window the user moved is a window whose place the user chose.
    factory.latest().place_at(ncui_webview::WindowGeometry {
        x: 300,
        y: 200,
        width: 700,
        height: 500,
    });
    page_performs(&mut runtime, &engine, "close_window", &json!({}));

    let written: std::collections::BTreeMap<String, ncui_webview::WindowGeometry> =
        serde_json::from_slice(&std::fs::read(&path).expect("the place was written"))
            .expect("the places file parses");
    assert_eq!(
        written.get("notes"),
        Some(&ncui_webview::WindowGeometry {
            x: 300,
            y: 200,
            width: 700,
            height: 500
        }),
        "closing the application writes down where its remembered windows were"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_window_a_page_does_not_name_is_not_remembered() {
    // Naming is what makes a window remembered: a page that opens an ordinary window must not
    // have its place written down under some invented name.
    let path = scratch_places();
    let (mut runtime, factory) = runtime_with_page(&[Capability::Window]);
    runtime.windows.remember_windows_in(&path);
    let _first = open_window(&mut runtime);
    let engine = factory.latest();

    let answer = page_asks_to_create(
        &mut runtime,
        &engine,
        &json!({ "url": "https://app.example.com/plain" }),
    );
    assert_eq!(answer["ok"], true, "{answer}");
    factory.latest().place_at(ncui_webview::WindowGeometry {
        x: 10,
        y: 10,
        width: 400,
        height: 400,
    });
    page_performs(&mut runtime, &engine, "close_window", &json!({}));

    assert!(
        !path.exists(),
        "a window the page did not name is not remembered at all"
    );
}

#[test]
fn a_remember_name_that_is_not_a_name_is_refused() {
    // The name is written into a file a person reads, so it is bounded and may not carry a
    // control character — the same rule a page's state key is held to.
    let path = scratch_places();
    let (mut runtime, factory) = runtime_with_page(&[Capability::Window]);
    runtime.windows.remember_windows_in(&path);
    let _first = open_window(&mut runtime);
    let engine = factory.latest();

    for name in [
        json!(""),
        json!("   "),
        json!("bad\nname"),
        json!("x".repeat(129)),
        json!(7),
    ] {
        let answer = page_asks_to_create(
            &mut runtime,
            &engine,
            &json!({ "url": "https://app.example.com/notes", "remember": name }),
        );
        assert_eq!(answer["ok"], false, "{name} must be refused: {answer}");
        assert_eq!(
            answer["error"]["code"], "validation",
            "the refusal must name the field: {answer}"
        );
    }
    assert_eq!(
        runtime.windows.summaries().expect("the surface").len(),
        1,
        "no refused name may have produced a window"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_page_changes_the_presentation_of_its_own_window() {
    // A page that is granted `window` may make its own window fullscreen, maximised, minimised or
    // focused. The answer is what the engine reports *after* the change, so a page learns what the
    // platform did rather than what it asked for.
    let (mut runtime, factory) = runtime_with_page(&[Capability::Window]);
    let first = open_window(&mut runtime);
    let engine = factory.latest();

    let request = json!({
        "id": "state-1",
        "action": "perform",
        "name": "window_state",
        "payload": { "fullscreen": true },
    });
    engine.post(&serde_json::to_string(&request).expect("a serialisable request"));
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    let reply = answer(&engine);
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["payload"]["window"], json!(first.to_string()));
    assert_eq!(reply["payload"]["state"]["fullscreen"], json!(true));
    assert_eq!(reply["payload"]["state"]["maximized"], json!(false));

    let labels = ncui_observability::metrics::Labels::new();
    assert!(
        runtime
            .metrics
            .counter_value("window.state_changes", &labels)
            .is_some_and(|count| count >= 1),
        "a presentation change must be visible as a count"
    );

    // No change at all is a read of the same state.
    let request = json!({ "id": "state-2", "action": "perform", "name": "window_state" });
    engine.post(&serde_json::to_string(&request).expect("a serialisable request"));
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    let reply = answer(&engine);
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["payload"]["state"]["fullscreen"], json!(true));
}

#[test]
fn a_page_cannot_change_the_presentation_of_a_window_it_does_not_own() {
    // The page names the *other* window. The target comes from the call's origin, so the caller's
    // own window is what changes — the named one is untouched.
    let (mut runtime, factory) = runtime_with_page(&[Capability::Window]);
    let _first = open_window(&mut runtime);
    let engine = factory.latest();
    let created = page_asks_to_create(
        &mut runtime,
        &engine,
        &json!({ "url": "https://app.example.com/second" }),
    );
    assert_eq!(created["ok"], true, "{created}");
    let summaries = runtime.windows.summaries().expect("the surface");
    let second = summaries
        .iter()
        .find(|summary| summary.url == "https://app.example.com/second")
        .map(|summary| summary.id)
        .expect("the second window");

    let request = json!({
        "id": "state-1",
        "action": "perform",
        "name": "window_state",
        "payload": { "window": second.to_string(), "maximized": true },
    });
    engine.post(&serde_json::to_string(&request).expect("a serialisable request"));
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    let reply = answer(&engine);
    assert_eq!(reply["ok"], true, "{reply}");
    assert!(
        reply["payload"]["window"] != json!(second.to_string()),
        "the caller's own window is what changes: {reply}"
    );
    assert_eq!(
        runtime.windows.window_state(second),
        Some(ncui_webview::WindowState::default()),
        "a window the page named must be untouched"
    );
}

#[test]
fn a_presentation_request_that_names_more_than_one_change_is_refused() {
    // Each of these is a different request, and a caller that sent two would be asking for
    // whichever the platform applied last.
    let (mut runtime, factory) = runtime_with_page(&[Capability::Window]);
    let _first = open_window(&mut runtime);
    let engine = factory.latest();

    for payload in [
        json!({ "fullscreen": true, "maximized": true }),
        json!({ "focus": false }),
        json!({ "minimized": "yes" }),
    ] {
        let request = json!({
            "id": "state-1",
            "action": "perform",
            "name": "window_state",
            "payload": payload,
        });
        engine.post(&serde_json::to_string(&request).expect("a serialisable request"));
        assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
        let reply = answer(&engine);
        assert_eq!(reply["ok"], false, "{payload} must be refused: {reply}");
        assert_eq!(
            reply["error"]["code"], "validation",
            "the refusal must name the field: {reply}"
        );
    }
}

#[test]
fn a_page_reshapes_the_window_it_is_in() {
    // The page renames its own window and gives it a new size. The answer is the window's record
    // *after* the platform took the change, so a page learns what its window now is.
    let (mut runtime, factory) = runtime_with_page(&[Capability::Window]);
    let first = open_window(&mut runtime);
    let engine = factory.latest();

    let request = json!({
        "id": "shape-1",
        "action": "perform",
        "name": "window_shape",
        "payload": { "title": "renamed", "width": 900, "height": 700 },
    });
    engine.post(&serde_json::to_string(&request).expect("a serialisable request"));
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    let reply = answer(&engine);
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["payload"]["window"], json!(first.to_string()));
    assert_eq!(reply["payload"]["title"], json!("renamed"));
    assert_eq!(reply["payload"]["width"], json!(900));

    let summary = runtime
        .windows
        .summaries()
        .expect("the surface")
        .into_iter()
        .find(|summary| summary.id == first)
        .expect("the caller's window");
    assert_eq!(
        summary.title, "renamed",
        "the list must report the window that exists"
    );
    assert_eq!((summary.width, summary.height), (900, 700));

    let labels = ncui_observability::metrics::Labels::new();
    assert!(
        runtime
            .metrics
            .counter_value("window.reshapes", &labels)
            .is_some_and(|count| count >= 1),
        "a reshape must be visible as a count"
    );
}

#[test]
fn a_shape_that_names_nothing_or_half_of_something_is_refused() {
    // Each of these would leave the window somewhere nobody asked for, so each is refused with the
    // field that produced it rather than applied as far as it goes.
    let (mut runtime, factory) = runtime_with_page(&[Capability::Window]);
    let _first = open_window(&mut runtime);
    let engine = factory.latest();

    for payload in [
        json!({}),
        json!({ "title": "   " }),
        json!({ "x": 10 }),
        json!({ "height": 480 }),
        json!({ "width": 1, "height": 480 }),
        json!({ "always_on_top": "yes" }),
    ] {
        let request = json!({
            "id": "shape-1",
            "action": "perform",
            "name": "window_shape",
            "payload": payload,
        });
        engine.post(&serde_json::to_string(&request).expect("a serialisable request"));
        assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
        let reply = answer(&engine);
        assert_eq!(reply["ok"], false, "{payload} must be refused: {reply}");
        assert_eq!(
            reply["error"]["code"], "validation",
            "the refusal must name the field: {reply}"
        );
    }
}

#[test]
fn a_page_cannot_reshape_a_window_it_does_not_own() {
    let (mut runtime, factory) = runtime_with_page(&[Capability::Window]);
    let _first = open_window(&mut runtime);
    let engine = factory.latest();
    let created = page_asks_to_create(
        &mut runtime,
        &engine,
        &json!({ "url": "https://app.example.com/second", "title": "second" }),
    );
    assert_eq!(created["ok"], true, "{created}");
    let summaries = runtime.windows.summaries().expect("the surface");
    let second = summaries
        .iter()
        .find(|summary| summary.url == "https://app.example.com/second")
        .map(|summary| summary.id)
        .expect("the second window");

    let request = json!({
        "id": "shape-1",
        "action": "perform",
        "name": "window_shape",
        "payload": { "window": second.to_string(), "title": "hijacked" },
    });
    engine.post(&serde_json::to_string(&request).expect("a serialisable request"));
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    let reply = answer(&engine);
    assert_eq!(reply["ok"], true, "{reply}");
    let summaries = runtime.windows.summaries().expect("the surface");
    assert_eq!(
        summaries
            .iter()
            .find(|summary| summary.id == second)
            .map(|summary| summary.title.as_str()),
        Some("second"),
        "a window the page named must be untouched"
    );
}

#[test]
fn a_page_without_the_window_capability_cannot_change_a_presentation() {
    let (mut runtime, factory) = runtime_with_page(&[]);
    let _first = open_window(&mut runtime);
    let engine = factory.latest();

    let request = json!({
        "id": "state-1",
        "action": "perform",
        "name": "window_state",
        "payload": { "fullscreen": true },
    });
    engine.post(&serde_json::to_string(&request).expect("a serialisable request"));
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    let reply = answer(&engine);
    assert_eq!(reply["ok"], false, "{reply}");
    assert_eq!(reply["error"]["code"], "capability_denied", "{reply}");
}
