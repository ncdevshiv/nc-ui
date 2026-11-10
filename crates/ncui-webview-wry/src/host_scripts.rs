//! The scripts the engine evaluates to tell a page about its window.
//!
//! Split out of [`crate::host`] so the binding stays focused on the window and the event
//! loop. Each function is the host-to-page half of one window event: the engine asks the
//! platform what changed, and the page hears about it through the bridge rather than by
//! polling.

/// What a page hears when its window is resized, in logical pixels.
///
/// Told rather than left to poll: a layout built for the old size is a layout the user has already
/// left, and a page that had to ask would not be able to lay out while the user is still dragging.
#[cfg(feature = "wry-engine")]
pub(super) fn resized_script(size: tao::dpi::LogicalSize<f64>) -> String {
    format!(
        "window.ncui && window.ncui.__dispatch({{\"event\":\"resized\",\"payload\":{{\"width\":{},\"height\":{}}}}});",
        size.width, size.height
    )
}

/// What a page hears when its window is moved, in logical pixels.
///
/// The counterpart of the resize: a page that keeps something anchored to the window's place — a
/// toolbar that follows it, a remembered position — needs to know where it went without asking.
#[cfg(feature = "wry-engine")]
pub(super) fn moved_script(position: tao::dpi::LogicalPosition<f64>) -> String {
    format!(
        "window.ncui && window.ncui.__dispatch({{\"event\":\"moved\",\"payload\":{{\"x\":{},\"y\":{}}}}});",
        position.x, position.y
    )
}
