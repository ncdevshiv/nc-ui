//! The window and `WebView` **host policy**: lifecycle, capability isolation, and the IPC seam.
//!
//! ADR-001 splits the system in two. The embedded `WebView` owns HTML parsing, CSS layout,
//! JavaScript execution, networking and the accessibility tree. NC-UI owns "the application
//! runtime around it: windows, native capabilities, GPU surfaces, IPC, scheduling, security,
//! and the agent interface". This crate is that second half, minus the parts other crates
//! already own.
//!
//! # What lives here, and what deliberately does not
//!
//! The security-relevant decisions of a `WebView` host are all *policy*: which origins a page
//! may navigate to, what a page is granted before it asks, what a message from a page is
//! allowed to request, and what happens when the engine dies. None of those require a window
//! to exist, and all of them are the parts that are easy to get subtly wrong. So they live
//! here, as pure logic over an injected [`WebViewBackend`].
//!
//! The native binding itself — `wry` on Windows, `WebKitGTK` on Linux, `WKWebView` on macOS —
//! implements [`WebViewBackend`] and lives in `ncui-webview-wry`, which is feature-gated off
//! by default so this crate still compiles and tests on a machine with no display and no
//! graphics stack. The split matters: everything here is policy, and policy that has never
//! met an engine has never been tested against one.
//!
//! # What a host engine is given
//!
//! An engine is created through [`HostFactory::spawn`], and it is handed the window's
//! [`HostSeam`]. That is deliberate. A navigation veto and a capability refusal are only
//! real if the engine asks the policy rather than keeping its own copy of the rules, so
//! [`WebViewBackend`] implementations receive the seam and call back into it. The engine
//! never sees a capability set or an allow-list it could disagree with.
//!
//! # The seam
//!
//! Everything crossing the boundary is one of two things, and neither carries a raw pointer,
//! a callback, or an engine type:
//!
//! * [`SeamMessage`] — a page asking the runtime to do something, validated against the
//!   page's [`CapabilityGrant`] before it is dispatched.
//! * [`NavigationRequest`] — the engine asking whether a navigation may proceed.
//!
//! Both are total: every input produces a verdict, never a panic, because both are driven by
//! content the runtime does not control.

pub mod capability;
pub mod lifecycle;
pub mod navigation;
pub mod request;
pub mod seam;
pub mod tray;
pub mod windows;

pub use capability::{CapabilityGrant, CapabilityRequest};
pub use lifecycle::{HostState, IllegalTransition, WindowLifecycle};
pub use navigation::{
    NavigationPolicy, NavigationRequest, Origin, Scheme, Verdict, is_loopback_host,
};
pub use request::{
    DEFAULT_WINDOW_HEIGHT, DEFAULT_WINDOW_TITLE, DEFAULT_WINDOW_WIDTH, Icon, MAX_ICON_EDGE,
    MAX_WINDOW_HEIGHT, MAX_WINDOW_WIDTH, MIN_WINDOW_HEIGHT, MIN_WINDOW_WIDTH, MonitorRect, Theme,
    WindowChange, WindowChangeError, WindowGeometry, WindowRequest, WindowRequestError,
    WindowShape, WindowState,
};
pub use seam::{
    ActionDefinition, ActionTable, ActionTableError, BRIDGE_PROTOCOL_VERSION, HostSeam,
    MAX_REQUEST_ID_BYTES, SeamError, SeamMessage, SeamRequest, SeamResponse,
};
pub use tray::{MAX_TRAY_TOOLTIP, TrayRequest};
pub use windows::{
    HostFactory, MAX_WINDOWS, MountedComponent, Window, WindowRegistry, WindowRejection,
    WindowSummary,
};

/// A message a page sent to its host.
///
/// Carries the raw body rather than a parsed value: the body is untrusted input, and the
/// seam is the thing that decides whether it is well-formed. Parsing it in the engine would
/// mean two parsers — one in the engine, one in the seam — and the one that runs first is
/// the one a hostile page meets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageMessage {
    /// The body the page posted, exactly as the engine received it.
    pub body: String,
}

impl PageMessage {
    /// Wraps a body.
    #[must_use]
    pub fn new(body: impl Into<String>) -> Self {
        Self { body: body.into() }
    }
}

/// The native binding behind this host.
///
/// Implementations wrap the platform embedding layer. Everything above this trait is policy
/// and is testable without one, which is why the crate compiles and tests on a machine with
/// no display.
pub trait WebViewBackend: core::fmt::Debug + Send {
    /// The error a backend reports when the engine itself fails — a crash, a lost
    /// connection, a process that will not start.
    fn last_error(&self) -> Option<String>;

    /// Whether the engine is still usable. A host that cannot answer this after a crash must
    /// report it rather than continuing to serve requests from a dead page.
    fn is_alive(&self) -> bool;

