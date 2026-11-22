//! The wry engine binding for embedded child webviews (ADR-017 Phase 3).
//!
//! The parent window is a real `tao` window the engine already owns; the child
//! is built with wry's `build_as_child`, which on Windows composes a child HWND
//! inside the parent's frame, and moved with wry's `set_bounds`. The engine
//! thread owns both, so every operation is a `HostEvent` the loop answers.

#![cfg(feature = "wry-engine")]

use std::sync::mpsc;
use std::time::Duration;

pub use ncui_webview_child::ChildWebviewError;
use ncui_webview_child::{ChildWebviewEngine, ChildWebviewRequest};

use crate::host::HostEvent;

/// How long an engine request for a child waits before the host reports the
/// engine as not answering. The same bound the geometry and presentation
/// requests use: an engine that has stopped answering is reported rather than
/// blocking the caller forever.
const CHILD_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Builds children and drives them, on the engine's own thread.
///
/// One binding per process, shared with the window host: children are built
/// through the same event loop the parent windows use, so they are created and
/// moved on the thread that owns the platform.
#[derive(Debug)]
pub(crate) struct WryChildWebviewEngine {
    proxy: tao::event_loop::EventLoopProxy<HostEvent>,
}

impl WryChildWebviewEngine {
    /// Creates the binding over an engine proxy.
    #[must_use]
    pub(crate) fn new(proxy: tao::event_loop::EventLoopProxy<HostEvent>) -> Self {
        Self { proxy }
    }
}

impl ChildWebviewEngine for WryChildWebviewEngine {
    fn create_child(
        &self,
        parent: ncui_core::WindowId,
        request: &ChildWebviewRequest,
    ) -> Result<u64, ChildWebviewError> {
        let (reply, answers) = mpsc::channel();
        self.proxy
            .send_event(HostEvent::CreateChild {
                parent: parent.to_string(),
                request: Box::new(request.clone()),
                reply,
            })
            .map_err(|error| ChildWebviewError::NotSupported {
                reason: format!("the engine loop is not accepting work: {error}"),
            })?;
        answers
            .recv_timeout(CHILD_REQUEST_TIMEOUT)
            .map_err(|error| ChildWebviewError::EngineFailed {
                reason: format!("the engine stopped answering: {error}"),
            })?
    }

    fn set_child_bounds(
        &self,
        _parent: ncui_core::WindowId,
        child: u64,
        request: &ChildWebviewRequest,
    ) -> Result<(), ChildWebviewError> {
        let (reply, answers) = mpsc::channel();
        self.proxy
            .send_event(HostEvent::ChildBounds {
                child,
                request: Box::new(request.clone()),
                reply,
            })
            .map_err(|error| ChildWebviewError::NotSupported {
                reason: format!("the engine loop is not accepting work: {error}"),
            })?;
        answers
            .recv_timeout(CHILD_REQUEST_TIMEOUT)
            .map_err(|error| ChildWebviewError::EngineFailed {
                reason: format!("the engine stopped answering: {error}"),
            })?
    }

    fn close_child(
        &self,
        _parent: ncui_core::WindowId,
        child: u64,
    ) -> Result<(), ChildWebviewError> {
        let (reply, answers) = mpsc::channel();
        self.proxy
            .send_event(HostEvent::CloseChild { child, reply })
            .map_err(|error| ChildWebviewError::NotSupported {
                reason: format!("the engine loop is not accepting work: {error}"),
            })?;
        answers
            .recv_timeout(CHILD_REQUEST_TIMEOUT)
            .map_err(|error| ChildWebviewError::EngineFailed {
                reason: format!("the engine stopped answering: {error}"),
            })?
    }
}

/// The bounds a child request names, in the form wry's `set_bounds` takes.
///
/// The request's rectangle is logical pixels relative to the parent's client
/// area, which is exactly what a child HWND position is.
#[must_use]
pub fn child_bounds(request: &ChildWebviewRequest) -> wry::Rect {
    wry::Rect {
        position: wry::dpi::LogicalPosition::new(f64::from(request.x), f64::from(request.y)).into(),
        size: wry::dpi::LogicalSize::new(f64::from(request.width), f64::from(request.height))
            .into(),
    }
}
