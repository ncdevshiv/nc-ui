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
pub mod filedrop;
pub mod lifecycle;
pub mod menu;
pub mod native;
pub mod navigation;
pub mod request;
pub mod seam;
pub mod tray;
pub mod windows;

use std::sync::Arc;

use ncui_core::WindowId;

pub use capability::{CapabilityGrant, CapabilityRequest};
pub use lifecycle::{HostState, IllegalTransition, WindowLifecycle};
pub use menu::{MenuEntry, MenuEvent, MenuRequest, MenuSubmenu};
pub use native::{
    MAX_NATIVE_PAYLOAD_BYTES, MAX_QUEUED_NATIVE_EVENTS, NativeViewBackend, NativeViewCommand,
    NativeViewError, NativeViewEvent, RoutingFactory, TrustedServiceSeam,
    TrustedServiceSeamFactory,
};
pub use navigation::{
    NavigationPolicy, NavigationRequest, Origin, Scheme, Verdict, is_loopback_host,
};
pub use request::{
    DEFAULT_WINDOW_HEIGHT, DEFAULT_WINDOW_TITLE, DEFAULT_WINDOW_WIDTH, Icon, MAX_ICON_EDGE,
    MAX_NATIVE_PROPS_BYTES, MAX_PROVIDER_ID_BYTES, MAX_WINDOW_HEIGHT, MAX_WINDOW_WIDTH,
    MIN_WINDOW_HEIGHT, MIN_WINDOW_WIDTH, MonitorRect, ResizeEdge, Theme, WindowChange,
    WindowChangeError, WindowContent, WindowGeometry, WindowRequest, WindowRequestError,
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

/// A typed failure from an engine's Chrome `DevTools` Protocol transport.
///
/// The stable [`code`](Self::code) is part of the runtime and agent-facing wire contract. Keeping
/// the type engine-neutral lets runtime policy preserve it without depending on a native backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CdpError {
    /// The engine's debugging endpoint file is missing, unreadable, or malformed.
    PortUnavailable { reason: String },
    /// The endpoint refused the connection or WebSocket upgrade.
    ConnectFailed { reason: String },
    /// The command was malformed or the transport stopped answering safely.
    TransportFailed { reason: String },
    /// The engine received the command but reported a protocol error for it.
    CommandFailed { reason: String },
    /// This window could not be bound unambiguously to one page target.
    TargetBindFailed { reason: String },
    /// The platform's embedded engine does not expose a Chromium CDP endpoint.
    UnsupportedPlatform { reason: String },
    /// A caller supplied a command without a valid CDP request identity.
    InvalidCommand { reason: String },
}

impl CdpError {
    /// The stable machine-readable failure name.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::PortUnavailable { .. } => "cdp_port_unavailable",
            Self::ConnectFailed { .. } => "cdp_connect_failed",
            Self::TransportFailed { .. } => "cdp_transport_failed",
            Self::CommandFailed { .. } => "cdp_command_failed",
            Self::TargetBindFailed { .. } => "cdp_target_bind_failed",
            Self::UnsupportedPlatform { .. } => "cdp_platform_unsupported",
            Self::InvalidCommand { .. } => "cdp_invalid_command",
        }
    }
}

impl core::fmt::Display for CdpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PortUnavailable { reason } => write!(f, "CDP port unavailable: {reason}"),
            Self::ConnectFailed { reason } => write!(f, "CDP connect failed: {reason}"),
            Self::TransportFailed { reason } => write!(f, "CDP transport failed: {reason}"),
            Self::CommandFailed { reason } => write!(f, "CDP command failed: {reason}"),
            Self::TargetBindFailed { reason } => write!(f, "CDP target bind failed: {reason}"),
            Self::UnsupportedPlatform { reason } => write!(f, "CDP unsupported: {reason}"),
            Self::InvalidCommand { reason } => write!(f, "CDP invalid command: {reason}"),
        }
    }
}

impl std::error::Error for CdpError {}

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

/// The maximum number of queued messages on one child IPC channel.
///
/// This matches the page bridge's maximum pending calls. A full channel applies backpressure
/// instead of allowing a page to grow host memory without bound.
pub const MAX_CHILD_IPC_MESSAGES: usize = 1024;

/// A message a child page sent to its host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildPageMessage {
    /// The engine-issued identity of the child that sent the message.
    pub child: u64,
    /// The body the child page posted, exactly as the engine received it.
    pub body: String,
}

