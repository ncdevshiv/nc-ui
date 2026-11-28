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

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ncui_core::{
    Capability, CapabilitySet, CorrelationId, ErrorKind, Limits, LimitsBuilder, NcError,
};
use ncui_runtime::Runtime;
use ncui_runtime::consent::{CONSENT_PROMPT_TITLE, ConsentPrompter, ConsentRequest};
use ncui_runtime::dispatch::dispatch;
use ncui_runtime::rpc::Request;
use ncui_security::authz::ConsentToken;
use ncui_webview::{
    ChildPageMessage, HostFactory, HostSeam, PageMessage, PageTarget, WebViewBackend,
    WindowConfirmation,
};
use ncui_webview_child::{ChildWebviewId, ChildWebviewRequest};
use serde_json::{Value, json};

/// The state the test's engine shares with the test.
#[derive(Debug)]
struct EngineState {
    /// Messages the page has "sent", waiting to be polled.
    outgoing: Mutex<Vec<String>>,
    /// Scripts the host has asked the engine to evaluate.
    evaluated: Mutex<Vec<String>>,
    /// Window drags the host has asked the engine to start.
    drags: Mutex<Vec<()>>,
    /// Child pages this parent engine owns.
    children: Mutex<BTreeMap<u64, PageEngine>>,
    /// Child creation requests the parent engine accepted.
    created: Mutex<Vec<ChildWebviewRequest>>,
    /// Geometry most recently applied to each child.
    bounds: Mutex<BTreeMap<u64, ChildWebviewRequest>>,
    /// Children the parent engine closed.
    closed: Mutex<Vec<u64>>,
    /// Confirmation prompts this window-owned surface has shown.
    confirmations: Mutex<Vec<(String, String)>>,
    /// Whether this engine can show a parented confirmation.
    confirmation_enabled: AtomicBool,
    /// The answer the test's person gives.
    confirmation_answer: AtomicBool,
    /// The test engine's child identity source.
    next_child: AtomicU64,
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            outgoing: Mutex::new(Vec::new()),
            evaluated: Mutex::new(Vec::new()),
            drags: Mutex::new(Vec::new()),
            children: Mutex::new(BTreeMap::new()),
            created: Mutex::new(Vec::new()),
            bounds: Mutex::new(BTreeMap::new()),
            closed: Mutex::new(Vec::new()),
            confirmations: Mutex::new(Vec::new()),
            confirmation_enabled: AtomicBool::new(false),
            confirmation_answer: AtomicBool::new(true),
            next_child: AtomicU64::new(0),
        }
    }
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

    fn child(&self, child: ChildWebviewId) -> Self {
        self.state
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&child.raw())
            .cloned()
            .expect("the child exists")
    }

    fn created_children(&self) -> usize {
        self.state
            .created
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Gives this engine a parented confirmation surface.
    fn enable_confirmation(&self) {
        self.state
            .confirmation_enabled
            .store(true, Ordering::SeqCst);
    }

    /// The confirmation prompts this engine was asked to show.
    fn confirmations(&self) -> Vec<(String, String)> {
        self.state
            .confirmations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The test's window-owned confirmation surface.
#[derive(Debug)]
struct TestWindowConfirmation {
    state: Arc<EngineState>,
}

impl WindowConfirmation for TestWindowConfirmation {
    fn confirm(&self, title: &str, message: &str) -> Result<bool, String> {
        self.state
            .confirmations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((title.to_owned(), message.to_owned()));
        Ok(self.state.confirmation_answer.load(Ordering::SeqCst))
    }
}

impl WebViewBackend for PageEngine {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

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

    fn confirmation(&self) -> Option<Box<dyn WindowConfirmation>> {
        self.state
            .confirmation_enabled
            .load(Ordering::SeqCst)
            .then(|| {
                Box::new(TestWindowConfirmation {
                    state: Arc::clone(&self.state),
                }) as Box<dyn WindowConfirmation>
            })
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

    fn create_child_webview(
        &self,
        request: &ChildWebviewRequest,
        _seam: &Arc<HostSeam>,
        ipc_tx: std::sync::mpsc::SyncSender<ChildPageMessage>,
    ) -> Result<u64, String> {
        let child = self.state.next_child.fetch_add(1, Ordering::SeqCst) + 1;
        self.state
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(child, PageEngine::new());
        self.state
            .created
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        drop(ipc_tx);
        Ok(child)
    }

    fn poll_child_webview_message(&self, child: u64) -> Option<ChildPageMessage> {
        let engine = self
            .state
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&child)
            .cloned()?;
        let body = engine
            .state
            .outgoing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()?;
        Some(ChildPageMessage::new(child, body))
    }

    fn evaluate_child_script(&self, child: u64, script: &str) -> bool {
        let Some(engine) = self
            .state
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&child)
            .cloned()
        else {
            return false;
        };
        engine.evaluate_script(script)
    }

    fn set_child_webview_bounds(
        &self,
        child: u64,
        request: &ChildWebviewRequest,
    ) -> Result<(), String> {
        self.state
            .bounds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(child, request.clone());
        Ok(())
    }

    fn close_child_webview(&self, child: u64) -> Result<(), String> {
        self.state
            .closed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(child);
        self.state
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&child);
        Ok(())
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

fn parent_engine(factory: &PageFactory, index: usize) -> PageEngine {
    factory
        .engines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(index)
        .cloned()
        .expect("the parent engine exists")
}

fn child_request() -> ChildWebviewRequest {
    ChildWebviewRequest {
        url: "https://app.example.com/child".to_owned(),
        x: 10,
        y: 20,
        width: 400,
        height: 300,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptReply {
    Approve,
    Decline,
    Fail,
}

#[derive(Debug)]
struct RecordingPrompter {
    reply: PromptReply,
    requests: Mutex<Vec<ConsentRequest>>,
}

impl RecordingPrompter {
    fn new(reply: PromptReply) -> Self {
        Self {
            reply,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ConsentRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ConsentPrompter for RecordingPrompter {
    fn confirm(&self, request: &ConsentRequest) -> ncui_core::Result<bool> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        match self.reply {
            PromptReply::Approve => Ok(true),
            PromptReply::Decline => Ok(false),
            PromptReply::Fail => Err(NcError::new(ErrorKind::Internal {
                operation: "test::consent_prompt".to_owned(),
                reason: "the approval surface is unavailable".to_owned(),
            })),
        }
    }
}

/// A prompter that records the ask, then waits on the test's answer — the
/// scripted person whose reading time the pump must not spend.
struct GatedPrompter {
    asked: std::sync::mpsc::Sender<()>,
    answers: Mutex<std::sync::mpsc::Receiver<bool>>,
}

impl core::fmt::Debug for GatedPrompter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GatedPrompter").finish_non_exhaustive()
    }
}

impl ConsentPrompter for GatedPrompter {
    fn confirm(&self, _request: &ConsentRequest) -> ncui_core::Result<bool> {
        let _ = self.asked.send(());
        let answers = self
            .answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(answers.recv().unwrap_or(false))
    }
}

fn runtime_with_page_grant_and_prompter(
    capabilities: &[Capability],
    prompter: Arc<dyn ConsentPrompter>,
) -> (Runtime, PageFactory) {
    let mut runtime = Runtime::start(Limits::default())
        .expect("runtime should start")
        .with_consent_prompter(prompter);
    runtime.grant_page_capabilities(capabilities);
    attach_page(runtime)
}

fn invoke_page(
    runtime: &mut Runtime,
    engine: &PageEngine,
    id: &str,
    name: &str,
    payload: &Value,
) -> String {
    engine.post(
        &json!({
            "id": id,
            "action": "perform",
            "name": name,
            "payload": payload
        })
        .to_string(),
    );
    // A call that reaches the consent prompt parks rather than answering in
    // place — the person is asked off the runtime lock — so pump until the
    // answer lands: the first pass handles the message, later passes settle it.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        runtime.pump_pages(Duration::from_secs(0));
        if let Some(script) = engine.scripts().last() {
            return script.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the page received no answer to {id}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn page_subject(window: ncui_core::WindowId) -> String {
    format!("page:{window}")
}

fn register_counting_write(runtime: &Runtime) -> Arc<AtomicU64> {
    let executions = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&executions);
    let _registration = runtime
        .register_page_command(
            "write_note",
            CapabilitySet::empty().with(Capability::FilesystemWrite),
            "Write a note",
            move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "written": true }))
            },
        )
        .expect("register the destructive page command");
    executions
}

fn mint_page_consent(
    runtime: &mut Runtime,
    window: ncui_core::WindowId,
    method: &str,
    required: CapabilitySet,
    params: &Value,
) -> ConsentToken {
    runtime
        .mint_consent(
            CorrelationId::new(),
            &page_subject(window),
            method,
            required,
            params,
        )
        .expect("mint an exact page-operation token")
}

fn with_consent_token(mut params: Value, token: &ConsentToken) -> Value {
    let Value::Object(operation) = &mut params else {
        panic!("consent is bound to object parameters");
    };
    operation.insert("consent_token".to_owned(), token.to_value());
    params
}

fn assert_page_refused(answer: &str, id: &str, code: &str) {
    assert!(
        answer.contains(&format!("\"id\":\"{id}\""))
            && answer.contains(&format!("\"code\":\"{code}\"")),
        "{id} must be refused as {code}: {answer}"
    );
}

fn create_child(runtime: &Runtime, parent: ncui_core::WindowId) -> ChildWebviewId {
    runtime
        .create_child_webview(parent, &child_request())
        .expect("the parent is open")
        .expect("the engine creates the child")
}

fn reply_id_from(script: &str) -> String {
    script
        .split("__runCommand(\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("a reply id")
        .to_owned()
}

#[test]
fn a_child_page_is_listed_and_its_answer_returns_only_to_that_child() {
    let (mut runtime, factory) = runtime_with_page_and_grant(&[Capability::Window]);
    let parent = open_window(&mut runtime);
    let child = create_child(&runtime, parent);
    let parent_engine = parent_engine(&factory, 0);
    let child_engine = parent_engine.child(child);

    let listed = runtime.list_child_webviews();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, child);
    assert_eq!(listed[0].parent, parent);
    assert_eq!(listed[0].bounds, child_request());
    assert_eq!(child.to_string(), "child-0000000000000001");
    assert!(
        runtime
            .windows
            .grant_for_target(PageTarget::Child { parent, child })
            .expect("the child is live")
            .expect("the child has a seam")
            .is_empty(),
        "a child must not inherit its parent's authority"
    );

    child_engine.post("{\"id\":\"child-describe\",\"action\":\"describe\"}");
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    assert!(parent_engine.scripts().is_empty());
    let answer = child_engine.scripts().pop().expect("the child answer");
    assert!(answer.contains("\"id\":\"child-describe\""));
    assert!(answer.contains("\"capabilities\":[]"));
}

#[test]
fn a_child_initial_url_is_vetted_before_the_engine_builds_it() {
    let (mut runtime, factory) = runtime_with_page();
    let parent = open_window(&mut runtime);
    let mut request = child_request();
    request.url = "https://untrusted.example/".to_owned();

    let refused = runtime
        .create_child_webview(parent, &request)
        .expect("the parent is open")
        .expect_err("the child URL is outside the parent's navigation policy");
    assert!(refused.contains("not an allowed origin"), "{refused}");
    assert_eq!(parent_engine(&factory, 0).created_children(), 0);
    assert!(runtime.list_child_webviews().is_empty());
}

#[test]
fn a_child_cannot_be_moved_closed_or_reached_through_another_parent() {
    let (mut runtime, factory) = runtime_with_page();
    let parent = open_window(&mut runtime);
    let other = open_window(&mut runtime);
    let child = create_child(&runtime, parent);
    let mut bounds = child_request();
    bounds.url.clear();
    bounds.x = 30;

    assert!(
        runtime
            .set_child_webview_bounds(other, child, &bounds)
            .expect("the other parent is open")
            .is_err(),
        "another parent cannot move this child"
    );
    assert!(
        runtime
            .close_child_webview(other, child)
            .expect("the other parent is open")
            .is_err(),
        "another parent cannot close this child"
    );
    assert!(
        !runtime
            .windows
            .post_to_target(
                PageTarget::Child {
                    parent: other,
                    child,
                },
                "1;",
            )
            .expect("the other parent is open"),
        "another parent cannot evaluate in this child"
    );
    assert_eq!(runtime.list_child_webviews().len(), 1);
    assert_eq!(
        parent_engine(&factory, 0).child(child).scripts(),
        Vec::<String>::new()
    );
}

#[test]
fn child_bounds_preserve_the_url_and_close_removes_the_live_entry() {
    let (mut runtime, factory) = runtime_with_page();
    let parent = open_window(&mut runtime);
    let child = create_child(&runtime, parent);
    let mut bounds = child_request();
    bounds.url.clear();
    bounds.x = 40;
    bounds.width = 320;

    runtime
        .set_child_webview_bounds(parent, child, &bounds)
        .expect("the parent is open")
        .expect("the child moves");
    let listed = runtime.list_child_webviews();
    assert_eq!(listed[0].bounds.url, child_request().url);
    assert_eq!(listed[0].bounds.x, 40);
    assert_eq!(listed[0].bounds.width, 320);

    runtime
        .close_child_webview(parent, child)
        .expect("the parent is open")
        .expect("the child closes");
    assert!(runtime.list_child_webviews().is_empty());
    assert!(!parent_engine(&factory, 0).evaluate_child_script(child.raw(), "1;"));
}

#[test]
fn a_child_cannot_use_an_action_that_targets_its_parent_window() {
    let (mut runtime, factory) = runtime_with_page_and_grant(&[Capability::Window]);
    let parent = open_window(&mut runtime);
    let child = create_child(&runtime, parent);
    let parent_engine = parent_engine(&factory, 0);
    let child_engine = parent_engine.child(child);

    child_engine.post("{\"id\":\"child-drag\",\"action\":\"perform\",\"name\":\"drag_region\"}");
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    assert_eq!(parent_engine.drag_count(), 0);
    let answer = child_engine.scripts().pop().expect("the refusal");
    assert!(answer.contains("\"code\":\"capability_denied\""));
    assert!(answer.contains("\"id\":\"child-drag\""));
}

#[test]
fn a_child_cannot_settle_a_sibling_pages_handler_call() {
    let runtime = Runtime::start(Limits::default()).expect("runtime should start");
    runtime
        .page_commands
        .register_js_command("js.echo", CapabilitySet::empty(), "Echo in the page")
        .expect("the command registers");
    let (mut runtime, factory) = attach_page(runtime);
    let parent = open_window(&mut runtime);
    let first = create_child(&runtime, parent);
    let second = create_child(&runtime, parent);
    let parent_engine = parent_engine(&factory, 0);
    let first_engine = parent_engine.child(first);
    let second_engine = parent_engine.child(second);

    first_engine.post("{\"id\":\"first-call\",\"action\":\"perform\",\"name\":\"js.echo\"}");
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    let run = first_engine
        .scripts()
        .pop()
        .expect("the first child's handler starts");
    let reply_id = reply_id_from(&run);

    second_engine.post(&format!(
        "{{\"id\":\"{reply_id}\",\"action\":\"command_result\",\"ok\":true,\"payload\":{{\"spoofed\":true}}}}"
    ));
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    assert_eq!(first_engine.scripts().len(), 1);
    assert!(second_engine.scripts().is_empty());

    first_engine.post(&format!(
        "{{\"id\":\"{reply_id}\",\"action\":\"command_result\",\"ok\":true,\"payload\":{{\"echo\":\"first\"}}}}"
    ));
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    let first_scripts = first_engine.scripts();
    assert_eq!(first_scripts.len(), 2);
    let settled = first_scripts.last().expect("the real answer");
    assert!(settled.contains("\"id\":\"first-call\""));
    assert!(settled.contains("\"echo\":\"first\""));
    assert!(!settled.contains("spoofed"));
    assert!(second_engine.scripts().is_empty());
    assert!(parent_engine.scripts().is_empty());
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
fn a_destructive_application_command_without_host_approval_fails_closed() {
    let (mut runtime, factory) = runtime_with_page_and_grant(&[Capability::FilesystemWrite]);
    let executions = register_counting_write(&runtime);
    let _window = open_window(&mut runtime);
    let engine = parent_engine(&factory, 0);

    for (id, payload) in [
        ("absent", json!({})),
        ("boolean", json!({ "consent": true })),
    ] {
        let answer = invoke_page(&mut runtime, &engine, id, "write_note", &payload);
        assert!(
            answer.contains("\"code\":\"capability_denied\"")
                && answer.contains(&format!("\"id\":\"{id}\"")),
            "a page cannot authorize itself with a boolean: {answer}"
        );
    }
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "a refused operation must not reach its handler"
    );
}

#[test]
fn a_destructive_page_command_validates_and_spends_consent_tokens() {
    let prompter = Arc::new(RecordingPrompter::new(PromptReply::Approve));
    let (mut runtime, factory) = runtime_with_page_grant_and_prompter(
        &[Capability::FilesystemWrite],
        Arc::clone(&prompter) as Arc<dyn ConsentPrompter>,
    );
    let required = CapabilitySet::empty().with(Capability::FilesystemWrite);
    let executions = register_counting_write(&runtime);
    let owner = open_window(&mut runtime);
    let other = open_window(&mut runtime);
    let engine = parent_engine(&factory, 0);
    let params = json!({ "path": "note.txt", "text": "approved" });
    let token = mint_page_consent(&mut runtime, owner, "write_note", required, &params);
    let wrong_command =
        mint_page_consent(&mut runtime, owner, "another_command", required, &params);
    let wrong_capability = mint_page_consent(
        &mut runtime,
        owner,
        "write_note",
        CapabilitySet::empty().with(Capability::FilesystemRead),
        &params,
    );
    let wrong_subject = mint_page_consent(&mut runtime, other, "write_note", required, &params);
    let altered = json!({ "path": "note.txt", "text": "not approved" });

    for (id, payload, code) in [
        (
            "forged-text",
            json!({ "consent_token": "forged" }),
            "validation",
        ),
        (
            "wrong-command",
            with_consent_token(params.clone(), &wrong_command),
            "capability_denied",
        ),
        (
            "wrong-capability",
            with_consent_token(params.clone(), &wrong_capability),
            "capability_denied",
        ),
        (
            "wrong-subject",
            with_consent_token(params.clone(), &wrong_subject),
            "capability_denied",
        ),
        (
            "altered-arguments",
            with_consent_token(altered, &token),
            "capability_denied",
        ),
    ] {
        let answer = invoke_page(&mut runtime, &engine, id, "write_note", &payload);
        assert_page_refused(&answer, id, code);
        if id == "forged-text" {
            assert!(answer.contains("consent_token"), "{answer}");
        }
    }

    let allowed = invoke_page(
        &mut runtime,
        &engine,
        "valid-token",
        "write_note",
        &with_consent_token(params, &token),
    );
    assert!(
        allowed.contains("\"id\":\"valid-token\"")
            && allowed.contains("\"ok\":true")
            && allowed.contains("\"written\":true"),
        "the exact owner operation must run: {allowed}"
    );
    let replayed = invoke_page(
        &mut runtime,
        &engine,
        "replayed-token",
        "write_note",
        &with_consent_token(json!({ "path": "note.txt", "text": "approved" }), &token),
    );
    assert_page_refused(&replayed, "replayed-token", "capability_denied");
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(
        prompter.requests().is_empty(),
        "every supplied-token refusal must fail without falling back to a prompt"
    );
    let audited = runtime
        .audit
        .to_vec()
        .into_iter()
        .find(|event| {
            event.action == "page.write_note"
                && event.request_id == token.correlation()
                && event.outcome.as_str() == "allowed"
        })
        .expect("the successful operation is audited under the token correlation");
    assert_eq!(audited.agent_id, page_subject(owner));
    assert_eq!(audited.outcome.as_str(), "allowed");
    assert_eq!(audited.capabilities_used, required);
}

#[test]
fn host_approval_presents_the_exact_page_operation_before_it_runs() {
    let prompter = Arc::new(RecordingPrompter::new(PromptReply::Approve));
    let (mut runtime, factory) = runtime_with_page_grant_and_prompter(
        &[Capability::FilesystemWrite],
        Arc::clone(&prompter) as Arc<dyn ConsentPrompter>,
    );
    let required = CapabilitySet::empty().with(Capability::FilesystemWrite);
    let executions = register_counting_write(&runtime);
    let window = open_window(&mut runtime);
    let engine = parent_engine(&factory, 0);

    let answer = invoke_page(
        &mut runtime,
        &engine,
        "prompted-approval",
        "write_note",
        &json!({
            "path": "note.txt",
            "text": "approved",
            "consent": true,
            "nested": { "b": 2, "a": 1 }
        }),
    );
    assert!(
        answer.contains("\"id\":\"prompted-approval\"")
            && answer.contains("\"ok\":true")
            && answer.contains("\"written\":true"),
        "host approval must permit the exact operation: {answer}"
    );
    let requests = prompter.requests();
    assert_eq!(requests.len(), 1, "one operation asks for approval once");
    let request = &requests[0];
    assert_eq!(request.subject(), page_subject(window).as_str());
    assert_eq!(request.method(), "write_note");
    assert_eq!(request.capabilities(), required);
    assert_eq!(
        request.params(),
        &json!({
            "path": "note.txt",
            "text": "approved",
            "nested": { "a": 1, "b": 2 }
        })
    );
    let message = request.message();
    assert!(message.contains(&page_subject(window)), "{message}");
    assert!(message.contains("write_note"), "{message}");
    assert!(
        message.contains("note.txt") && message.contains("approved"),
        "{message}"
    );
    assert!(
        !message.contains("\"consent\"") && !message.contains("consent_token"),
        "{message}"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let audited = runtime
        .audit
        .to_vec()
        .into_iter()
        .find(|event| event.action == "page.write_note")
        .expect("the prompted operation is audited");
    assert_eq!(audited.agent_id, page_subject(window));
    assert_eq!(audited.outcome.as_str(), "allowed");
}

#[test]
fn a_page_approval_uses_the_window_parented_prompt_before_the_generic_host_prompt() {
    let prompter = Arc::new(RecordingPrompter::new(PromptReply::Approve));
    let (mut runtime, factory) = runtime_with_page_grant_and_prompter(
        &[Capability::FilesystemWrite],
        Arc::clone(&prompter) as Arc<dyn ConsentPrompter>,
    );
    // The runtime-default native prompt is what a parented window supersedes; a custom
    // boundary would be asked instead, which the decline/fail tests below rely on.
    let as_default: Arc<dyn ConsentPrompter> = prompter.clone();
    runtime.set_default_consent_prompter(as_default);
    let executions = register_counting_write(&runtime);
    let _window = open_window(&mut runtime);
    let engine = parent_engine(&factory, 0);
    engine.enable_confirmation();

    let answer = invoke_page(
        &mut runtime,
        &engine,
        "parented-approval",
        "write_note",
        &json!({ "path": "note.txt", "text": "approved" }),
    );

    assert!(
        answer.contains("\"ok\":true") && answer.contains("\"written\":true"),
        "the parented approval should permit the operation: {answer}"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(
        prompter.requests().is_empty(),
        "a window that can parent its prompt must not fall back to an unparented generic prompt"
    );
    let confirmations = engine.confirmations();
    assert_eq!(confirmations.len(), 1);
    assert_eq!(confirmations[0].0, CONSENT_PROMPT_TITLE);
    assert!(
        confirmations[0].1.contains("write_note")
            && confirmations[0].1.contains("note.txt")
            && confirmations[0].1.contains("approved"),
        "the parented prompt must show the exact operation: {:?}",
        confirmations[0].1
    );
}

#[test]
fn a_declined_or_failed_page_approval_never_runs_the_command() {
    for reply in [PromptReply::Decline, PromptReply::Fail] {
        let prompter = Arc::new(RecordingPrompter::new(reply));
        let (mut runtime, factory) = runtime_with_page_grant_and_prompter(
            &[Capability::FilesystemWrite],
            Arc::clone(&prompter) as Arc<dyn ConsentPrompter>,
        );
        let executions = register_counting_write(&runtime);
        let _window = open_window(&mut runtime);
        let engine = parent_engine(&factory, 0);

        let answer = invoke_page(
            &mut runtime,
            &engine,
            "withheld-approval",
            "write_note",
            &json!({ "path": "note.txt" }),
        );
        assert_page_refused(&answer, "withheld-approval", "capability_denied");
        assert_eq!(
            executions.load(Ordering::SeqCst),
            0,
            "{reply:?} must not execute"
        );
        assert_eq!(prompter.requests().len(), 1, "{reply:?} must be observable");
    }
}

#[test]
fn a_consent_prompt_parks_the_call_not_the_pump() {
    // The lock-hostage regression test the F2 arc measured: a prompt waited on
    // under `&mut Runtime` parks every other caller for the answer's whole life.
    // The pump must handle the next message while the person is still reading.
    let (asked, asked_rx) = std::sync::mpsc::channel::<()>();
    let (answer_tx, answer_rx) = std::sync::mpsc::channel::<bool>();
    let prompter = Arc::new(GatedPrompter {
        asked,
        answers: Mutex::new(answer_rx),
    });
    let (mut runtime, factory) = runtime_with_page_grant_and_prompter(
        &[Capability::FilesystemWrite, Capability::FilesystemRead],
        prompter,
    );
    let executions = register_counting_write(&runtime);
    let pinged = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&pinged);
    let _ping = runtime
        .register_page_command(
            "ping",
            CapabilitySet::empty().with(Capability::FilesystemRead),
            "Ping",
            move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "pong": true }))
            },
        )
        .expect("register a non-destructive page command");
    let _window = open_window(&mut runtime);
    let engine = parent_engine(&factory, 0);

    // The destructive call parks: the person is asked by a worker, and the
    // pump returns rather than spending the person's reading time.
    engine.post(
        &json!({
            "id": "parked",
            "action": "perform",
            "name": "write_note",
            "payload": { "path": "note.txt", "text": "first" }
        })
        .to_string(),
    );
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    asked_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the destructive call reaches the person");
    assert!(
        engine.scripts().is_empty(),
        "the parked call has not been answered"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "consent has not run the handler"
    );

    // While the person decides, an unrelated call is served — the prompt is not
    // a hostage the runtime holds for them.
    engine.post(
        &json!({
            "id": "during-prompt",
            "action": "perform",
            "name": "ping",
            "payload": {}
        })
        .to_string(),
    );
    assert_eq!(runtime.pump_pages(Duration::from_secs(0)), 1);
    let scripts = engine.scripts();
    assert_eq!(scripts.len(), 1, "the unrelated call is answered");
    assert!(
        scripts[0].contains("\"id\":\"during-prompt\"") && scripts[0].contains("pong"),
        "the unrelated call completes during the prompt: {}",
        scripts[0]
    );

    // The person's answer wakes the settle worker; the pump delivers the reply
    // once it lands, which is why this pumps until the answer arrives.
    answer_tx.send(true).expect("the person answers");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let settled = loop {
        runtime.pump_pages(Duration::from_secs(0));
        let scripts = engine.scripts();
        if scripts.len() == 2 {
            break scripts[1].clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the parked call never settled"
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(
        settled.contains("\"id\":\"parked\"") && settled.contains("\"written\":true"),
        "the approved call completes: {settled}"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[test]
fn a_declined_answer_settles_the_parked_call_as_a_refusal() {
    let (asked, asked_rx) = std::sync::mpsc::channel::<()>();
    let (answer_tx, answer_rx) = std::sync::mpsc::channel::<bool>();
    let prompter = Arc::new(GatedPrompter {
        asked,
        answers: Mutex::new(answer_rx),
    });
    let (mut runtime, factory) =
        runtime_with_page_grant_and_prompter(&[Capability::FilesystemWrite], prompter);
    let executions = register_counting_write(&runtime);
    let _window = open_window(&mut runtime);
    let engine = parent_engine(&factory, 0);

    engine.post(
        &json!({
            "id": "declined",
            "action": "perform",
            "name": "write_note",
            "payload": { "path": "note.txt" }
        })
        .to_string(),
    );
    runtime.pump_pages(Duration::from_secs(0));
    asked_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the call reaches the person");
    answer_tx.send(false).expect("the person declines");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let settled = loop {
        runtime.pump_pages(Duration::from_secs(0));
        let scripts = engine.scripts();
        if let Some(script) = scripts.first() {
            break script.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the refusal never settled the call"
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_page_refused(&settled, "declined", "capability_denied");
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "a declined call never runs"
    );
}

#[test]
fn a_custom_approval_boundary_is_asked_even_when_the_window_can_parent() {
    // The window-parented prompt supersedes only the runtime-default native prompt. An
    // embedder that installs its own boundary — a test harness, an embedding application
    // with its own consent UX — must be asked instead, or every automated run of a
    // destructive page operation would wedge behind a dialog no one can click.
    let prompter = Arc::new(RecordingPrompter::new(PromptReply::Approve));
    let (mut runtime, factory) = runtime_with_page_grant_and_prompter(
        &[Capability::FilesystemWrite],
        Arc::clone(&prompter) as Arc<dyn ConsentPrompter>,
    );
    let executions = register_counting_write(&runtime);
    let _window = open_window(&mut runtime);
    let engine = parent_engine(&factory, 0);
    engine.enable_confirmation();

    let answer = invoke_page(
        &mut runtime,
        &engine,
        "custom-boundary",
        "write_note",
        &json!({ "path": "note.txt", "text": "approved" }),
    );

    assert!(
        answer.contains("\"ok\":true"),
        "the custom boundary approves the operation: {answer}"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(
        prompter.requests().len(),
        1,
        "the embedder's own boundary must be asked"
    );
    assert!(
        engine.confirmations().is_empty(),
        "a window that can parent its prompt must not bypass the embedder's boundary"
    );
}

#[test]
fn a_rate_limited_page_is_refused_before_the_host_is_prompted() {
    let limits = LimitsBuilder::new()
        .agent_rate(1, 1)
        .build()
        .expect("a one-request page budget");
    let prompter = Arc::new(RecordingPrompter::new(PromptReply::Approve));
    let approval_surface = Arc::clone(&prompter) as Arc<dyn ConsentPrompter>;
    let mut runtime = Runtime::start(limits)
        .expect("runtime should start")
        .with_consent_prompter(approval_surface);
    runtime.grant_page_capabilities(&[Capability::FilesystemWrite]);
    let executions = register_counting_write(&runtime);
    let (mut runtime, factory) = attach_page(runtime);
    let _window = open_window(&mut runtime);
    let engine = parent_engine(&factory, 0);

    let allowed = invoke_page(
        &mut runtime,
        &engine,
        "budget-spent",
        "describe_metrics",
        &json!({}),
    );
    assert!(allowed.contains("\"ok\":true"), "{allowed}");
    let refused = invoke_page(
        &mut runtime,
        &engine,
        "over-budget",
        "write_note",
        &json!({}),
    );
    assert_page_refused(&refused, "over-budget", "limit_exceeded");
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    assert!(prompter.requests().is_empty());
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
