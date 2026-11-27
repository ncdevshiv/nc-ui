//! The GPUI native window host: the platform binding that opens real native windows.
//!
//! ADR-001 splits the system in two for the web engine, and ADR-020 applies the same split
//! to native views: the policy — which windows may exist, what their content is, what a
//! view may ask the runtime — lives in `ncui-webview`, and the platform binding lives here.
//! `gpui` owns Win32 windowing, DirectX rendering and DirectWrite text; this crate owns the
//! engine thread, the command channel into it, and the translation between the runtime's
//! window operations and gpui's window handles.
//!
//! # What lives here, and what deliberately does not
//!
//! The engine thread and the host objects that talk to it live here. The decision of
//! *whether* a window may open — capacity, validation, capability narrowing — does not:
//! that is the registry's, in `ncui-webview`, and this crate is handed windows only after
//! the registry has already said yes.
//!
//! # The threading contract
//!
//! gpui's Windows platform captures the constructing thread as its main thread and pumps a
//! `GetMessageW` loop on it. This crate therefore owns one dedicated thread, named
//! `ncui-gpui-loop`, on which the `gpui::Application` is both constructed and run. Every
//! operation a host performs — opening a window, resizing it, reading its geometry back —
//! is a command sent into that thread with a reply channel, the same shape the wry host
//! uses for its own engine. No gpui object is ever touched off the engine thread.

#![deny(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

pub mod error;

#[cfg(feature = "gpui-engine")]
pub mod engine;
#[cfg(feature = "gpui-engine")]
pub mod host;
#[cfg(feature = "gpui-engine")]
mod ops;
#[cfg(feature = "gpui-engine")]
pub mod provider;

#[cfg(not(feature = "gpui-engine"))]
pub mod placeholder_types;

pub use error::GpuiHostError;

#[cfg(feature = "gpui-engine")]
pub use engine::shutdown;
#[cfg(feature = "gpui-engine")]
pub use host::{GpuiHostFactory, GpuiWindowHost};
#[cfg(feature = "gpui-engine")]
pub use provider::{
    NativeEventSink, NativeViewContext, NativeViewInstance, NativeViewProvider, ProviderRegistry,
    providers,
};

#[cfg(not(feature = "gpui-engine"))]
pub use placeholder_types::GpuiHostFactory;