impl ChildPageMessage {
    /// Wraps a child identity and body.
    #[must_use]
    pub fn new(child: u64, body: impl Into<String>) -> Self {
        Self {
            child,
            body: body.into(),
        }
    }
}

/// The page a host operation addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageTarget {
    /// The parent window's own page.
    Window(WindowId),
    /// A child embedded in a parent window.
    Child {
        /// The window that owns the child.
        parent: WindowId,
        /// The child's engine-issued identity.
        child: ncui_webview_child::ChildWebviewId,
    },
}

impl PageTarget {
    /// The window that owns the target page.
    #[must_use]
    pub const fn parent(&self) -> WindowId {
        match self {
            Self::Window(window) => *window,
            Self::Child { parent, .. } => *parent,
        }
    }

    /// Whether the target is an embedded child rather than the window's own page.
    #[must_use]
    pub const fn is_child(&self) -> bool {
        matches!(self, Self::Child { .. })
    }
}

impl core::fmt::Display for PageTarget {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Window(window) => write!(f, "{window}"),
            Self::Child { parent, child } => write!(f, "child:{parent}:{child}"),
        }
    }
}

/// A synchronous confirmation surface owned by one top-level native window.
///
/// The platform dialog is shown by the engine that owns the window, because that is the only
/// place the window object can be used as the dialog's parent without copying raw handles or
/// unsafe pointers across threads. The runtime receives only the person's answer.
pub trait WindowConfirmation: core::fmt::Debug + Send + Sync {
    /// Shows a destructive-operation confirmation parented to the window.
    ///
    /// The text has already been bounded and validated by runtime consent policy. `Ok(true)`
    /// means the person explicitly approved; every other completed answer is `Ok(false)`.
    ///
    /// # Errors
    ///
    /// Returns an engine-owned failure when the window is gone or the platform dialog cannot
    /// be shown or completed. The runtime treats an error as withholding approval.
    fn confirm(&self, title: &str, message: &str) -> Result<bool, String>;
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

