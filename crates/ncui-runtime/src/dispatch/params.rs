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
//! several hundred lines of field validation.

use core::time::Duration;
use std::time::Instant;

use ncui_core::error::validation;
use ncui_core::{Capability, CapabilitySet, CorrelationId, ErrorKind, NcError, Result, WindowId};
use ncui_observability::logring::{LogLevel, LogRecord};
use ncui_security::authz::ConsentToken;
use ncui_webview::{MAX_WINDOWS, WindowRejection};
use serde_json::{Value, json};

use crate::Runtime;

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

pub(crate) fn window_request_param(
    params: &Value,
    remembered_place: Option<ncui_webview::WindowGeometry>,
) -> Result<ncui_webview::WindowRequest> {
    let url = url_param(params)?;
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

    let mut request = ncui_webview::WindowRequest::new(url);
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

/// Reads the `consent_token` parameter, when present.
///
/// `None` means the caller supplied no token. A present-but-malformed token is a
/// refusal, not an absence: reading corruption as "no consent" would tell the
/// caller nothing about why its token was ignored. The legacy `consent: true`
/// bool is accepted as a value and ignored here — it authorizes nothing
/// destructive — because removing the field outright would break clients that
/// still send it before they migrate to `consent_token`.
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
