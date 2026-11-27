//! Doc-only stand-ins for the engine-off configuration.
//!
//! The `gpui-engine` feature is off by default so the workspace compiles on a machine with
//! no display. Intra-doc links in the engine-off build still need the names to resolve, so
//! this module exports the shape of the API without the engine behind it — the same
//! pattern the wry host uses for its own off configuration.

/// The factory that opens native windows, present only when built with `gpui-engine`.
#[derive(Debug)]
pub struct GpuiHostFactory;
