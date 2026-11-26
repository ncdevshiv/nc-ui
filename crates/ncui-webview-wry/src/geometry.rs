//! Engine window construction options and logical geometry.

#![cfg(feature = "wry-engine")]

/// The properties a window is created with, gathered so the builder reads in one place.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct WindowOptions {
    /// Whether the window floats above other windows.
    pub(super) always_on_top: bool,
    /// Whether the window carries its own frame.
    pub(super) decorations: bool,
    /// Whether the window's background may be see-through.
    pub(super) transparent: bool,
    /// Whether the user may resize the window.
    pub(super) resizable: bool,
    /// The floor the user may drag the window to.
    pub(super) minimum: Option<(u32, u32)>,
    /// Whether the window is created filling its display.
    pub(super) fullscreen: bool,
    /// Whether the window is created maximised.
    pub(super) maximized: bool,
}

/// A window's geometry, in logical pixels.
///
/// Logical rather than physical because that is what a caller stores and asks for: a remembered
/// geometry in physical pixels would be wrong by the scale factor on every run after the display
/// changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// The left edge.
    pub x: i32,
    /// The top edge.
    pub y: i32,
    /// The width.
    pub width: u32,
    /// The height.
    pub height: u32,
}