    /// Takes the next message the page has sent, if there is one.
    ///
    /// This is the page-to-host direction of the bridge. The engine does not interpret the
    /// message: it hands the body over, and the host evaluates it through the seam, which
    /// is where the capability check lives. An engine that parsed and acted on the message
    /// itself would be a second, ungoverned path into the runtime.
    fn poll_page_message(&self) -> Option<PageMessage>;

    /// Waits for the next message from the page, up to `timeout`.
    ///
    /// The default polls, so a backend with no blocking primitive still works. A backend
    /// that has one overrides this: the wait is what lets an application's page loop block
    /// between messages instead of sleeping, which is the difference between a bridge that
    /// answers in microseconds and one that answers in milliseconds.
    fn wait_page_message(&self, timeout: std::time::Duration) -> Option<PageMessage> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(message) = self.poll_page_message() {
                return Some(message);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(1).min(remaining));
        }
    }

    /// Evaluates a script in the page, returning whether it was accepted.
    ///
    /// This is the host-to-page direction of the bridge. A host that cannot evaluate a
    /// script — because the engine died, or because the page is not ready — says so rather
    /// than reporting a success that never happened.
    fn evaluate_script(&self, script: &str) -> bool;

    /// Initiate a window drag (titlebar drag region).
    ///
    /// Returns `true` when the drag was initiated, `false` when the backend
    /// cannot perform this operation. Called by the page when the user
    /// mousedowns on a `[data-ncui-drag-region]` element.
    fn drag_window(&self) -> bool {
        false
    }

    /// Where the window is and how big it is, or `None` when this backend cannot say.
    ///
    /// A backend that cannot report its geometry says so rather than guessing: a guessed
    /// geometry written into an application's state would move the user's window somewhere it
    /// has never been, and the next run would put it there.
    fn geometry(&self) -> Option<WindowGeometry> {
        None
    }

    /// How the window is being presented, or `None` when this backend cannot say.
    ///
    /// The same rule the geometry follows: a backend that cannot ask the platform says so rather
    /// than reporting what it was asked for, because the user is the one who maximises, minimises
    /// and focuses windows.
    fn window_state(&self) -> Option<WindowState> {
        None
    }

    /// Asks the platform for one change to the window's presentation, reporting the result.
    ///
    /// `None` when the backend cannot make the change at all — an engine without the call, a
    /// window that has gone — which is a different answer from a state the platform reported.
    /// The answer is the state *after* the change, read back from the platform rather than
    /// assumed, so a caller learns what happened instead of what it asked for.
    fn set_window_state(&self, change: WindowChange) -> Option<WindowState> {
        let _ = change;
        None
    }

    /// Asks the platform to make the window take `shape`, reporting whether it did.
    ///
    /// `false` when the backend cannot make the change at all — an engine without the call, a
    /// window that has gone. Unlike the presentation, there is nothing to report back: a shape is
    /// what the caller asked for, and a platform that applied it has nothing more to say.
    fn set_window_shape(&self, shape: &WindowShape) -> bool {
        let _ = shape;
        false
    }

    /// Whether the platform is drawing its interface light or dark, or `None` when this backend
    /// cannot say.
    ///
    /// A backend that cannot ask the platform says so rather than guessing: a page told "light" by a
    /// backend that made it up would follow a setting the user does not have.
    fn window_theme(&self) -> Option<Theme> {
        None
    }

    /// Shows the application's tray icon, or removes it when `tray` is `None`.
    ///
    /// `false` when this backend cannot show one at all — a build without the platform's call, or a
    /// machine whose tray refused it. The tray belongs to the application rather than to the window
    /// this is asked through: a window is only the handle to the engine that owns the platform's
    /// loop, and an application with no window can still be one a user needs to reach.
    fn set_tray(&self, tray: Option<TrayRequest>) -> bool {
        let _ = tray;
        false
    }

    /// Scales the window's page by `factor`, where `1.0` is the platform default.
    ///
    /// `false` when this backend cannot scale — an engine without the call, or a window
    /// that has gone. The result is not read back: the platform applies the scale and the
    /// next paint shows it.
    fn set_zoom(&self, factor: f64) -> bool {
        let _ = factor;
        false
    }

    /// Opens the platform's web inspector for this window.
    ///
    /// `false` when this backend cannot open one, or cannot answer whether it did — on a
    /// platform that opens the inspector without being able to report its state, that is
    /// the honest answer, and it is reported as such rather than claimed as a success.
    fn open_devtools(&self) -> bool {
        false
    }

    /// Closes the platform's web inspector for this window. See [`WebViewBackend::open_devtools`].
    fn close_devtools(&self) -> bool {
        false
    }

    /// Whether the platform's web inspector is open for this window.
    ///
    /// `None` when this backend cannot say — a platform that opens the inspector without a
    /// state query is reported as unknown, not as closed.
    fn is_devtools_open(&self) -> Option<bool> {
        None
    }

    /// The backend as [`Any`](core::any::Any), for a caller that knows a concrete engine
    /// type and needs a capability only it exposes (the CDP transport). A backend that has
    /// no such capability returns a value that downcasts to nothing.
    fn as_any(&self) -> &dyn core::any::Any;

    /// Sends one CDP command to this window's own `DevTools` endpoint (ADR-017 Phase 2).
    ///
    /// The default refuses: a backend without the transport says so by name rather than
    /// answering a delivery that never happened. The error string is the backend's own
    /// named reason, ready for the caller's report.
    fn cdp_send(&self, command: &serde_json::Value) -> Result<serde_json::Value, String> {
        let _ = command;
        Err("this backend has no CDP transport".to_owned())
    }

    /// Builds one child webview inside the parent window's frame
    /// (ADR-017 Phase 3), returning the engine's identity for it.
    ///
    /// The default refuses: a backend without the embedding capability says so by
    /// name rather than answering a creation that never happened.
    fn create_child_webview(
        &self,
        request: &ncui_webview_child::ChildWebviewRequest,
    ) -> Result<u64, String> {
        let _ = request;
        Err("this backend cannot embed child webviews".to_owned())
    }

    /// Moves a live child webview to a new rectangle.
    fn set_child_webview_bounds(
        &self,
        child: u64,
        request: &ncui_webview_child::ChildWebviewRequest,
    ) -> Result<(), String> {
        let _ = (child, request);
        Err("this backend cannot move child webviews".to_owned())
    }

    /// Closes a live child webview.
    fn close_child_webview(&self, child: u64) -> Result<(), String> {
        let _ = child;
        Err("this backend cannot close child webviews".to_owned())
    }
}

