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
//! implements [`WebViewBackend`] and is *not* built by default. That is a deliberate
//! dependency decision (ADR-012): an embedding layer plus its window and COM trees is a very
//! large addition to a runtime whose security argument rests on a small dependency set, and
//! it cannot be verified without a window host. See `docs/known-gaps.md`.
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
pub mod seam;
pub mod windows;

pub use capability::{CapabilityGrant, CapabilityRequest};
pub use lifecycle::{HostState, IllegalTransition, WindowLifecycle};
pub use navigation::{NavigationPolicy, NavigationRequest, Origin, Scheme, Verdict};
pub use seam::{HostSeam, SeamMessage, SeamResponse};
pub use windows::{
    HostFactory, MAX_WINDOWS, MountedComponent, Window, WindowRegistry, WindowRejection,
    WindowSummary,
};

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
}
