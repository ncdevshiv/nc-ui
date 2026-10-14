//! The `wry` binding for the `WebView` host.
//!
//! This crate is the *only* place in NC-UI that depends on `wry` and `tao`. It implements
//! the `WebViewBackend` trait defined in `ncui-webview` and provides the `WindowHost`
//! that drives a real window, loads an empty `about:blank` page by default, evaluates
//! JavaScript, and bridges messages between the page and the runtime.
//!
//! The separation is deliberate (ADR-012): `ncui-webview` stays pure policy, testable
//! anywhere, while this crate pays the cost of the `wry`/`tao` dependency tree
//! (`webkit2gtk`, `gtk`, `soup3` on Linux).
//!
//! # What lives here
//!
//! * [`WindowHost`] - drives the lifecycle, evaluates scripts, and reports liveness.
//!
//! The IPC/auth pipeline is not wired through dispatch yet: the wry IPC handler records
//! the request body only.

pub mod host;

pub use host::WindowHost;
