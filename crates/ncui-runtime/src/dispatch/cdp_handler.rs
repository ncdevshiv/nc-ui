//! CDP command forwarding handler.

use ncui_core::{ErrorKind, NcError, Result};
use serde_json::{Value, json};

use crate::Runtime;
use super::json_error;

/// `ncui.cdp.send` -- hand a CDP command to a window's page relay.
///
/// The command is delivered to the target window's page as a
/// `window.ncui.__cdpSend(command)` call. The bridge defines `__cdpSend` on
/// every page, so a delivery reported here really ran: the page's relay (an
/// `ncui.on("ncui:cdp", ...)` listener, or the `window.__ncuiCdpLog` ring)
/// receives the command and is responsible for the exchange with the debugging
/// endpoint it holds. The host-side CDP transport itself is ADR-017 Phase 2.
///
/// A window that is not open, or an engine that refuses the script, is an
/// error rather than a `sent: true` that nothing received.
pub(crate) fn invoke_cdp_send(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let window_id = params
        .get("window")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            NcError::new(ErrorKind::Validation {
                field: "window".to_owned(),
                reason: "`window` is required: name the window whose page relays the command"
                    .to_owned(),
            })
            .operation("dispatch::cdp_send")
        })?;

    let method = params
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty() && method.len() <= 128)
        .ok_or_else(|| {
            NcError::new(ErrorKind::Validation {
                field: "method".to_owned(),
                reason: "`method` must be a non-empty CDP method name of at most 128 bytes"
                    .to_owned(),
            })
            .operation("dispatch::cdp_send")
        })?;

    let parsed_id = window_id.parse().map_err(|_| {
        NcError::new(ErrorKind::Validation {
            field: "window".to_owned(),
            reason: format!("invalid window id: {window_id}"),
        })
        .operation("dispatch::cdp_send")
    })?;

    let cdp_params = params.get("params").cloned().unwrap_or(Value::Null);
    let cdp_command = json!({
        "id": 1,
        "method": method,
        "params": cdp_params,
    });

    let script = format!(
        "window.ncui && window.ncui.__cdpSend && window.ncui.__cdpSend({});",
        serde_json::to_string(&cdp_command).map_err(|e| json_error(&e))?
    );

    let delivered = runtime
        .windows
        .post_to_page(parsed_id, &script)
        .map_err(|_| {
            NcError::new(ErrorKind::NotFound {
                kind: "window".to_owned(),
                id: window_id.to_owned(),
            })
            .operation("dispatch::cdp_send")
        })?;

    if delivered {
        serde_json::to_value(json!({
            "sent": true,
            "method": method,
            "window": window_id,
        }))
        .map_err(|e| json_error(&e))
    } else {
        Err(NcError::new(ErrorKind::Conflict {
            reason: format!("the engine refused the CDP script for window {window_id}"),
        })
        .operation("dispatch::cdp_send"))
    }
}
