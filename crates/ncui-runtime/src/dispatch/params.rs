//! Parameter parsing and log-query handling for the dispatch surface.
//!
//! Every argument a caller sends is untrusted input, so each one is read by a
//! dedicated function that answers "what did the caller actually ask for?" with a
//! typed value or a refusal naming the field at fault. These helpers are pure: they
//! read a [`Value`] and return a value or an error, with the single exception of
//! [`invoke_logs_query`], which reads the runtime's bounded log ring.
//!
//! They live apart from the pipeline in `dispatch` so the pipeline reads as the
//! security property it is — rate limit, authorize, dispatch, audit — rather than as
//! several hundred lines of field validation. The parameter readers are pure; the
//! `invoke_*` handlers that answer a call carry it to the runtime's window registry.

use core::time::Duration;
use std::time::Instant;

use ncui_core::error::validation;
use ncui_core::{Capability, CapabilitySet, CorrelationId, ErrorKind, NcError, Result, WindowId};
use ncui_observability::logring::{LogLevel, LogRecord};
use ncui_security::authz::ConsentToken;
use ncui_webview::{MAX_WINDOWS, WindowRejection};
use serde_json::{Value, json};

use crate::Runtime;
use crate::dispatch::cdp_handler::cdp_error;

pub(crate) fn cursor_param(params: &Value) -> Result<Option<usize>> {
    let Some(value) = params.get("cursor") else {
        return Ok(None);
    };
    let text = value.as_str().ok_or_else(|| {
        validation("cursor", "must be a string").operation("dispatch::cursor_param")
    })?;
    if text.is_empty() {
        return Ok(Some(0));
    }
    let offset: usize = text.parse().map_err(|_| {
        validation(
            "cursor",
            "must be an opaque cursor returned by a previous query",
        )
        .operation("dispatch::cursor_param")
    })?;
    Ok(Some(offset))
}

/// The change a caller asked of a window's presentation, when it asked for one.
///
/// The rule lives with the change itself, because "at most one change, each named exactly" is a
/// property of what a change *is*; this only turns its refusal into the runtime's error shape.
pub(crate) fn window_change_param(params: &Value) -> Result<Option<ncui_webview::WindowChange>> {
    ncui_webview::WindowChange::from_payload(params).map_err(|error| {
        validation(error.field(), error.to_string()).operation("dispatch::window_change_param")
    })
}

/// The shape a caller asked a window to take.
///
/// Read field by field, so a payload that names one thing wrong is refused with the field that
/// produced it rather than as a shape that failed as a whole. The rule that a position or a size
/// names both of itself lives with the shape, because it is a property of what a shape is.
pub(crate) fn window_shape_param(params: &Value) -> Result<ncui_webview::WindowShape> {
    let title = match params.get("title") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| {
                    validation("title", "must be a string")
                        .operation("dispatch::window_shape_param")
                })?
                .to_owned(),
        ),
    };
    let dimension = |field: &'static str| -> Result<Option<u32>> {
        match params.get(field) {
            None => Ok(None),
            Some(value) => Ok(Some(window_dimension(value, field)?)),
        }
    };
    let coordinate = |field: &'static str| -> Result<Option<i32>> {
        match params.get(field) {
            None => Ok(None),
            Some(value) => Ok(Some(window_coordinate(value, field)?)),
        }
    };
    let flag = |field: &'static str| -> Result<Option<bool>> {
        match params.get(field) {
            None => Ok(None),
            Some(value) => Ok(Some(value.as_bool().ok_or_else(|| {
                validation(field, "must be true or false").operation("dispatch::window_shape_param")
            })?)),
        }
    };
    let shape = ncui_webview::WindowShape {
        title,
        x: coordinate("x")?,
        y: coordinate("y")?,
        width: dimension("width")?,
        height: dimension("height")?,
        always_on_top: flag("always_on_top")?,
        resizable: flag("resizable")?,
    };
    shape.validate().map_err(|error| {
        validation(error_field(&error), error.to_string()).operation("dispatch::window_shape_param")
    })?;
    Ok(shape)
}

/// The field a refused shape request should be blamed on.
pub(crate) fn error_field(error: &ncui_webview::WindowRequestError) -> &'static str {
    use ncui_webview::WindowRequestError as ShapeError;
    match error {
        ShapeError::EmptyTitle => "title",
        ShapeError::WidthOutOfRange { .. } | ShapeError::HalfSize => "width",
        ShapeError::HeightOutOfRange { .. } => "height",
        ShapeError::HalfPosition => "x",
        _ => "window",
    }
}

/// Evaluates a script in a window's page.
///
/// The host-to-page half of the bridge. The engine's answer is reported as it came: a
/// refusal is a refusal, because reporting it as a success would tell an agent the page
/// received something it did not.
pub(crate) fn invoke_window_post(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let id = window_id_param(params)?;
    let script = script_param(params)?;
    let posted = runtime
        .windows
        .post_to_page(id, &script)
        .map_err(window_rejection)?;
    if posted {
        serde_json::to_value(json!({ "posted": true })).map_err(|e| json_error(&e))
    } else {
        Err(NcError::new(ErrorKind::Conflict {
            reason: format!("the engine refused the script for window {id}"),
        })
        .operation("dispatch::window_post"))
    }
}

/// `ncui.window.drag.region` -- initiate a window drag from a drag region.
///
/// Called by the page when the user mousedowns on a `[data-ncui-drag-region]`
/// element. The handler asks the calling window's engine to start a platform
/// drag (`tao::window::Window::drag_window()`), which hands the move to the OS
/// until the mouse is released.
///
/// This is the NC-UI equivalent of Tauri 2's `app-region: drag` / Electron's
/// `-webkit-app-region: drag`. Without it, an undecorated window cannot be
/// moved by the user.
///
/// The request targets the calling window, so the engine asked is the one whose
/// page asked. A window with no engine, or an engine that refuses, is reported
/// as the refusal it is: a `dragging: true` answered from a headless registry
/// would tell the page a drag began that the user never felt.
pub(crate) fn invoke_window_drag_region(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let id = window_id_param(params)?;
    let dragged = runtime.windows.drag_window(id).map_err(|_| {
        NcError::new(ErrorKind::NotFound {
            kind: "window".to_owned(),
            id: id.to_string(),
        })
        .operation("dispatch::window_drag_region")
    })?;
    if dragged {
        serde_json::to_value(json!({ "dragging": true })).map_err(|e| json_error(&e))
    } else {
        Err(NcError::new(ErrorKind::Conflict {
            reason: format!("the engine could not start a drag for window {id}"),
        })
        .operation("dispatch::window_drag_region"))
    }
}