/// A backend whose engine can embed and drive child webviews (ADR-017 Phase 3).
///
/// Implemented by the wry host; the runtime reaches it by downcasting a window's
/// backend, so the runtime stays decoupled from the engine crate.
pub trait ChildWebviewCapable: core::fmt::Debug + Send + Sync + core::any::Any {
    /// Builds one child webview inside the parent's frame.
    fn create_child(
        &self,
        parent: ncui_core::WindowId,
        request: &ncui_webview_child::ChildWebviewRequest,
    ) -> Result<ncui_webview_child::ChildWebviewId, String>;

    /// Moves a live child webview to a new rectangle.
    fn set_child_bounds(
        &self,
        parent: ncui_core::WindowId,
        child: ncui_webview_child::ChildWebviewId,
        request: &ncui_webview_child::ChildWebviewRequest,
    ) -> Result<(), String>;

    /// Closes a live child webview.
    fn close_child(
        &self,
        parent: ncui_core::WindowId,
        child: ncui_webview_child::ChildWebviewId,
    ) -> Result<(), String>;

    /// Lists every live child this engine holds.
    fn list_children(&self) -> Vec<ncui_webview_child::ChildWebviewEntry> {
        Vec::new()
    }
}

/// Lifts an owned backend into a boxed one, so the registry can hold heterogeneous engines.
///
/// `?Sized` is what makes `Box<dyn WebViewBackend>` itself a backend: without it a registry
/// storing `Box<dyn WebViewBackend>` could not store that box again in another box, and every
/// call site would have to reach through the box by hand.
impl<T: WebViewBackend + ?Sized> WebViewBackend for Box<T> {
    fn as_any(&self) -> &dyn core::any::Any {
        T::as_any(self)
    }

    fn last_error(&self) -> Option<String> {
        T::last_error(self)
    }

    fn is_alive(&self) -> bool {
        T::is_alive(self)
    }

    fn poll_page_message(&self) -> Option<PageMessage> {
        T::poll_page_message(self)
    }

    fn evaluate_script(&self, script: &str) -> bool {
        T::evaluate_script(self, script)
    }

    fn drag_window(&self) -> bool {
        T::drag_window(self)
    }

    fn geometry(&self) -> Option<WindowGeometry> {
        T::geometry(self)
    }

    fn window_state(&self) -> Option<WindowState> {
        T::window_state(self)
    }

    fn set_window_state(&self, change: WindowChange) -> Option<WindowState> {
        T::set_window_state(self, change)
    }

    fn set_window_shape(&self, shape: &WindowShape) -> bool {
        T::set_window_shape(self, shape)
    }

    fn set_tray(&self, tray: Option<TrayRequest>) -> bool {
        T::set_tray(self, tray)
    }

    fn window_theme(&self) -> Option<Theme> {
        T::window_theme(self)
    }
}
