//! Parent-owned embedded child webviews (ADR-017 Phase 3).
//!
//! Child surfaces are built and driven by the per-parent [`crate::WindowHost`] on the engine's
//! event-loop thread. This module owns their engine state and translates logical rectangles into
//! the form wry accepts.

#![cfg(feature = "wry-engine")]

use std::sync::Arc;
use std::sync::mpsc::{SyncSender, TrySendError};

use ncui_webview::{ChildPageMessage, HostSeam};
use ncui_webview_child::{ChildWebviewError, ChildWebviewRequest};
use tao::window::Window;
use wry::{WebView, WebViewBuilder};

/// The bounds a child request names, in the form wry's `set_bounds` takes.
fn child_bounds(request: &ChildWebviewRequest) -> wry::Rect {
    wry::Rect {
        position: wry::dpi::LogicalPosition::new(f64::from(request.x), f64::from(request.y)).into(),
        size: wry::dpi::LogicalSize::new(f64::from(request.width), f64::from(request.height))
            .into(),
    }
}

/// One live child webview inside a parent window's frame.
pub(super) struct EngineChild {
    /// The window whose frame owns this surface.
    pub(super) parent: u64,
    /// The wry webview, built as a child of the parent's frame.
    pub(super) webview: WebView,
    /// GTK owns the child's position on Linux, where wry's raw-handle child API is X11-only.
    #[cfg(target_os = "linux")]
    container: gtk::Fixed,
}

impl EngineChild {
    pub(super) fn set_bounds(
        &mut self,
        request: &ChildWebviewRequest,
    ) -> Result<(), ChildWebviewError> {
        let bounds = child_bounds(request);
        #[cfg(target_os = "linux")]
        {
            use gtk::prelude::*;
            use wry::WebViewExtUnix as _;

            let widget = self.webview.webview();
            let scale = f64::from(widget.scale_factor());
            let (width, height) = bounds.size.to_logical::<i32>(scale).into();
            let (x, y) = bounds.position.to_logical::<i32>(scale).into();
            widget.set_size_request(width, height);
            self.container.move_(&widget, x, y);
        }
        self.webview
            .set_bounds(bounds)
            .map_err(|error| ChildWebviewError::EngineFailed {
                reason: error.to_string(),
            })
    }
}

pub(super) fn build_child(
    id: u64,
    parent: u64,
    window: &Window,
    request: &ChildWebviewRequest,
    seam: &Arc<HostSeam>,
    ipc_tx: SyncSender<ChildPageMessage>,
    host_ipc_tx: SyncSender<ChildPageMessage>,
) -> Result<EngineChild, String> {
    #[cfg(test)]
    std::thread::sleep(std::time::Duration::from_millis(25));

    // Creation is the one navigation the caller has already vetted. Every later navigation is
    // offered to the child's isolated seam, which carries the parent's policy but none of its
    // authority.
    let initial = request.url.clone();
    let navigation_seam = Arc::clone(seam);
    let navigation_handler = move |target: String| -> bool {
        if target == initial {
            return true;
        }
        let Some(navigation) = ncui_webview::NavigationRequest::from_url(&target, true) else {
            return false;
        };
        navigation_seam
            .evaluate_navigation(&navigation)
            .is_allowed()
    };
    let builder = WebViewBuilder::new()
        .with_url(request.url.clone())
        .with_bounds(child_bounds(request))
        .with_navigation_handler(navigation_handler)
        .with_ipc_handler(move |request: wry::http::Request<String>| {
            let message = ChildPageMessage::new(id, request.body().to_owned());
            match ipc_tx.try_send(message) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(message)) => {
                    let _ = host_ipc_tx.try_send(message);
                }
            }
        })
        .with_initialization_script(crate::host::BRIDGE_SCRIPT);
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        use tao::platform::unix::WindowExtUnix as _;
        use wry::WebViewBuilderExtUnix as _;

        let vbox = window
            .default_vbox()
            .ok_or_else(|| "the parent window has no visual box for a child webview".to_owned())?;
        let container = gtk::Fixed::new();
        vbox.pack_start(&container, true, true, 0);
        container.show_all();
        let webview = builder.build_gtk(&container).map_err(|error| {
            vbox.remove(&container);
            error.to_string()
        })?;
        Ok(EngineChild {
            parent,
            webview,
            container,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let webview = builder
            .build_as_child(window)
            .map_err(|error| error.to_string())?;
        Ok(EngineChild { parent, webview })
    }
}