/// `ncui.window.zoom` -- scale a window's page by a factor.
///
/// `1.0` is the platform default; the factor is applied by the platform, and the next
/// paint shows it. A window that is not open, or an engine that cannot scale, is reported
/// as the refusal it is rather than answered with a zoom that never happened.
/// `ncui.window.child.create` -- embed a child webview into a window's frame.
///
/// The request is validated by the child crate; the engine builds the surface on
/// its own thread and the child is recorded with its parent's identity.
pub(crate) fn invoke_window_child_create(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    const OPERATION: &str = "dispatch::window_child_create";
    let parent = window_id_param(params)?;
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| validation("url", "must be a non-empty string").operation(OPERATION))?;
    let request = ncui_webview_child::ChildWebviewRequest {
        url: url.to_owned(),
        ..child_bounds_param(params, OPERATION)?
    };
    validate_child_request(&request, OPERATION)?;
    let created = runtime
        .create_child_webview(parent, &request)
        .map_err(|_| {
            NcError::new(ErrorKind::NotFound {
                kind: "window".to_owned(),
                id: parent.to_string(),
            })
            .operation("dispatch::window_child_create")
        })?
        .map_err(|reason| {
            NcError::new(ErrorKind::Conflict { reason }).operation("dispatch::window_child_create")
        })?;
    serde_json::to_value(json!({ "child": created.to_string() })).map_err(|e| json_error(&e))
}

/// `ncui.window.child.bounds` -- move a live child webview.
pub(crate) fn invoke_window_child_bounds(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    const OPERATION: &str = "dispatch::window_child_bounds";
    let parent = window_id_param(params)?;
    let child = child_id_param(params, OPERATION)?;
    let request = child_bounds_param(params, OPERATION)?;
    validate_child_request(&request, OPERATION)?;
    runtime
        .set_child_webview_bounds(parent, child, &request)
        .map_err(|_| {
            NcError::new(ErrorKind::NotFound {
                kind: "window".to_owned(),
                id: parent.to_string(),
            })
            .operation("dispatch::window_child_bounds")
        })?
        .map_err(|reason| {
            NcError::new(ErrorKind::Conflict { reason }).operation("dispatch::window_child_bounds")
        })?;
    serde_json::to_value(json!({ "moved": true })).map_err(|e| json_error(&e))
}

/// `ncui.window.child.close` -- close a live child webview.
pub(crate) fn invoke_window_child_close(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let parent = window_id_param(params)?;
    let child = child_id_param(params, "dispatch::window_child_close")?;
    runtime
        .close_child_webview(parent, child)
        .map_err(|_| {
            NcError::new(ErrorKind::NotFound {
                kind: "window".to_owned(),
                id: parent.to_string(),
            })
            .operation("dispatch::window_child_close")
        })?
        .map_err(|reason| {
            NcError::new(ErrorKind::Conflict { reason }).operation("dispatch::window_child_close")
        })?;
    serde_json::to_value(json!({ "closed": true })).map_err(|e| json_error(&e))
}

/// `ncui.window.child.list` -- every live child webview.
pub(crate) fn invoke_window_child_list(runtime: &mut Runtime) -> Result<Value> {
    let children = runtime.list_child_webviews();
    let listed: Vec<Value> = children
        .iter()
        .map(|entry| {
            json!({
                "child": entry.id.to_string(),
                "parent": entry.parent.to_string(),
                "x": entry.bounds.x,
                "y": entry.bounds.y,
                "width": entry.bounds.width,
                "height": entry.bounds.height,
            })
        })
        .collect();
    serde_json::to_value(json!({ "children": listed })).map_err(|e| json_error(&e))
}

fn child_id_param(
    params: &Value,
    operation: &'static str,
) -> Result<ncui_webview_child::ChildWebviewId> {
    params
        .get("child")
        .and_then(Value::as_str)
        .and_then(ncui_webview_child::ChildWebviewId::parse)
        .ok_or_else(|| {
            validation("child", "must be a canonical child webview id").operation(operation)
        })
}

fn child_bounds_param(
    params: &Value,
    operation: &'static str,
) -> Result<ncui_webview_child::ChildWebviewRequest> {
    let coordinate = |field: &str| -> Result<i32> {
        let value = params
            .get(field)
            .and_then(Value::as_i64)
            .ok_or_else(|| validation(field, "must be an integer").operation(operation))?;
        i32::try_from(value)
            .map_err(|_| validation(field, "must fit in a 32-bit coordinate").operation(operation))
    };
    let extent = |field: &str| -> Result<u32> {
        let value = params.get(field).and_then(Value::as_u64).ok_or_else(|| {
            validation(field, "must be a non-negative integer").operation(operation)
        })?;
        u32::try_from(value)
            .map_err(|_| validation(field, "must fit in a 32-bit extent").operation(operation))
    };
    Ok(ncui_webview_child::ChildWebviewRequest {
        url: String::new(),
        x: coordinate("x")?,
        y: coordinate("y")?,
        width: extent("width")?,
        height: extent("height")?,
    })
}

fn validate_child_request(
    request: &ncui_webview_child::ChildWebviewRequest,
    operation: &'static str,
) -> Result<()> {
    let result = if request.url.is_empty() {
        request.validate_bounds()
    } else {
        request.validate()
    };
    result.map_err(|error| match error {
        ncui_webview_child::ChildWebviewError::InvalidRequest { field, reason } => {
            validation(field, reason).operation(operation)
        }
        _ => validation("child", error.to_string()).operation(operation),
    })
}

pub(crate) fn invoke_window_zoom(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let id = window_id_param(params)?;
    let factor = params
        .get("factor")
        .and_then(Value::as_f64)
        .filter(|factor| (0.25..=5.0).contains(factor))
        .ok_or_else(|| {
            validation("factor", "must be a number between 0.25 and 5.0")
                .operation("dispatch::window_zoom")
        })?;
    let applied = runtime.windows.set_zoom(id, factor).map_err(|_| {
        NcError::new(ErrorKind::NotFound {
            kind: "window".to_owned(),
            id: id.to_string(),
        })
        .operation("dispatch::window_zoom")
    })?;
    if applied {
        serde_json::to_value(json!({ "zoomed": true, "factor": factor }))
            .map_err(|e| json_error(&e))
    } else {
        Err(NcError::new(ErrorKind::Conflict {
            reason: format!("the engine could not scale window {id}"),
        })
        .operation("dispatch::window_zoom"))
    }
}

/// `ncui.window.print_to_pdf` — render a window's page to PDF.
///
/// The engine is the printer: the webview is a Chromium engine, and its own `DevTools`
/// protocol (`Page.printToPDF`) renders the live page exactly as the browser's print
/// preview would. The command rides the same loopback transport `ncui.cdp.send` uses —
/// no FFI, no new dependency, no second path to keep in step — and returns the PDF as
/// base64 rather than writing a file nobody asked for. A caller that wants it on disk
/// writes the bytes it is given, under the authority it already holds.
pub(crate) fn invoke_window_print_to_pdf(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    const OPERATION: &str = "dispatch::window_print_to_pdf";
    let id = window_id_param(params)?;
    let mut cdp_params = serde_json::Map::new();
    if let Some(landscape) = optional_bool(params, "landscape")? {
        cdp_params.insert("landscape".to_owned(), json!(landscape));
    }
    if let Some(print_background) = optional_bool(params, "print_background")? {
        cdp_params.insert("printBackground".to_owned(), json!(print_background));
    }
    if let Some(scale) = optional_number(params, "scale")? {
        if !(0.1..=2.0).contains(&scale) {
            return Err(
                validation("scale", "must be a number between 0.1 and 2.0").operation(OPERATION)
            );
        }
        cdp_params.insert("scale".to_owned(), json!(scale));
    }
    for (field, cdp_field) in [
        ("paper_width", "paperWidth"),
        ("paper_height", "paperHeight"),
    ] {
        if let Some(inches) = optional_number(params, field)? {
            if !(1.0..=100.0).contains(&inches) {
                return Err(
                    validation(field, "must be a number between 1 and 100 inches")
                        .operation(OPERATION),
                );
            }
            cdp_params.insert(cdp_field.to_owned(), json!(inches));
        }
    }
    let data = cdp_capture_data(
        runtime,
        id,
        "Page.printToPDF",
        &Value::Object(cdp_params),
        OPERATION,
    )?;
    capture_answer(id, "pdf", &data)
}

