//! Trusted runtime services for native views.
//!
//! A native view is host-linked Rust, so it does not cross the page bridge. It still asks
//! through a narrow seam rather than reaching into the runtime: the seam carries the
//! application command table and the window's narrowed grant, and every call is answered
//! or refused in the same [`ncui_webview::SeamError`] shape a page caller handles.

use std::fmt;
use std::sync::Arc;

use ncui_core::{CapabilitySet, WindowId};
use ncui_observability::metrics::{Labels, MetricsRegistry};
use ncui_webview::{SeamError, TrustedServiceSeam, TrustedServiceSeamFactory};
use serde_json::Value;

use crate::commands::PageCommands;

/// One native window's service channel.
///
/// The window id is carried for diagnostics, never for lookup: the authority is already
/// closed over in `grant`, so a call cannot become a search for a wider grant.
struct NativeViewServices {
    window: WindowId,
    grant: CapabilitySet,
    commands: PageCommands,
    metrics: Arc<MetricsRegistry>,
}

impl fmt::Debug for NativeViewServices {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeViewServices")
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl TrustedServiceSeam for NativeViewServices {
    fn invoke(&self, service: &str, input: Value) -> Result<Value, SeamError> {
        let answer = self.commands.native_view_call(service, self.grant, input);
        let outcome = if answer.is_ok() { "served" } else { "refused" };
        let labels = Labels::from_pairs([("op", outcome)]).unwrap_or_else(|_| Labels::new());
        let _ = self
            .metrics
            .counter_inc("native_view.service_calls", &labels, 1);
        if let Err(error) = &answer {
            tracing::info!(
                window = %self.window,
                service,
                code = error.code.as_str(),
                "a native view's service call was refused"
            );
        }
        answer
    }
}

/// Builds the factory the window registry installs.
///
/// `PageCommands` is shared, not copied: commands registered after the factory is built
/// are visible to windows born afterwards, while each window's grant remains fixed at
/// birth. That pairing is deliberate — the command table may grow, authority may not.
pub(crate) fn factory(
    commands: PageCommands,
    metrics: Arc<MetricsRegistry>,
) -> TrustedServiceSeamFactory {
    Arc::new(move |window, grant| {
        Arc::new(NativeViewServices {
            window,
            grant,
            commands: commands.clone(),
            metrics: Arc::clone(&metrics),
        }) as Arc<dyn TrustedServiceSeam>
    })
}
