//! CDP command forwarding handler.

use ncui_core::{ErrorKind, NcError, Result};
use serde_json::{Value, json};

use super::json_error;
use crate::Runtime;

/// `ncui.cdp.send` -- deliver a CDP command to a window's own `DevTools` endpoint.
///
/// The window's engine was created with remote debugging enabled (ADR-017 Phase 2), and
/// this asks the engine to carry the command to the endpoint it opened. A page that also
/// wants to observe the traffic keeps its `ncui.on("ncui:cdp", ...)` relay; the transport
/// and the relay are independent paths to the same commands.
///
/// A window that is not open, an engine without the transport, or an endpoint that refuses
/// the command is an error naming what happened, never a `sent: true` that nothing
/// received.
pub(crate) fn invoke_cdp_send(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let window_id = params
        .get("window")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            NcError::new(ErrorKind::Validation {
                field: "window".to_owned(),
                reason: "`window` is required: name the window whose page the command targets"
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

    let answer = runtime
        .windows
        .cdp_send(parsed_id, &cdp_command)
        .map_err(|_| {
            NcError::new(ErrorKind::NotFound {
                kind: "window".to_owned(),
                id: window_id.to_owned(),
            })
            .operation("dispatch::cdp_send")
        })?
        .map_err(|reason| {
            NcError::new(ErrorKind::Conflict { reason }).operation("dispatch::cdp_send")
        })?;

    serde_json::to_value(json!({
        "sent": true,
        "method": method,
        "window": window_id,
        "result": answer.get("result").cloned().unwrap_or(Value::Null),
    }))
    .map_err(|e| json_error(&e))
}