/// `ncui.window.screenshot` — capture a window's page as an image.
///
/// The same engine transport carries Chromium's `Page.captureScreenshot`, which is how
/// the browser's own inspector photographs a page. The image is returned as base64 in
/// the requested format; what is captured is the page's content, and the platform's
/// window frame around it is not part of the picture.
pub(crate) fn invoke_window_screenshot(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    const OPERATION: &str = "dispatch::window_screenshot";
    let id = window_id_param(params)?;
    let mut cdp_params = serde_json::Map::new();
    let mut format = "png";
    if let Some(requested) = params.get("format").and_then(Value::as_str) {
        if requested != "png" && requested != "jpeg" {
            return Err(validation("format", "must be \"png\" or \"jpeg\"").operation(OPERATION));
        }
        format = requested;
        cdp_params.insert("format".to_owned(), json!(requested));
    }
    if let Some(quality) = optional_number(params, "quality")? {
        if !(0.0..=100.0).contains(&quality) {
            return Err(
                validation("quality", "must be a number between 0 and 100").operation(OPERATION)
            );
        }
        cdp_params.insert("quality".to_owned(), json!(quality));
    }
    let data = cdp_capture_data(
        runtime,
        id,
        "Page.captureScreenshot",
        &Value::Object(cdp_params),
        OPERATION,
    )?;
    capture_answer(id, format, &data)
}

/// Sends one CDP capture command to a window's engine and returns the base64 payload.
///
/// The mapping matches `ncui.cdp.send`'s: a window that is not open is `not_found`, and an
/// engine without the transport or an endpoint that refuses preserves its typed `cdp_*`
/// code. An answer without the promised payload is a conflict rather than an empty success.
fn cdp_capture_data(
    runtime: &mut Runtime,
    id: WindowId,
    method: &str,
    cdp_params: &Value,
    operation: &'static str,
) -> Result<String> {
    let command = json!({ "id": 1, "method": method, "params": cdp_params });
    let answer = runtime
        .windows
        .cdp_send(id, &command)
        .map_err(|_| {
            NcError::new(ErrorKind::NotFound {
                kind: "window".to_owned(),
                id: id.to_string(),
            })
            .operation(operation)
        })?
        .map_err(|error| cdp_error(&error, operation))?;
    answer
        .get("result")
        .and_then(|result| result.get("data"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            NcError::new(ErrorKind::Conflict {
                reason: format!("the engine answered {method} without capture data"),
            })
            .operation(operation)
        })
}

/// The capture answer: the payload, and the byte length it decodes to.
fn capture_answer(id: WindowId, format: &str, data: &str) -> Result<Value> {
    let bytes = base64_decoded_len(data);
    serde_json::to_value(json!({
        "window": id.to_string(),
        "format": format,
        "bytes": bytes,
        "data": data,
    }))
    .map_err(|e| json_error(&e))
}

/// The byte length a canonical base64 string decodes to, from its length and padding.
fn base64_decoded_len(encoded: &str) -> usize {
    let padding = encoded.matches('=').count();
    encoded.len() * 3 / 4 - padding
}

/// An optional boolean parameter: absent or `null` means "the engine's default".
fn optional_bool(params: &Value, field: &str) -> Result<Option<bool>> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_bool().map(Some).ok_or_else(|| {
            validation(field, "must be a boolean").operation("dispatch::optional_param")
        }),
    }
}

/// An optional number parameter: absent or `null` means "the engine's default".
fn optional_number(params: &Value, field: &str) -> Result<Option<f64>> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_f64().map(Some).ok_or_else(|| {
            validation(field, "must be a number").operation("dispatch::optional_param")
        }),
    }
}

/// Resolves the window for a devtools request, refusing an unknown window by name.
fn devtools_window(runtime: &Runtime, params: &Value) -> Result<WindowId> {
    let id = window_id_param(params)?;
    // The state call is the existence probe: `Ok(_)` for a live window (its engine may
    // still answer "unknown"), `Err` for one that is not open.
    if runtime.windows.is_devtools_open(id).is_err() {
        return Err(NcError::new(ErrorKind::NotFound {
            kind: "window".to_owned(),
            id: id.to_string(),
        })
        .operation("dispatch::window_devtools"));
    }
    Ok(id)
}

/// `ncui.window.devtools.open` -- open a window's platform web inspector.
pub(crate) fn invoke_window_devtools_open(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let id = devtools_window(runtime, params)?;
    let opened = runtime.windows.open_devtools(id).map_err(|_| {
        NcError::new(ErrorKind::NotFound {
            kind: "window".to_owned(),
            id: id.to_string(),
        })
        .operation("dispatch::window_devtools")
    })?;
    serde_json::to_value(json!({ "opened": opened })).map_err(|e| json_error(&e))
}

/// `ncui.window.devtools.close` -- close a window's platform web inspector.
pub(crate) fn invoke_window_devtools_close(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let id = devtools_window(runtime, params)?;
    let closed = runtime.windows.close_devtools(id).map_err(|_| {
        NcError::new(ErrorKind::NotFound {
            kind: "window".to_owned(),
            id: id.to_string(),
        })
        .operation("dispatch::window_devtools")
    })?;
    serde_json::to_value(json!({ "closed": closed })).map_err(|e| json_error(&e))
}

/// `ncui.window.devtools.state` -- whether a window's platform web inspector is open.
///
/// `"unknown"` is an answer, not a gap: a platform that opens the inspector without a
/// state query (wry's Windows backend) is reported as unknown rather than as closed.
pub(crate) fn invoke_window_devtools_state(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let id = devtools_window(runtime, params)?;
    let state = runtime.windows.is_devtools_open(id).map_err(|_| {
        NcError::new(ErrorKind::NotFound {
            kind: "window".to_owned(),
            id: id.to_string(),
        })
        .operation("dispatch::window_devtools")
    })?;
    let state = state.map_or("unknown", |open| if open { "open" } else { "closed" });
    serde_json::to_value(json!({ "state": state })).map_err(|e| json_error(&e))
}