    /// A confirmation surface owned by this backend's top-level window, when it can show one.
    ///
    /// `None` means the runtime uses its ordinary host prompt. When a backend does provide one,
    /// its failure is final rather than a reason to fall back to an unparented prompt: consent
    /// must never become less clearly attached to the asking window merely because parenting failed.
    fn confirmation(&self) -> Option<Box<dyn WindowConfirmation>> {
        None
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

    /// Initiate an edge-resize drag on a frameless window.
    ///
    /// The complement of [`WebViewBackend::drag_window`]: a window the OS did
    /// not decorate has no borders to grab, so the page's own edge regions ask
    /// the platform for the resize the user is reaching for. Returns `true`
    /// when the drag was initiated, `false` when the backend cannot.
    fn drag_resize(&self, edge: ResizeEdge) -> bool {
        let _ = edge;
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

    /// The platform's native handle for this window as an integer, or `None` when this
    /// backend cannot or will not say.
    ///
    /// Native surfaces — the video presenter is the first — child into the host window at
    /// platform level, and the platform speaks in handles. The integer is the platform's own
    /// currency (`HWND` on Windows); what it means is the consumer's business, which is why
    /// backends on platforms no consumer yet understands answer `None` rather than a number
    /// that would mean the wrong thing. A backend that cannot answer says so, exactly as it
    /// does for geometry: a guessed handle would child a surface into a window that is not
    /// this one.
    fn native_window_handle(&self) -> Option<isize> {
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

    /// Attaches `menu` to this window, or removes it when `menu` is `None`.
    ///
    /// `false` when this backend has no native menu support or the platform refused the request.
    fn set_menu(&self, menu: Option<MenuRequest>) -> bool {
        let _ = menu;
        false
    }

    /// Takes the next native menu selection, if one has arrived.
    fn poll_menu_event(&self) -> Option<MenuEvent> {
        None
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

    /// The native-view half of this host, when the window's content is native (ADR-020).
    ///
    /// `None` is the honest answer from a page window: there is no native view, and a page
    /// host says so rather than offering a native half that never produces anything. A
    /// host whose window is native returns its other half here — one object implementing
    /// both contracts, with the web half honestly reporting that there is no page.
    fn native_view(&self) -> Option<&dyn NativeViewBackend> {
        None
    }

    /// The backend as [`Any`](core::any::Any), for a caller that knows a concrete engine
    /// type and needs a capability only it exposes (the CDP transport). A backend that has
    /// no such capability returns a value that downcasts to nothing.
    fn as_any(&self) -> &dyn core::any::Any;

    /// Sends one CDP command to this window's own page target (ADR-017 Phase 2).
    ///
    /// The default refuses with a typed, stable error code. Runtime policy can therefore
    /// distinguish an unsupported platform, a malformed command, and an engine failure
    /// without depending on a native backend or parsing a diagnostic string.
    fn cdp_send(&self, command: &serde_json::Value) -> Result<serde_json::Value, CdpError> {
        let _ = command;
        Err(CdpError::UnsupportedPlatform {
            reason: "this backend has no CDP transport".to_owned(),
        })
    }

    /// Builds one child webview inside the parent window's frame
    /// (ADR-017 Phase 3), returning the engine's identity for it.
    ///
    /// The default refuses: a backend without the embedding capability says so by
    /// name rather than answering a creation that never happened.
    fn create_child_webview(
        &self,
        request: &ncui_webview_child::ChildWebviewRequest,
        seam: &Arc<HostSeam>,
        ipc_tx: std::sync::mpsc::SyncSender<ChildPageMessage>,
    ) -> Result<u64, String> {
        let _ = (request, seam, ipc_tx);
        Err("this backend cannot embed child webviews".to_owned())
    }

    /// Takes the next message from a child page, if there is one.
    fn poll_child_webview_message(&self, child: u64) -> Option<ChildPageMessage> {
        let _ = child;
        None
    }

    /// Waits for the next message from a child page, up to `timeout`.
    fn wait_child_webview_message(
        &self,
        child: u64,
        timeout: std::time::Duration,
    ) -> Option<ChildPageMessage> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(message) = self.poll_child_webview_message(child) {
                return Some(message);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(1).min(remaining));
        }
    }

    /// Evaluates a script in a child page, returning whether it was accepted.
    fn evaluate_child_script(&self, child: u64, script: &str) -> bool {
        let _ = (child, script);
        false
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

    fn wait_page_message(&self, timeout: std::time::Duration) -> Option<PageMessage> {
        T::wait_page_message(self, timeout)
    }

    fn evaluate_script(&self, script: &str) -> bool {
        T::evaluate_script(self, script)
    }

    fn drag_window(&self) -> bool {
        T::drag_window(self)
    }

    fn drag_resize(&self, edge: ResizeEdge) -> bool {
        T::drag_resize(self, edge)
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

    fn set_menu(&self, menu: Option<MenuRequest>) -> bool {
        T::set_menu(self, menu)
    }

    fn poll_menu_event(&self) -> Option<MenuEvent> {
        T::poll_menu_event(self)
    }

    fn window_theme(&self) -> Option<Theme> {
        T::window_theme(self)
    }

    fn set_zoom(&self, factor: f64) -> bool {
        T::set_zoom(self, factor)
    }

    fn open_devtools(&self) -> bool {
        T::open_devtools(self)
    }

    fn close_devtools(&self) -> bool {
        T::close_devtools(self)
    }

    fn is_devtools_open(&self) -> Option<bool> {
        T::is_devtools_open(self)
    }

    fn native_view(&self) -> Option<&dyn NativeViewBackend> {
        T::native_view(self)
    }

    fn cdp_send(&self, command: &serde_json::Value) -> Result<serde_json::Value, CdpError> {
        T::cdp_send(self, command)
    }

    fn create_child_webview(
        &self,
        request: &ncui_webview_child::ChildWebviewRequest,
        seam: &Arc<HostSeam>,
        ipc_tx: std::sync::mpsc::SyncSender<ChildPageMessage>,
    ) -> Result<u64, String> {
        T::create_child_webview(self, request, seam, ipc_tx)
    }

    fn poll_child_webview_message(&self, child: u64) -> Option<ChildPageMessage> {
        T::poll_child_webview_message(self, child)
    }

    fn wait_child_webview_message(
        &self,
        child: u64,
        timeout: std::time::Duration,
    ) -> Option<ChildPageMessage> {
        T::wait_child_webview_message(self, child, timeout)
    }

    fn evaluate_child_script(&self, child: u64, script: &str) -> bool {
        T::evaluate_child_script(self, child, script)
    }

    fn set_child_webview_bounds(
        &self,
        child: u64,
        request: &ncui_webview_child::ChildWebviewRequest,
    ) -> Result<(), String> {
        T::set_child_webview_bounds(self, child, request)
    }

    fn close_child_webview(&self, child: u64) -> Result<(), String> {
        T::close_child_webview(self, child)
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;

    #[derive(Debug, Default)]
    struct ForwardingBackend {
        zoom: Mutex<Option<u64>>,
        devtools_open: Mutex<bool>,
        cdp_commands: Mutex<Vec<serde_json::Value>>,
        page_wait: Mutex<Option<Duration>>,
    }

    impl NativeViewBackend for ForwardingBackend {
        fn poll_native_event(&self) -> Option<NativeViewEvent> {
            None
        }

        fn send_view_command(&self, command: NativeViewCommand) -> Result<(), NativeViewError> {
            let _ = command;
            Ok(())
        }
    }

    impl WebViewBackend for ForwardingBackend {
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
            None
        }

        fn wait_page_message(&self, timeout: Duration) -> Option<PageMessage> {
            *self.page_wait.lock().expect("the wait recorder is usable") = Some(timeout);
            Some(PageMessage::new("blocking wait"))
        }

        fn evaluate_script(&self, _script: &str) -> bool {
            true
        }

        fn set_zoom(&self, factor: f64) -> bool {
            *self.zoom.lock().expect("the zoom recorder is usable") = Some(factor.to_bits());
            true
        }

        fn open_devtools(&self) -> bool {
            *self
                .devtools_open
                .lock()
                .expect("the inspector recorder is usable") = true;
            true
        }

        fn close_devtools(&self) -> bool {
            *self
                .devtools_open
                .lock()
                .expect("the inspector recorder is usable") = false;
            true
        }

        fn is_devtools_open(&self) -> Option<bool> {
            Some(
                *self
                    .devtools_open
                    .lock()
                    .expect("the inspector recorder is usable"),
            )
        }

        fn native_view(&self) -> Option<&dyn NativeViewBackend> {
            Some(self)
        }

        fn cdp_send(&self, command: &serde_json::Value) -> Result<serde_json::Value, CdpError> {
            self.cdp_commands
                .lock()
                .expect("the CDP recorder is usable")
                .push(command.clone());
            Ok(serde_json::json!({"forwarded": true}))
        }
    }

    #[test]
    fn a_boxed_backend_forwards_optional_window_capabilities() {
        let backend: Box<dyn WebViewBackend> = Box::new(ForwardingBackend::default());
        // The native half must forward too: method resolution prefers the box's own
        // impl over the object inside it, so a forward that is missing here is a method
        // the inner object never gets to answer — however it answered for itself.
        assert!(
            backend.native_view().is_some(),
            "native_view must forward through the box impl"
        );
        assert!(backend.set_zoom(1.25));
        assert!(backend.open_devtools());
        assert_eq!(backend.is_devtools_open(), Some(true));
        assert!(backend.close_devtools());
        assert_eq!(backend.is_devtools_open(), Some(false));

        let command = serde_json::json!({"id": 7, "method": "Runtime.evaluate"});
        assert_eq!(
            backend.cdp_send(&command),
            Ok(serde_json::json!({"forwarded": true}))
        );
        assert_eq!(
            backend.wait_page_message(Duration::from_millis(25)),
            Some(PageMessage::new("blocking wait"))
        );

        let recording = backend
            .as_any()
            .downcast_ref::<ForwardingBackend>()
            .expect("the boxed backend retains its concrete type");
        assert_eq!(
            *recording.zoom.lock().expect("the zoom recorder is usable"),
            Some(1.25_f64.to_bits())
        );
        assert!(
            !*recording
                .devtools_open
                .lock()
                .expect("the inspector recorder is usable")
        );
        assert_eq!(
            recording
                .cdp_commands
                .lock()
                .expect("the CDP recorder is usable")
                .as_slice(),
            &[command]
        );
        assert_eq!(
            *recording
                .page_wait
                .lock()
                .expect("the wait recorder is usable"),
            Some(Duration::from_millis(25))
        );
    }

    #[test]
    fn page_target_identifies_and_formats_windows_and_children() {
        let parent = WindowId::from_raw(0x1234).expect("a nonzero window id");
        let child = ncui_webview_child::ChildWebviewId::from_raw(7);
        let window = PageTarget::Window(parent);
        assert_eq!(window.parent(), parent);
        assert!(!window.is_child());
        assert_eq!(window.to_string(), parent.to_string());

        let target = PageTarget::Child { parent, child };
        assert_eq!(target.parent(), parent);
        assert!(target.is_child());
        assert_eq!(target.to_string(), format!("child:{parent}:{child}"));
    }
}
