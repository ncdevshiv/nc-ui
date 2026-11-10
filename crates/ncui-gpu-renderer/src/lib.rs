//! wgpu-backed GPU renderer for GPU islands.
//!
//! ADR-002: *"GPU Islands must support the full input contract"* — this crate owns the
//! rendering half: the GPU device, the queue, the surface, the render pass, and the
//! device-loss recovery path. The other half — which island a pointer landed on, what a
//! key press means, where the focus ring goes — is `ncui-gpu`'s, and the two meet at the
//! [`frame::Frame`] this crate draws.
//!
//! # What is here, and what is behind a feature
//!
//! * [`frame`] — the typed binary representation ADR-002 requires for the hot path. It is
//!   pure: it compiles and tests on any machine, and it is the format a frame travels in.
//! * [`render`] — the render pass, split into a pure [`render::RenderPlan`] and the
//!   `wgpu` binding that draws it.
//! * [`surface`] — where a frame is drawn: a window or an offscreen target.
//! * [`device`] — device ownership, and the recovery path when the device is lost.
//! * [`glyphs`] — rasterising a glyph's outline into per-pixel coverage. Pure, like the
//!   frame and the plan: a letter's shape is arithmetic, and arithmetic can be tested on a
//!   machine with no display.
//! * `text` — the text layout path, integrating `HarfBuzz` through the `rustybuzz` crate.
//! * [`atlas`] — a cache of rasterised glyphs, so a frame that repeats a line does not
//!   rasterise it again.
//!
//! The `wgpu` binding is behind the `wgpu-device` feature, so the crate — and the whole
//! workspace — compiles on a machine with no graphics stack. ADR-012 records `wgpu` as a
//! deliberate dependency exception, confined to this crate. The text stack is behind
//! `text-shaping`, for the same reason: the layout policy is pure and testable without a
//! font stack, and a machine with no fonts should still compile the workspace.
//!
//! # The safety boundary
//!
//! `wgpu` performs all graphics FFI internally. This crate's own code is `unsafe`-free:
//! it owns the `wgpu` device, queue and surface and exposes only a safe abstraction.
//! ADR-012 records this as a deliberate dependency exception.

pub mod device;
pub mod frame;
pub mod glyphs;
pub mod island;
pub mod render;
pub mod surface;

#[cfg(feature = "text-shaping")]
pub mod atlas;
#[cfg(feature = "text-shaping")]
pub mod text;

pub use device::{DeviceError, DeviceSupervisor, GpuDevice};
pub use frame::{Frame, FrameError, IslandRect};
#[cfg(feature = "wgpu-device")]
pub use island::{IslandError, IslandPixels, render_to_pixels};
pub use render::{RenderError, RenderPlan};
pub use surface::{Surface, SurfaceKind, SurfaceSize};

#[cfg(feature = "wgpu-device")]
pub use device::{DeviceFactory, WgpuDevice, WgpuDeviceFactory};