/// The budget a caller gave a call, when it gave one.
///
/// Bounded at both ends because it reaches a comparison against a clock: a budget of zero and a
/// budget of a year are both numbers a caller typed by mistake, and the runtime refuses them rather
/// than reading the first as "no budget" and the second as "no limit". The ceiling is an hour, which
/// is longer than any call this runtime serves and short enough that a caller cannot ask for a
/// deadline that outlives the process.
pub(crate) fn deadline_param(params: &Value) -> Result<Option<Duration>> {
    /// The shortest budget a caller may ask for.
    const MIN_MS: u64 = 1;
    /// The longest budget a caller may ask for.
    const MAX_MS: u64 = 3_600_000;
    let Some(value) = params.get("deadline_ms") else {
        return Ok(None);
    };
    let milliseconds = value.as_u64().ok_or_else(|| {
        validation("deadline_ms", "must be a whole number of milliseconds")
            .operation("dispatch::deadline_param")
    })?;
    if !(MIN_MS..=MAX_MS).contains(&milliseconds) {
        return Err(validation(
            "deadline_ms",
            format!("must be between {MIN_MS} and {MAX_MS} milliseconds, got {milliseconds}"),
        )
        .operation("dispatch::deadline_param"));
    }
    Ok(Some(Duration::from_millis(milliseconds)))
}

/// Applies the caller's budget to a completed outcome.
///
/// The budget is a report rather than an interruption: the pipeline is synchronous, so a
/// handler runs to completion and what the caller is owed is the truth about how long it
/// took. A success that outran the budget is answered as a timeout, and the timeout is
/// counted rather than silent. The audit is recorded by the caller from the real outcome,
/// so a call that was allowed and then ran long is not filed as a denial.
pub(crate) fn enforce_deadline(
    outcome: Result<Value>,
    started: Instant,
    deadline: Option<Duration>,
    method: &str,
    metrics: &ncui_observability::metrics::MetricsRegistry,
    correlation: CorrelationId,
) -> Result<Value> {
    match (outcome, deadline) {
        (Ok(_), Some(budget)) if started.elapsed() > budget => {
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let _ = metrics.counter_inc(
                "request.timeouts",
                &ncui_observability::metrics::Labels::new(),
                1,
            );
            tracing::warn!(
                method = %method,
                budget_ms = budget.as_millis(),
                elapsed_ms,
                "a call exceeded the deadline its caller gave it"
            );
            Err(NcError::new(ErrorKind::Timeout { elapsed_ms })
                .operation("dispatch::deadline")
                .correlated(correlation))
        }
        (other, _) => other,
    }
}

/// Reads the `script` parameter of a window post.
///
/// The script is a page's own code, so it is bounded: an unbounded script parameter is an
/// unbounded allocation driven by a caller, and the limit is the same one the transport
/// applies to a frame.
pub(crate) fn script_param(params: &Value) -> Result<String> {
    let script = params
        .get("script")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            validation("script", "must be a string").operation("dispatch::script_param")
        })?;
    if script.is_empty() {
        return Err(validation("script", "must not be empty").operation("dispatch::script_param"));
    }
    if script.len() > MAX_SCRIPT_BYTES {
        return Err(validation(
            "script",
            format!("must not exceed {MAX_SCRIPT_BYTES} bytes"),
        )
        .operation("dispatch::script_param"));
    }
    Ok(script.to_owned())
}

/// The largest script the host will evaluate in a page.
///
/// Bounded because the script is an allocation the caller chooses the size of, and the
/// host holds it in a queue behind the engine's event loop.
const MAX_SCRIPT_BYTES: usize = 64 * 1024;

/// Reads the `url` parameter of a window request.
pub(crate) fn url_param(params: &Value) -> Result<String> {
    params
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| validation("url", "must be a string").operation("dispatch::url_param"))
        .and_then(|url| {
            if url.is_empty() {
                Err(validation("url", "must not be empty").operation("dispatch::url_param"))
            } else {
                Ok(url.to_owned())
            }
        })
}

/// Reads the shape of a window from a window request's parameters.
///
/// The URL is required; the title and the size are optional and fall back to the defaults
/// the window would have had anyway. A size that is present but not a number, or is
/// outside the range a window may be created at, is a refusal rather than a value the
/// engine has to interpret.
/// Which window option a payload field sets.
///
/// Named rather than passed as a closure so the loop above reads as a list of fields, and so a
/// field added to the request is a compile error here rather than a silently ignored parameter.
#[derive(Debug, Clone, Copy)]
enum Flag {
    /// Whether the window floats above other windows.
    AlwaysOnTop,
    /// Whether the window carries its own frame.
    Decorations,
    /// Whether the window's background may be see-through.
    Transparent,
    /// Whether the user may resize the window.
    Resizable,
    /// Whether the window fills its display.
    Fullscreen,
    /// Whether the window is created maximised.
    Maximized,
}

/// One coordinate of a window's place, from a caller that may be a page.
///
/// Bounded because the value reaches the platform: a coordinate far outside any display is not a
/// place, it is a number, and a window asked for one is a window nobody can find. The bound is
/// generous — a display may legitimately sit thousands of pixels from the origin, and negative
/// coordinates are ordinary on a desk with a display to the left.
pub(crate) fn window_coordinate(value: &Value, field: &str) -> Result<i32> {
    /// How far from the origin a display may be. A coordinate past this is a number, not a place.
    const LIMIT: i64 = 1_000_000;
    let number = value.as_i64().ok_or_else(|| {
        validation(field, "must be a whole number").operation("dispatch::window_coordinate")
    })?;
    if number.abs() > LIMIT {
        return Err(validation(
            field,
            format!("must be between -{LIMIT} and {LIMIT}, got {number}"),
        )
        .operation("dispatch::window_coordinate"));
    }
    i32::try_from(number).map_err(|_| {
        validation(field, "is not a coordinate a display could have")
            .operation("dispatch::window_coordinate")
    })
}

/// The place a page's remembered window was, when the page named it and it was remembered.
///
/// A name is bounded and may not carry a control character, for the reason a state key may not:
/// it is written into a file a person reads.
pub(crate) fn remembered_place(
    runtime: &Runtime,
    params: &Value,
) -> Result<Option<ncui_webview::WindowGeometry>> {
    let Some(key) = params.get("remember") else {
        return Ok(None);
    };
    let key = key.as_str().ok_or_else(|| {
        validation("remember", "must be a string").operation("dispatch::remembered_place")
    })?;
    if key.trim().is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        return Err(validation(
            "remember",
            "must be a name: not empty, at most 128 bytes, without control characters",
        )
        .operation("dispatch::remembered_place"));
    }
    Ok(runtime.windows.place_for(key))
}

/// Reads the `content` of a window request, when the caller named one.
///
/// Absent means a page window — the default the request contract itself carries, so a
/// payload written before native windows existed means exactly what it always meant. The
/// tagged shape is the contract's own (`{"kind": "page"}` or `{"kind": "native",
/// "provider": ..., "props": ...}`), parsed by serde rather than field by field because
/// the contract crate is what owns which shapes exist.
pub(crate) fn content_param(params: &Value) -> Result<Option<ncui_webview::WindowContent>> {
    let Some(value) = params.get("content") else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| {
            validation(
                "content",
                format!("must be a window content object: {error}"),
            )
            .operation("dispatch::content_param")
        })
}

