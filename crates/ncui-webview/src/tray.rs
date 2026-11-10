//! The tray icon: what an application shows when none of its windows are visible.
//!
//! A window is how a user works with an application; the tray is how they know it is still there.
//! It is the only part of an application that is visible when every window is closed or hidden,
//! and the only way back to one that has been minimised out of sight — which is why an application
//! that runs in the background needs one, and why the platform owns it rather than the page.
//!
//! # What travels through the port
//!
//! The decoded image and the words to show beside it. Not a path, and not the bytes: the image is
//! decoded before it gets here, so a host never reads a file and the same request means the same
//! pixels on every platform. [`crate::Icon`] is the same value a window's frame carries, because a
//! platform's tray and its window title bar want the same thing.

use serde::{Deserialize, Serialize};

use crate::request::Icon;

/// The longest tooltip a tray icon may carry.
///
/// A tooltip is a label, not a sentence: a platform truncates what does not fit, and a limit here
/// means what the application asked for is what the platform shows.
pub const MAX_TRAY_TOOLTIP: usize = 128;

/// The image an application shows in the tray, and the words beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrayRequest {
    /// The image the tray shows.
    pub icon: Icon,
    /// The words the platform shows when the pointer rests on the tray icon.
    pub tooltip: String,
}

impl TrayRequest {
    /// A tray request from an image and a tooltip.
    #[must_use]
    pub fn new(icon: Icon, tooltip: impl Into<String>) -> Self {
        Self {
            icon,
            tooltip: tooltip.into(),
        }
    }

    /// Whether this is a tray icon a platform can be asked for.
    ///
    /// The icon must be a whole image — the platform would read past the end of a short buffer —
    /// and the tooltip must be something a label can be: not empty, and not longer than a platform
    /// will show.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.icon.is_whole()
            && !self.tooltip.trim().is_empty()
            && self.tooltip.chars().count() <= MAX_TRAY_TOOLTIP
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    fn icon() -> Icon {
        Icon {
            width: 2,
            height: 2,
            rgba: vec![255; 16],
        }
    }

    #[test]
    fn a_tray_request_is_usable_only_when_a_platform_could_show_it() {
        let good = TrayRequest::new(icon(), "Counter");
        assert!(good.is_usable());

        // An image that is not whole is an image the platform would read past the end of.
        let short = TrayRequest::new(
            Icon {
                width: 4,
                height: 4,
                rgba: vec![0; 16],
            },
            "Counter",
        );
        assert!(!short.is_usable(), "a short buffer is not an image");

        // A tooltip is a label: an empty one says nothing, and one longer than a platform shows
        // would be silently truncated.
        assert!(!TrayRequest::new(icon(), "   ").is_usable());
        assert!(!TrayRequest::new(icon(), "x".repeat(MAX_TRAY_TOOLTIP + 1)).is_usable());
        assert!(TrayRequest::new(icon(), "x".repeat(MAX_TRAY_TOOLTIP)).is_usable());
    }
}
