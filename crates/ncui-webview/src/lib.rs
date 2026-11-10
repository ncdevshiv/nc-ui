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
}

/// Lifts an owned backend into a boxed one, so the registry can hold heterogeneous engines.
///
/// `?Sized` is what makes `Box<dyn WebViewBackend>` itself a backend: without it a registry
/// storing `Box<dyn WebViewBackend>` could not store that box again in another box, and every
/// call site would have to reach through the box by hand.
impl<T: WebViewBackend + ?Sized> WebViewBackend for Box<T> {
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