/// Builds the base request a window's parameters describe, before its shape is applied.
///
/// The content decides how the URL is read at all: a page window requires one, exactly as
/// it always has, while a native window carries none — so the reading happens here, with
/// the strictness that kind of window answers to.
fn base_request_param(
    params: &Value,
    content: Option<ncui_webview::WindowContent>,
) -> Result<ncui_webview::WindowRequest> {
    match content {
        Some(ncui_webview::WindowContent::Native { provider, props }) => {
            let mut request = ncui_webview::WindowRequest::native(provider).with_props(props);
            // A URL a caller passed alongside native content is carried into the request so
            // the contract's own validation rejects it, rather than being refused here with a
            // second wording of the same rule — or worse, dropped, which would accept a
            // payload the caller did not mean.
            if let Some(value) = params.get("url") {
                value
                    .as_str()
                    .ok_or_else(|| {
                        validation("url", "must be a string").operation("dispatch::url_param")
                    })?
                    .clone_into(&mut request.url);
            }
            Ok(request)
        }
        Some(ncui_webview::WindowContent::Page) | None => {
            Ok(ncui_webview::WindowRequest::new(url_param(params)?))
        }
    }
}

pub(crate) fn window_request_param(
    params: &Value,
    remembered_place: Option<ncui_webview::WindowGeometry>,
) -> Result<ncui_webview::WindowRequest> {
    let content = content_param(params)?;
    let title = match params.get("title") {
        None => None,
        Some(value) => Some(value.as_str().ok_or_else(|| {
            validation("title", "must be a string").operation("dispatch::window_request_param")
        })?),
    };
    let width = match params.get("width") {
        None => None,
        Some(value) => Some(window_dimension(value, "width")?),
    };
    let height = match params.get("height") {
        None => None,
        Some(value) => Some(window_dimension(value, "height")?),
    };

    let place = match (params.get("x"), params.get("y")) {
        (None, None) => None,
        (Some(x), Some(y)) => Some((window_coordinate(x, "x")?, window_coordinate(y, "y")?)),
        // One axis is refused rather than half-applied: a window placed by one coordinate and
        // left to the platform on the other is a window at a place nobody asked for, and the
        // platform's own rule for a new window is a better answer than half of a caller's.
        _ => {
            return Err(validation("x", "and `y` must be given together")
                .operation("dispatch::window_request_param"));
        }
    };

    let mut request = base_request_param(params, content)?;
    if let Some(title) = title {
        request = request.with_title(title);
    }
    // A window the page named is a window whose place is remembered: the place is applied here,
    // before anything else, so the window is created where it was rather than moved there after
    // the user has seen it somewhere else.
    if let Some(place) = remembered_place {
        request = request.with_size(place.width, place.height);
        request = request.with_position(place.x, place.y);
    }
    // The options are read as flags: a caller that names one gets it, and a caller that names none
    // gets the default the request already carries — which is why each is only applied when it is
    // present, rather than being written back as false.
    for (field, apply) in [
        ("always_on_top", Flag::AlwaysOnTop),
        ("decorations", Flag::Decorations),
        ("transparent", Flag::Transparent),
        ("resizable", Flag::Resizable),
        ("fullscreen", Flag::Fullscreen),
        ("maximized", Flag::Maximized),
    ] {
        if let Some(value) = params.get(field) {
            let value = value.as_bool().ok_or_else(|| {
                validation(field, "must be true or false")
                    .operation("dispatch::window_request_param")
            })?;
            request = match apply {
                Flag::AlwaysOnTop => request.with_always_on_top(value),
                Flag::Decorations => request.with_decorations(value),
                Flag::Transparent => request.with_transparent(value),
                Flag::Resizable => request.with_resizable(value),
                Flag::Fullscreen => request.with_fullscreen(value),
                Flag::Maximized => request.with_maximized(value),
            };
        }
    }
    let minimum = match (params.get("min_width"), params.get("min_height")) {
        (None, None) => None,
        (Some(width), Some(height)) => Some((
            window_dimension(width, "min_width")?,
            window_dimension(height, "min_height")?,
        )),
        // The same refusal the request makes, reported here so a page learns which field was
        // missing rather than reading it out of a message about the pair.
        _ => {
            return Err(
                validation("min_width", "and `min_height` must be given together")
                    .operation("dispatch::window_request_param"),
            );
        }
    };
    let mut width_value = request.width;
    let mut height_value = request.height;
    if let Some(width) = width {
        width_value = width;
    }
    if let Some(height) = height {
        height_value = height;
    }
    request = request.with_size(width_value, height_value);
    if let Some((x, y)) = place {
        request = request.with_position(x, y);
    }
    if let Some((width, height)) = minimum {
        request = request.with_min_size(width, height);
    }
    request.validate().map_err(|error| {
        validation("window", error.to_string()).operation("dispatch::window_request_param")
    })?;
    Ok(request)
}

/// Reads one dimension of a window request.
pub(crate) fn window_dimension(value: &Value, field: &'static str) -> Result<u32> {
    let raw = value.as_u64().ok_or_else(|| {
        validation(field, "must be a non-negative integer")
            .operation("dispatch::window_request_param")
    })?;
    u32::try_from(raw).map_err(|_| {
        validation(field, "is larger than the largest window")
            .operation("dispatch::window_request_param")
    })
}

/// Reads and narrows the `grant` parameter of a window request.
///
/// Absent means the default grant; a caller may only narrow down, never widen.
pub(crate) fn grant_param(params: &Value) -> Result<Option<CapabilitySet>> {
    match params.get("grant") {
        None => Ok(None),
        Some(value) => {
            let names: Vec<String> = value
                .as_array()
                .ok_or_else(|| {
                    validation("grant", "must be an array of capability strings")
                        .operation("dispatch::grant_param")
                })?
                .iter()
                .map(|item| {
                    item.as_str()
                        .ok_or_else(|| {
                            validation("grant", "each entry must be a capability string")
                                .operation("dispatch::grant_param")
                        })
                        .map(ToOwned::to_owned)
                })
                .collect::<Result<Vec<_>>>()?;
            let mut set = CapabilitySet::empty();
            for name in names {
                set = set.with(name.parse::<Capability>().map_err(|e| {
                    validation("grant", format!("unknown capability `{name}`: {e}"))
                        .operation("dispatch::grant_param")
                })?);
            }
            Ok(Some(set))
        }
    }
}

/// Reads and parses a `window` identifier parameter.
pub(crate) fn window_id_param(params: &Value) -> Result<WindowId> {
    params
        .get("window")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            validation("window", "must be a window id string")
                .operation("dispatch::window_id_param")
        })?
        .parse::<WindowId>()
        .map_err(|e| validation("window", e.to_string()).operation("dispatch::window_id_param"))
}

/// A severity threshold for a log query.
///
/// `None` is "every level", which is what a query with no `level` parameter means. A
/// threshold is a *floor on severity*: `warn` returns warnings and errors, because
/// [`LogLevel`] is ordered most-severe-first and a caller asking for warnings wants the
/// failures too.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LevelFilter {
    threshold: Option<LogLevel>,
}

