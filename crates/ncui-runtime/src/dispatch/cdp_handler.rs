//! CDP command forwarding handler.

use ncui_core::{ErrorKind, NcError, Result};
use serde_json::{Value, json};

use crate::Runtime;
use super::json_error;

/// `ncui.cdp.send` -- send a CDP command to a child webview.
pub(crate) fn invoke_cdp_send(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let window_id = params
        .get("window")
        .and_then(Value::as_str)
        .unwrap_or("00000000000000000000000000000001");

    let method = params
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("Page.navigate");

    let cdp_params = params.get("params").cloned().unwrap_or(Value::Null);

    let cdp_command = json!({
        "id": 1,
        "method": method,
        "params": cdp_params,
    });

    let script = format!(
        "window.ncui && window.ncui.__cdpSend && window.ncui.__cdpSend({});",
        serde_json::to_string(&cdp_command).unwrap_or_default()
    );

    let parsed_id = window_id
        .parse()
        .map_err(|_| NcError::new(ErrorKind::Validation {
            field: "window".to_owned(),
            reason: format!("invalid window id: {window_id}"),
        })
        .operation("dispatch::cdp_send"))?;

    let _ = runtime.windows.post_to_page(parsed_id, &script);

    serde_json::to_value(json!({
        "sent": true,
        "method": method,
        "window": window_id,
    }))
    .map_err(|e| json_error(&e))
}
