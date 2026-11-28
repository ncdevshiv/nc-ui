//! The platform video presenter: a child window the OS composites, driven by `wgpu`.
//!
//! This crate is the platform body of [`ncui_gpu::surface`]'s contract and the one
//! place in the workspace whose job is raw window-system FFI. The architecture:
//!
//! - **A child window, not a webview object.** On Windows the surface is a child
//!   `HWND` of the host window, positioned over the rect the placement machinery
//!   computes. The OS compositor layers it above the webview's own content — the
//!   airspace technique — so pixels never enter the browser process at all.
//! - **`WS_EX_TRANSPARENT`, because the surface is a monitor, not a control.**
//!   Input falls through to the page beneath; a surface that ate clicks would be
//!   a control pretending to be a display.
//! - **Presentation is `wgpu`, pacing is the producer's.** Frames upload as RGBA
//!   and draw as a textured fullscreen triangle — the GPU's only job is the copy.
//!   The presenter presents when a frame arrives rather than polling a clock:
//!   the engine owns time, and a presenter with its own clock is a second opinion.
//!
//! # The unsafe boundary, stated
//!
//! `unsafe` exists in this crate for exactly the Win32 calls that create, place
//! and destroy the window, plus the window procedure. Everything else — the GPU
//! stack included — is safe Rust: the surface is created through `wgpu`'s safe
//! `create_surface` with a handle wrapper whose validity the struct's own drop
//! order guarantees (the window is declared first and destroyed last). Each
//! unsafe block carries its invariant at the point of use; there is no
//! crate-level allowance for more.

#[cfg(target_os = "windows")]
pub mod evidence;
#[cfg(target_os = "windows")]
pub mod win32;

#[cfg(target_os = "windows")]
pub use win32::{PresentError, RgbaPixels, Win32Presenter};