impl LevelFilter {
    /// Whether a record passes the filter.
    pub(crate) fn matches(self, record: &LogRecord) -> bool {
        match self.threshold {
            None => true,
            Some(threshold) => record.level <= threshold,
        }
    }
}

/// Reads the `level` parameter of a log query.
///
/// An absent parameter is not a filter; an unrecognised one is a refusal, because
/// silently ignoring a level a caller asked for would return records they did not ask
/// for and call them a match.
pub(crate) fn log_level_filter(params: &Value) -> Result<LevelFilter> {
    let Some(value) = params.get("level") else {
        return Ok(LevelFilter { threshold: None });
    };
    let text = value.as_str().ok_or_else(|| {
        validation("level", "must be a level name").operation("dispatch::log_level_filter")
    })?;
    let level = LogLevel::from_code(text).ok_or_else(|| {
        validation(
            "level",
            format!("`{text}` is not a level; expected one of error, warn, info, debug, trace"),
        )
        .operation("dispatch::log_level_filter")
    })?;
    Ok(LevelFilter {
        threshold: Some(level),
    })
}

/// A correlation-id filter for a log query.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CorrelationFilter {
    wanted: Option<CorrelationId>,
}

impl CorrelationFilter {
    /// Whether a record passes the filter.
    pub(crate) fn matches(self, record: &LogRecord) -> bool {
        match self.wanted {
            None => true,
            Some(wanted) => record.correlation == Some(wanted),
        }
    }
}

/// Reads the `correlation` parameter of a log query.
///
/// An absent parameter is not a filter; an unparseable one is a refusal, for the same
/// reason an unrecognised level is.
pub(crate) fn correlation_filter(params: &Value) -> Result<CorrelationFilter> {
    let Some(value) = params.get("correlation") else {
        return Ok(CorrelationFilter { wanted: None });
    };
    let text = value.as_str().ok_or_else(|| {
        validation("correlation", "must be a correlation id string")
            .operation("dispatch::correlation_filter")
    })?;
    let id = text.parse::<CorrelationId>().map_err(|e| {
        validation("correlation", e.to_string()).operation("dispatch::correlation_filter")
    })?;
    Ok(CorrelationFilter { wanted: Some(id) })
}

/// Converts a [`WindowRejection`] into the error class dispatch reports.
pub(crate) fn window_rejection(rejection: WindowRejection) -> NcError {
    match rejection {
        WindowRejection::AtCapacity { open } => NcError::new(ErrorKind::Conflict {
            reason: format!("at capacity: {open} windows are open and the limit is {MAX_WINDOWS}"),
        })
        .operation("dispatch::window"),
        WindowRejection::Ungrantable { capability } => NcError::new(ErrorKind::CapabilityDenied {
            capability: capability.to_string(),
            subject: "page".to_owned(),
        })
        .operation("dispatch::window"),
        WindowRejection::EngineUnavailable { reason } => NcError::new(ErrorKind::NotFound {
            kind: "webview_host".to_owned(),
            id: reason.unwrap_or_else(|| "engine unavailable".to_owned()),
        })
        .operation("dispatch::window"),
        WindowRejection::Unrequestable { reason } => NcError::new(ErrorKind::Validation {
            field: "window".to_owned(),
            reason,
        })
        .operation("dispatch::window"),
        WindowRejection::Unreachable { url, reason } => NcError::new(ErrorKind::Validation {
            field: "url".to_owned(),
            reason: format!("`{url}` cannot be opened in a window: {reason}"),
        })
        .operation("dispatch::window"),
    }
}
///
/// Exceeding the maximum is a refusal, not a silent clamp: a silently clamped result reads
/// as "this is everything", which is worse than an error.
pub(crate) fn bounded_limit(params: &Value) -> Result<usize> {
    const MAX_LIMIT: usize = 10_000;
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(Ok(100), |value| {
            if value == 0 {
                Err(NcError::new(ErrorKind::Validation {
                    field: "limit".to_owned(),
                    reason: "must be greater than zero".to_owned(),
                })
                .operation("dispatch::limit"))
            } else if value > MAX_LIMIT as u64 {
                Err(NcError::new(ErrorKind::Validation {
                    field: "limit".to_owned(),
                    reason: format!("must not exceed {MAX_LIMIT}"),
                })
                .operation("dispatch::limit"))
            } else {
                Ok(value as usize)
            }
        })?;
    Ok(limit)
}

/// Inspects one mounted component by identity.
///
/// The component must be present in the host's manifest index; an unknown identity is a
/// refusal rather than an empty answer, so an agent cannot mistake "not found" for "no
/// data".
pub(crate) fn invoke_component_inspect(runtime: &Runtime, params: &Value) -> Result<Value> {
    let components = runtime.windows.manifests().map_err(window_rejection)?;
    // An absent `component` returns the whole set, so the method doubles as a detailed
    // companion to `ncui.components.list` rather than requiring a second call.
    let Some(wanted) = params.get("component").and_then(Value::as_str) else {
        return serde_json::to_value(json!({ "components": components }))
            .map_err(|e| json_error(&e));
    };
    let wanted: ncui_core::ComponentId = wanted.parse::<ncui_core::ComponentId>().map_err(|e| {
        validation("component", e.to_string()).operation("dispatch::component_param")
    })?;
    let found = components.into_iter().find(|c| c.id == wanted);
    match found {
        Some(component) => {
            serde_json::to_value(json!({ "component": component })).map_err(|e| json_error(&e))
        }
        None => Err(NcError::new(ErrorKind::NotFound {
            kind: "component".to_owned(),
            id: wanted.to_string(),
        })
        .operation("dispatch::component_inspect")),
    }
}

/// A snapshot of the runtime's observable state.
///
/// Aggregates what the more specific `*.inspect` methods report, so an agent can read
/// the whole shape of the system in one call without issuing several.
pub(crate) fn invoke_state_inspect(runtime: &Runtime) -> Result<Value> {
    let workers = runtime.workers.snapshot();
    let worker_count = workers.len();
    let windows = runtime.windows.summaries().ok();
    let window_count = windows.as_ref().map_or(0, Vec::len);
    let component_count = runtime.windows.manifests().map_or(0, |m| m.len());
    serde_json::to_value(json!({
        "windows": window_count,
        "components": component_count,
        "workers": worker_count,
        "scheduler_ready_depth": runtime.scheduler.depth(),
        "scheduler_capacity": runtime.scheduler.capacity(),
        "log_ring_len": runtime.logs.len(),
        "log_ring_capacity": runtime.logs.capacity(),
        "log_ring_evicted": runtime.logs.evicted_total(),
        "metrics_series": runtime.metrics.len(),
        "metrics_evicted": runtime.metrics.evicted_total(),
        "tracing_active": runtime.tracing_active,
        "profiling_active": runtime.profiling_active,
    }))
    .map_err(|e| json_error(&e))
}

pub(crate) fn json_error(error: &serde_json::Error) -> NcError {
    NcError::new(ErrorKind::Codec {
        operation: "dispatch::serialize".to_owned(),
        reason: error.to_string(),
    })
}

