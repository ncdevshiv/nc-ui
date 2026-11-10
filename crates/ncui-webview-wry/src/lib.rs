//! The `wry` binding for the `WebView` host.
//!
//! This crate is the *only* place in NC-UI that depends on `wry` and `tao`. It implements
//! the `WebViewBackend` trait defined in `ncui-webview` and provides the `WindowHost`
//! that drives a real window, loads a page, evaluates JavaScript, and bridges messages
//! between the page and the runtime.
//!
//! The separation is deliberate (ADR-012): `ncui-webview` stays pure policy, testable
//! anywhere, while this crate pays the cost of the `wry`/`tao` dependency tree
//! (`webkit2gtk`, `gtk`, `soup3` on Linux).
//!
//! # What lives here
//!
//! * [`WindowHost`] - one window's engine: liveness, script evaluation, and the page's
//!   messages, all governed by the window's seam.
//! * [`WryHostFactory`] - the adapter from NC-UI's host port to `wry`, so the registry can
//!   create a window per request.
//!
//! # The engine decides nothing
//!
//! A navigation is offered to the window's seam and the engine obeys the seam's verdict. A
//! message from the page is forwarded verbatim and evaluated by the runtime through that
//! same seam. The engine holds no copy of the rules, so the rules it obeys and the rules
//! the runtime enforces are the same rules.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

pub mod host;
mod host_scripts;

pub use host::{WindowHost, WryHostFactory};
