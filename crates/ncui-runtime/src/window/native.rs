//! The native half of the window surface.
//!
//! A native window carries a view instead of a page, and the two questions that view
//! answers — what did it emit, and what may it be told — are the native half of the
//! surface. They live apart from the page half because their caller is different: the
//! page half serves the document inside the window, and this half serves the agent
//! outside it, through `ncui.window.native.events` and `ncui.window.native.command`.
//!
//! Everything here happens inside the registry lock, because the host is owned there:
//! the events and the commands cross through the lock rather than the host leaving it,
//! which no borrow could honestly do.

use ncui_core::WindowId;
use ncui_webview::WindowRejection;

use super::Windows;

impl Windows {
    /// Drains every native-view event `window` has queued, with the count dropped since
    /// the last drain.
    ///
    /// The shape mirrors [`Windows::send_native_command`]'s: the outer refusal is a
    /// window that is not open, `Ok(None)` is an open window without a native view, and
    /// `Some` is the queue's whole contents. Draining rather than polling one is the
    /// honest shape of the boundary: the queue is bounded and the view does not pause
    /// while an agent asks, so a caller owed "everything since last time" cannot
    /// reconstruct it one event at a time without racing the drops it is trying to count.
    ///
    /// The dropped count is the delta of the backend's cumulative counter against what
    /// the last drain reported (see the field on [`Windows`]), because "how many were
    /// lost since the window opened" is a question a metrics counter answers better than
    /// a caller reconstructing arithmetic, and one loss must be counted once.
    pub fn drain_native_events(
        &self,
        window: WindowId,
    ) -> Result<Option<(Vec<ncui_webview::NativeViewEvent>, u64)>, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry = registry
                .get(window)
                .ok_or_else(|| WindowRejection::EngineUnavailable {
                    reason: Some(format!("window {window} is not open")),
                })?;
            let Some(native) = entry.host.as_ref().and_then(|host| host.native_view()) else {
                return Ok(None);
            };
            let mut events = Vec::new();
            while let Some(event) = native.poll_native_event() {
                events.push(event);
            }
            // The counter is read after the queue is empty, inside the same lock: a drop
            // that lands between the two reads is still accounted to this drain rather
            // than falling between two of them.
            let cumulative = native.dropped_native_events();
            let mut reported = self
                .native_drops_reported
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let last = reported.insert(window, cumulative).unwrap_or(0);
            // Saturating rather than wrapping: a backend whose counter resets is a
            // backend with nothing honest to subtract, and zero is the only count that
            // means nothing was lost.
            let dropped = cumulative.saturating_sub(last);
            Ok(Some((events, dropped)))
        })
    }

    /// Delivers one command to the native view `window` carries.
    ///
    /// The nested shape keeps the three answers apart, exactly as
    /// [`Windows::drain_native_events`] does: the outer refusal is a window that is not
    /// open, `Ok(None)` is an open window without a native view, and the innermost
    /// answer is the view's own — a command it accepted or the typed refusal it
    /// answered with, carried whole so dispatch can keep its stable code rather than
    /// classify a string.
    pub fn send_native_command(
        &self,
        window: WindowId,
        command: ncui_webview::NativeViewCommand,
    ) -> Result<Option<Result<(), ncui_webview::NativeViewError>>, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry = registry
                .get(window)
                .ok_or_else(|| WindowRejection::EngineUnavailable {
                    reason: Some(format!("window {window} is not open")),
                })?;
            let Some(native) = entry.host.as_ref().and_then(|host| host.native_view()) else {
                return Ok(None);
            };
            Ok(Some(native.send_view_command(command)))
        })
    }
}