/// `ncui.devtools.tree` -- live snapshot of the window tree.
///
/// Returns every open window with its lifecycle state and capability grant, plus
/// the total window count and the runtime's uptime. This is the data an agent needs
/// to understand "what is on screen right now" without polling each window individually.
pub(crate) fn invoke_devtools_tree(runtime: &Runtime) -> Result<Value> {
    let windows: Vec<ncui_webview::WindowSummary> = runtime.windows.summaries().unwrap_or_default();
    let mut window_list: Vec<Value> = Vec::with_capacity(windows.len());
    for w in &windows {
        window_list.push(json!({
            "id": w.id.to_string(),
            "title": w.title,
            "state": w.state,
            "url": w.url,
            "serving": w.serving,
        }));
    }
    serde_json::to_value(json!({
        "windows": window_list,
        "window_count": window_list.len(),
        "uptime_ms": runtime.elapsed().as_millis() as u64,
    }))
    .map_err(|e| json_error(&e))
}

/// `ncui.devtools.events` -- recent runtime events, filterable by kind.
///
/// Returns up to `limit` events from the devtools ring, newest first. If `kind`
/// is specified in the params, only events of that kind are returned.
pub(crate) fn invoke_devtools_events(runtime: &Runtime, params: &Value) -> Result<Value> {
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .min(1024) as usize;
    let kind = params.get("kind").and_then(Value::as_str);
    let events = runtime.devtools.events(kind, limit);
    serde_json::to_value(json!({
        "events": events,
        "count": events.len(),
    }))
    .map_err(|e| json_error(&e))
}

/// `ncui.devtools.profile` -- per-method timing histograms.
///
/// Returns profiling data for all methods recorded while profiling was active.
/// Each entry has p50/p90/p95/p99/max in nanoseconds and the total call count.
pub(crate) fn invoke_devtools_profile(runtime: &Runtime) -> Result<Value> {
    let entries = runtime.devtools.profile();
    let call_count = runtime.devtools.profile_call_count();
    serde_json::to_value(json!({
        "methods": entries,
        "total_calls": call_count,
        "profiling_active": runtime.profiling_active,
    }))
    .map_err(|e| json_error(&e))
}

/// `ncui.app.emit` -- emit an application-defined event to a window's page.
///
/// The JSON-RPC wrapper over [`Runtime::emit`] / [`Runtime::emit_all`], the
/// host-to-page event delivery path that Tauri 2 provides via `app.emit`. The
/// page's bridge dispatches the event to listeners registered via
/// `window.ncui.on(event, handler)`, exactly as the built-in `resized`/`moved`/
/// `theme` events arrive.
///
/// # Parameters
///
/// - `event` (string, required): the event name. Non-empty, at most 256 bytes.
/// - `payload` (any, optional): the event payload. Defaults to `null`.
/// - `window` (string, optional): the window ID to emit to. Omitted, or `"all"`,
///   broadcasts to every open window.
///
/// A window that is not open, or whose engine refuses the script, is an error
/// rather than a silent `emitted: false`: the caller is told the delivery did
/// not happen.
pub(crate) fn invoke_app_emit(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let event = params.get("event").and_then(Value::as_str).ok_or_else(|| {
        NcError::new(ErrorKind::Validation {
            field: "event".to_owned(),
            reason: "`event` must be a non-empty string of at most 256 bytes".to_owned(),
        })
        .operation("dispatch::app_emit")
    })?;
    let payload = params.get("payload").cloned().unwrap_or(Value::Null);
    match params.get("window").and_then(Value::as_str) {
        None | Some("all") => {
            let delivered = runtime.emit_all(event, &payload)?;
            serde_json::to_value(json!({ "emitted": delivered, "event": event }))
                .map_err(|e| json_error(&e))
        }
        _ => {
            let id = window_id_param(params)?;
            runtime.emit(id, event, &payload)?;
            serde_json::to_value(json!({
                "emitted": 1,
                "event": event,
                "window": id.to_string(),
            }))
            .map_err(|e| json_error(&e))
        }
    }
}

/// Reads the `consent_token` parameter, when present.
///
/// `None` means the caller supplied no token. A present-but-malformed token is a
/// refusal, not an absence: reading corruption as "no consent" would tell the
/// caller nothing about why its token was ignored. The legacy `consent` field
/// authorizes nothing and is excluded from the operation's parameter binding.
pub(crate) fn consent_token_param(params: &Value) -> Result<Option<ConsentToken>> {
    let Some(value) = params.get("consent_token") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    ConsentToken::from_value(value)
        .map(Some)
        .map_err(|mut error| {
            error = error.operation("dispatch::consent_token");
            error
        })
}

/// Answers a log query, filtered by level and correlation.
///
/// The filter is applied before the limit, so a limit of ten returns ten *matching*
/// entries rather than ten entries of which some matched. A limit that counted the wrong
/// population would make the number an answer to a different question.
#[allow(
    clippy::too_many_lines,
    reason = "one query reads four filters, a limit, a cursor and the ring; splitting it would hide the ordering"
)]
pub(crate) fn invoke_logs_query(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let limit = bounded_limit(params)?;
    let level = log_level_filter(params)?;
    let correlation = correlation_filter(params)?;
    let cursor = cursor_param(params)?;
    let matched: Vec<_> = runtime
        .logs
        .to_vec()
        .into_iter()
        .filter(|entry| level.matches(entry))
        .filter(|entry| correlation.matches(entry))
        .collect();
    // Newest first, so an agent reads what just happened before what happened earlier.
    let newest_first: Vec<_> = matched.into_iter().rev().collect();
    let matched_count = newest_first.len();
    let offset = cursor.unwrap_or(0).min(matched_count);
    let end = (offset + limit).min(matched_count);
    let slice: Vec<_> = newest_first[offset..end].to_vec();
    let next_cursor = if end < matched_count {
        Some(end.to_string())
    } else {
        None
    };
    let truncated = next_cursor.is_some();
    serde_json::to_value(json!({
        "entries": slice,
        "truncated": truncated,
        "ring_capacity": runtime.logs.capacity(),
        "matched": matched_count,
        "next_cursor": next_cursor,
    }))
    .map_err(|e| json_error(&e))
}

#[cfg(test)]
mod child_param_tests {
    use super::{
        child_bounds_param, child_id_param, invoke_window_child_create, validate_child_request,
    };
    use crate::Runtime;
    use ncui_core::{ErrorKind, Limits};
    use serde_json::json;

    const OPERATION: &str = "dispatch::child_param_test";
    const NO_WINDOW: &str = "00000000000000000000000000000009";

    #[test]
    fn child_geometry_is_required_and_cannot_wrap_or_truncate() {
        for params in [
            json!({ "x": 0, "y": 0, "width": 100 }),
            json!({ "x": -1, "y": 0, "width": 100, "height": 100 }),
            json!({ "x": 0, "y": 0, "width": -1, "height": 100 }),
            json!({ "x": 0, "y": 0, "width": 4_294_967_296_u64, "height": 100 }),
            json!({ "x": 0, "y": 0, "width": 100.5, "height": 100 }),
        ] {
            let error = match child_bounds_param(&params, OPERATION) {
                Ok(request) => {
                    validate_child_request(&request, OPERATION).expect_err("invalid geometry")
                }
                Err(error) => error,
            };
            assert!(
                matches!(error.kind(), ErrorKind::Validation { .. }),
                "{error:?}"
            );
        }
    }

    #[test]
    fn child_geometry_rules_are_applied_before_the_engine_is_asked() {
        let request = child_bounds_param(
            &json!({ "x": 0, "y": 0, "width": 0, "height": 100 }),
            OPERATION,
        )
        .expect("numeric geometry parses");
        let error = validate_child_request(&request, OPERATION).expect_err("zero extent");
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn child_creation_rejects_an_empty_url_before_window_lookup() {
        let mut runtime = Runtime::start(Limits::default()).expect("runtime");
        let error = invoke_window_child_create(
            &mut runtime,
            &json!({
                "window": NO_WINDOW,
                "url": "",
                "x": 0,
                "y": 0,
                "width": 100,
                "height": 100
            }),
        )
        .expect_err("empty child url");
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn child_ids_must_use_the_canonical_hex_wire_form() {
        let parsed = child_id_param(&json!({ "child": "child-0000000000000001" }), OPERATION)
            .expect("canonical id");
        assert_eq!(parsed.raw(), 1);
        for malformed in ["1", "child-1", "child-000000000000000g"] {
            assert!(
                child_id_param(&json!({ "child": malformed }), OPERATION).is_err(),
                "{malformed}"
            );
        }
    }
}

#[cfg(test)]
mod capture_tests {
    use super::{base64_decoded_len, invoke_window_print_to_pdf, invoke_window_screenshot};
    use crate::Runtime;
    use ncui_core::{ErrorKind, Limits};
    use serde_json::json;

    fn runtime() -> Runtime {
        Runtime::start(Limits::default()).expect("runtime")
    }

    /// A well-formed window identity no runtime has open.
    const NO_WINDOW: &str = "00000000000000000000000000000009";

    #[test]
    fn base64_length_matches_the_bytes_the_payload_decodes_to() {
        assert_eq!(base64_decoded_len(""), 0);
        assert_eq!(base64_decoded_len("QQ=="), 1);
        assert_eq!(base64_decoded_len("QUI="), 2);
        assert_eq!(base64_decoded_len("QUJD"), 3);
        assert_eq!(base64_decoded_len("QUJDRA=="), 4);
    }

    #[test]
    fn print_to_pdf_refuses_a_scale_outside_the_engine_range() {
        let mut rt = runtime();
        let error =
            invoke_window_print_to_pdf(&mut rt, &json!({ "window": NO_WINDOW, "scale": 5.0 }))
                .expect_err("scale is refused");
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn print_to_pdf_refuses_an_out_of_range_paper_size() {
        let mut rt = runtime();
        let error = invoke_window_print_to_pdf(
            &mut rt,
            &json!({ "window": NO_WINDOW, "paper_width": 500.0 }),
        )
        .expect_err("paper width is refused");
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn print_to_pdf_names_a_window_that_is_not_open() {
        let mut rt = runtime();
        let error = invoke_window_print_to_pdf(&mut rt, &json!({ "window": NO_WINDOW }))
            .expect_err("no window is open");
        assert!(
            matches!(error.kind(), ErrorKind::NotFound { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn screenshot_refuses_a_format_the_engine_does_not_offer() {
        let mut rt = runtime();
        let error =
            invoke_window_screenshot(&mut rt, &json!({ "window": NO_WINDOW, "format": "webp" }))
                .expect_err("webp is refused");
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn screenshot_refuses_a_quality_outside_the_engine_range() {
        let mut rt = runtime();
        let error = invoke_window_screenshot(
            &mut rt,
            &json!({ "window": NO_WINDOW, "format": "jpeg", "quality": 101 }),
        )
        .expect_err("quality is refused");
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn screenshot_names_a_window_that_is_not_open() {
        let mut rt = runtime();
        let error = invoke_window_screenshot(&mut rt, &json!({ "window": NO_WINDOW }))
            .expect_err("no window is open");
        assert!(
            matches!(error.kind(), ErrorKind::NotFound { .. }),
            "{error:?}"
        );
    }
}

#[cfg(test)]
mod native_param_tests {
    use super::window_request_param;
    use ncui_core::ErrorKind;
    use serde_json::json;

    #[test]
    fn a_native_window_request_parses_through_the_same_path_without_a_url() {
        let request = window_request_param(
            &json!({
                "content": { "kind": "native", "provider": "test.panel", "props": { "rows": 3 } },
                "title": "Panel",
                "width": 400,
                "height": 300
            }),
            None,
        )
        .expect("a well-formed native request passes validation");
        assert!(request.content.is_native());
        assert!(request.url.is_empty(), "a native window carries no URL");
        assert_eq!(request.title, "Panel");
        assert_eq!(request.width, 400);
    }

    #[test]
    fn a_native_window_request_carrying_a_url_is_rejected() {
        let error = window_request_param(
            &json!({
                "url": "https://example.com",
                "content": { "kind": "native", "provider": "test.panel" }
            }),
            None,
        )
        .expect_err("a native window must not name a URL");
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_page_window_request_still_requires_a_url() {
        for params in [
            json!({}),
            json!({ "content": { "kind": "page" } }),
            json!({ "url": "" }),
        ] {
            let error = window_request_param(&params, None)
                .expect_err("a page window without a URL is refused");
            assert!(
                matches!(error.kind(), ErrorKind::Validation { .. }),
                "{params}: {error:?}"
            );
        }
    }

    #[test]
    fn a_bad_provider_is_rejected_at_validation() {
        for provider in [
            String::new(),
            "Uppercase".to_owned(),
            "has space".to_owned(),
            "a".repeat(129),
        ] {
            let error = window_request_param(
                &json!({ "content": { "kind": "native", "provider": provider } }),
                None,
            )
            .expect_err("a provider outside the contract is refused");
            assert!(
                matches!(error.kind(), ErrorKind::Validation { .. }),
                "{provider}: {error:?}"
            );
        }
    }

    #[test]
    fn props_past_the_contracts_bound_are_rejected_at_validation() {
        let error = window_request_param(
            &json!({
                "content": {
                    "kind": "native",
                    "provider": "test.panel",
                    "props": { "data": "x".repeat(70_000) }
                }
            }),
            None,
        )
        .expect_err("props past the bound are refused");
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_content_that_is_not_the_contracts_shape_is_rejected() {
        for content in [
            json!("native"),
            json!({ "kind": "dance" }),
            json!({ "kind": "native" }),
            json!({ "provider": "test.panel" }),
        ] {
            let error = window_request_param(
                &json!({ "url": "https://example.com", "content": content }),
                None,
            )
            .expect_err("content outside the contract is refused");
            assert!(
                matches!(error.kind(), ErrorKind::Validation { .. }),
                "{content}: {error:?}"
            );
        }
    }
}
