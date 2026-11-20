//! Method registry and the request pipeline.
//!
//! [`dispatch`] is the single entry point for every agent request, and the order of its
//! steps is a security property rather than an implementation detail:
//!
//! 1. **Rate limit** — before any work, so a flooding client cannot make the runtime do
//!    work it will discard. It runs after authentication (in the caller) so an
//!    unauthenticated peer cannot exhaust a real agent's budget.
//! 2. **Authorize** — before dispatch, so a denied method never runs a handler.
//! 3. **Dispatch** — the handler.
//! 4. **Audit** — unconditionally, including on failure. A request that was refused is
//!    precisely the one an incident review wants to see.
//!
//! The clock is passed in rather than read, so rate limiting is testable without sleeping
//! and without wall-clock assertions.

use core::time::Duration;
use std::time::{Instant, SystemTime};

use ncui_core::error::validation;
use ncui_core::{Capability, CapabilitySet, CorrelationId, ErrorKind, NcError, Result};
use ncui_security::audit::AuditEvent;
use ncui_security::audit::AuditOutcome;
use ncui_security::authz::Authorizer;
use serde_json::{Value, json};

use crate::Runtime;
use crate::agent::AgentSession;
use crate::describe::{MethodDescription, describe};
use self::cargo::{invoke_benchmark_run, invoke_test_run};
use crate::health;
use crate::rpc::{Request, Response};

pub(crate) use self::params::json_error;
mod cargo;
mod params;

use self::params::{
    consent_token_param,
    deadline_param,
    enforce_deadline,
    grant_param,
    invoke_component_inspect,
    invoke_devtools_events,
    invoke_devtools_profile,
    invoke_app_emit,
    invoke_devtools_tree,
    invoke_logs_query,
    invoke_state_inspect,
    invoke_window_drag_region,
    invoke_window_post,
    remembered_place,
    window_change_param,
    window_id_param,
    window_rejection,
    window_request_param,
    window_shape_param,
};

/// The methods this runtime implements, with the authority each one needs.
///
/// The table is the single source of truth: `ncui.describe` reports it, and dispatch
/// enforces it. Keeping them in one place is what stops the advertised surface and the
/// enforced surface from drifting apart.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "the method table is the single source of truth; splitting it would obscure the surface"
)]
pub fn method_table() -> Vec<MethodDescription> {
    let entry =
        |name: &str, summary: &str, requires: CapabilitySet, destructive: bool| MethodDescription {
            name: name.to_owned(),
            summary: summary.to_owned(),
            requires,
            destructive,
        };
    let inspect = CapabilitySet::empty().with(Capability::InspectRuntime);
    vec![
        entry(
            "ncui.describe",
            "Runtime identity, methods, roles and capabilities.",
            CapabilitySet::empty(),
            false,
        ),
        entry(
            "ncui.health",
            "Liveness, readiness and per-subsystem health.",
            CapabilitySet::empty(),
            false,
        ),
        entry(
            "ncui.feature.status",
            "Which features are complete and which are absent.",
            CapabilitySet::empty(),
            false,
        ),
        entry(
            "ncui.workers.inspect",
            "Worker lifecycle, queue depth and task counters.",
            inspect,
            false,
        ),
        entry(
            "ncui.scheduler.inspect",
            "Ready-queue depth and cooperative budget.",
            inspect,
            false,
        ),
        entry(
            "ncui.logs.query",
            "Bounded log ring, filtered by level and correlation.",
            CapabilitySet::empty().with(Capability::ReadLogs),
            false,
        ),
        entry(
            "ncui.metrics.query",
            "Metric snapshot including percentiles.",
            CapabilitySet::empty().with(Capability::ReadMetrics),
            false,
        ),
        entry(
            "ncui.logs.purge",
            "Discard the in-memory log ring. Irreversible, so it requires per-call consent.",
            CapabilitySet::empty().with(Capability::ReadLogs),
            true,
        ),
        entry(
            "ncui.window.list",
            "Every open window, with its lifecycle state and page grant.",
            CapabilitySet::empty().with(Capability::InspectRuntime),
            false,
        ),
        entry(
            "ncui.window.create",
            "Open a window on a URL. Requires a host to be installed.",
            CapabilitySet::empty().with(Capability::Window),
            false,
        ),
        entry(
            "ncui.window.post",
            "Evaluate a script in a window's page. The host-to-page half of the bridge.",
            CapabilitySet::empty().with(Capability::Window),
            false,
        ),
        entry(
            "ncui.window.drag.region",
            "Initiate a window drag from a drag region (titlebar). Called by the page when the user mousedowns on a [data-ncui-drag-region] element.",
            CapabilitySet::empty().with(Capability::Window),
            false,
        ),
        entry(
            "ncui.window.state",
            "How a window is presented, and how to change it: fullscreen, maximised, minimised or focused.",
            CapabilitySet::empty().with(Capability::Window),
            false,
        ),
        entry(
            "ncui.window.theme",
            "Whether the platform is drawing its interface light or dark.",
            CapabilitySet::empty().with(Capability::Window),
            false,
        ),
        entry(
            "ncui.window.shape",
            "Change a window's title, place, size, always-on-top or resizability.",
            CapabilitySet::empty().with(Capability::Window),
            false,
        ),
        entry(
            "ncui.window.close",
            "Close a window by identity.",
            CapabilitySet::empty().with(Capability::Window),
            false,
        ),
        entry(
            "ncui.components.list",
            "Every component declared by mounted windows' manifests.",
            inspect,
            false,
        ),
        entry(
            "ncui.gpu.inspect",
            "GPU islands, their render modes, the input contract they demonstrate, and the device's health.",
            CapabilitySet::empty().with(Capability::InspectGpu),
            false,
        ),
        entry(
            "ncui.capabilities.describe",
            "Every capability, grouped by scope.",
            CapabilitySet::empty(),
            false,
        ),
        entry(
            "ncui.component.inspect",
            "One mounted component, by identity, with its window and grant.",
            inspect,
            false,
        ),
        entry(
            "ncui.state.inspect",
            "A snapshot of the runtime's observable state.",
            inspect,
            false,
        ),
        entry(
            "ncui.trace.start",
            "Begin verbose tracing of operations.",
            inspect,
            false,
        ),
        entry(
            "ncui.trace.stop",
            "End verbose tracing of operations.",
            inspect,
            false,
        ),
        entry(
            "ncui.profile.start",
            "Begin profiling: record per-method timing.",
            inspect,
            false,
        ),
        entry("ncui.profile.stop", "End profiling.", inspect, false),
        entry(
            "ncui.test.run",
            "Run the project's test suite.",
            CapabilitySet::empty().with(Capability::RunTests),
            false,
        ),
        entry(
            "ncui.benchmark.run",
            "Run the project's benchmark suite.",
            CapabilitySet::empty().with(Capability::RunBenchmarks),
            false,
        ),
        entry(
            "ncui.ipc.inspect",
            "The IPC protocol version and frame limits.",
            inspect,
            false,
        ),
        entry(
            "ncui.app.emit",
            "Emit an application-defined event to a window's page. The host-to-page event delivery path for deep links, notification clicks, and custom application events.",
            CapabilitySet::empty().with(Capability::Window),
            false,
        ),
        entry(
            "ncui.devtools.tree",
            "Live snapshot of the window tree: every open window, its state, and its capability grant.",
            inspect,
            false,
        ),
        entry(
            "ncui.devtools.events",
            "Recent runtime events (navigations, capability denials, window state changes), filterable by kind.",
            inspect,
            false,
        ),
        entry(
            "ncui.devtools.profile",
            "Per-method timing histograms recorded while profiling was active.",
            inspect,
            false,
        ),
        entry(
            "ncui.devtools.profile.reset",
            "Clear all recorded profiling data.",
            inspect,
            false,
        ),
        entry(
            "ncui.devtools.summary",
            "A summary of the devtools subsystem: event count, capacity, profile methods, and call count.",
            inspect,
            false,
        ),
    ]
}

/// The method table, built once and shared.
///
/// The table is pure: it depends on nothing at run time. Building it per call allocated a
/// hundred strings for a page's every bridge call, which is a cost the hot path should not
/// pay. The slice is borrowed rather than cloned, so a caller that needs an owned table can
/// still call [`method_table`].
pub(crate) fn method_table_cached() -> &'static [MethodDescription] {
    static TABLE: std::sync::OnceLock<Vec<MethodDescription>> = std::sync::OnceLock::new();
    TABLE.get_or_init(method_table)
}

/// Handles one request against the built-in method table.
///
/// Returns `None` for a notification, which by definition expects no response.
#[must_use]
pub fn dispatch(
    runtime: &mut Runtime,
    session: &mut AgentSession,
    request: &Request,
    now: Duration,
    feature_registry: Option<&Value>,
) -> Option<Response> {
    let table = method_table();
    dispatch_with(runtime, session, request, now, feature_registry, &table)
}

/// Handles one request against an explicit method table.
///
/// Taking the table as data rather than reading a constant is what makes the authorization
/// gate testable end to end: a table can be built with a capability no role below
/// Administrator holds, and the denial path is exercised for real rather than asserted in
/// the abstract. It also lets an embedder extend the surface without forking dispatch.
///
/// Returns `None` for a notification, which by definition expects no response.
#[must_use]
pub fn dispatch_with(
    runtime: &mut Runtime,
    session: &mut AgentSession,
    request: &Request,
    now: Duration,
    feature_registry: Option<&Value>,
    table: &[MethodDescription],
) -> Option<Response> {
    let id = request.id.clone().unwrap_or(Value::Null);
    if request.is_notification() {
        // A notification still goes through authorization and auditing; it simply gets no
        // response. Skipping the pipeline here would make notifications a bypass.
        let _ = run_pipeline(runtime, session, request, now, feature_registry, table);
        return None;
    }
    Some(
        match run_pipeline(runtime, session, request, now, feature_registry, table) {
            Ok(value) => Response::success(id, value),
            Err(error) => Response::from_error(id, &error),
        },
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "pipeline order is the security property; consent block splits helpers out of it"
)]
fn run_pipeline(
    runtime: &mut Runtime,
    session: &mut AgentSession,
    request: &Request,
    now: Duration,
    feature_registry: Option<&Value>,
    table: &[MethodDescription],
) -> Result<Value> {
    let correlation = CorrelationId::new();

    // The request shape the specification states: `params` is an object or absent. A positional list
    // is refused rather than read as "no parameters" — a caller that sent one would otherwise be told
    // nothing about why its arguments were ignored, which is the silent-clamp mistake the same
    // document refuses to make about `limit`.
    if !request.params.is_null() && !request.params.is_object() {
        return Err(validation("params", "must be an object, or absent")
            .operation("dispatch::params")
            .correlated(correlation));
    }

    let method = table
        .iter()
        .find(|m| m.name == request.method)
        .ok_or_else(|| {
            NcError::new(ErrorKind::NotFound {
                kind: "method".to_owned(),
                id: request.method.clone(),
            })
            .operation("dispatch::lookup")
            .correlated(correlation)
        })?;

    let deadline = deadline_param(&request.params)?;

    let consent_token = consent_token_param(&request.params)?;

    let started = Instant::now();
    let outcome = (|| -> Result<Value> {
        session.rate_limit(now, 1)?;
        session.authorize(method.requires)?;
        if method.destructive {
            // A destructive method requires a minted token: a bare `consent: true`
            // authorizes nothing. The token is bound to the correlation, method, and
            // capability recorded at mint time; validation checks the presented
            // token against that recorded binding and the requested method, then
            // spends the nonce. The pipeline correlation (fresh per call, used for
            // audit and error correlation) is distinct from the token's
            // mint-time correlation: the token proves the user approved the
            // operation that was minted, not this particular dispatch's fresh id.
            let Some(token) = consent_token.as_ref() else {
                return Err(NcError::new(ErrorKind::CapabilityDenied {
                    capability: "consent_token".to_owned(),
                    subject: session.agent_id.clone(),
                })
                .operation("dispatch::consent")
                .correlated(correlation));
            };
            let bound_correlation = token.correlation();
            if !token.matches(bound_correlation, &request.method, method.requires) {
                return Err(NcError::new(ErrorKind::CapabilityDenied {
                    capability: "consent_token".to_owned(),
                    subject: session.agent_id.clone(),
                })
                .operation("dispatch::consent")
                .correlated(correlation));
            }
            // The pure predicate is checked first so a mismatched token is refused
            // even when an agent holds the capability; ledger spend is second.
            Authorizer.require_destructive_with_token(
                &session.agent_id,
                method.requires,
                session.granted,
                Some(token),
                bound_correlation,
                &request.method,
            )?;
            runtime.consume_consent(token, bound_correlation, &request.method, method.requires)?;
        } else if let Some(token) = consent_token.as_ref() {
            // A token on a non-destructive method is validated for shape but
            // spends nothing: there is nothing to consent to.
            let bound_correlation = token.correlation();
            Authorizer.require_destructive_with_token(
                &session.agent_id,
                method.requires,
                session.granted,
                Some(token),
                bound_correlation,
                &request.method,
            )?;
        }
        invoke(runtime, session, method, request, feature_registry)
    })();

    // Audit every attempt, successful or not. A refusal is the interesting event.
    let audit = AuditEvent::new(
        SystemTime::now(),
        &session.agent_id,
        correlation,
        &request.method,
        &request.method,
        if outcome.is_ok() {
            AuditOutcome::Allowed
        } else {
            AuditOutcome::Denied
        },
        method.requires,
    );
    if let Ok(event) = audit {
        runtime.audit.record(event);
    }

    // The caller's budget, reported rather than interrupted. See `enforce_deadline`.
    let outcome = enforce_deadline(
        outcome,
        started,
        deadline,
        &request.method,
        &runtime.metrics,
        correlation,
    );

    // Record profiling data when profiling is active.
    if runtime.profiling_active {
        let elapsed = started.elapsed();
        runtime.devtools.record_profile(&request.method, elapsed);
    }

    // Prefix the failing method onto the operation. An agent that fires several requests
    // and receives "capability `delete.data` is not granted" otherwise cannot tell which
    // of them was refused, which makes a denial far harder to act on than it needs to be.
    let method_name = method.name.clone();
    match outcome {
        Ok(value) => Ok(value),
        Err(error) => {
            let operation = format!("dispatch::{method_name}/{}", error.operation_name());
            Err(error.correlated(correlation).operation(operation))
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "dispatch is a single match over the method table; splitting handlers out of this module would hide the pipeline"
)]
fn invoke(
    runtime: &mut Runtime,
    session: &AgentSession,
    method: &MethodDescription,
    request: &Request,
    feature_registry: Option<&Value>,
) -> Result<Value> {
    match method.name.as_str() {
        "ncui.describe" => {
            let description = describe(session.granted, &method_table());
            serde_json::to_value(description).map_err(|e| json_error(&e))
        }
        "ncui.health" => {
            let deep = request
                .params
                .get("deep")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            serde_json::to_value(health::report(runtime, deep)).map_err(|e| json_error(&e))
        }
        "ncui.feature.status" => feature_registry.cloned().ok_or_else(|| {
            NcError::new(ErrorKind::NotFound {
                kind: "feature_registry".to_owned(),
                id: "protocols/feature-registry.json".to_owned(),
            })
            .operation("dispatch::feature_status")
        }),
        "ncui.workers.inspect" => {
            let mut snapshots = runtime.workers.snapshot();
            snapshots.sort_by_key(|w| w.id);
            serde_json::to_value(json!({
                "workers": snapshots,
                "queue_depth": runtime.workers.queue_depth(),
                "queue_high_water": runtime.workers.queue_high_water(),
            }))
            .map_err(|e| json_error(&e))
        }
        "ncui.scheduler.inspect" => serde_json::to_value(json!({
            "ready_depth": runtime.scheduler.depth(),
            "capacity": runtime.scheduler.capacity(),
        }))
        .map_err(|e| json_error(&e)),
        "ncui.logs.query" => invoke_logs_query(runtime, &request.params),
        "ncui.logs.purge" => {
            // The consent gate has already run; reaching here means the call was
            // authorized. Clearing the ring is the whole operation.
            let purged = runtime.logs.len();
            runtime.logs.clear();
            serde_json::to_value(json!({ "purged": purged })).map_err(|e| json_error(&e))
        }
        "ncui.window.list" => {
            let summaries = runtime.windows.summaries().map_err(window_rejection)?;
            serde_json::to_value(json!({ "windows": summaries })).map_err(|e| json_error(&e))
        }
        "ncui.window.create" => {
            let remembered = remembered_place(runtime, &request.params)?;
            let window_request = window_request_param(&request.params, remembered)?;
            let grant = grant_param(&request.params)?;
            // The host factory is installed on the window surface at startup; when none is
            // installed the registry reports the refusal rather than this handler inventing
            // a window.
            let id = runtime
                .windows
                .open(&window_request, grant)
                .map_err(window_rejection)?;
            // A window the page named is one whose place the runtime remembers from now on: the
            // name is the page's, because the page is what knows that two windows of one page are
            // two windows.
            let remembered_key = request.params.get("remember").and_then(Value::as_str);
            if let Some(key) = remembered_key {
                runtime.windows.remember_place(id, key);
                // A window that will be put back where it was left is visible: an application
                // whose named windows stopped being remembered shows as a count that stopped
                // rising.
                let _ = runtime.metrics.counter_inc(
                    "window.remembered",
                    &ncui_observability::metrics::Labels::new(),
                    1,
                );
            }
            // A window is visible: an operator can see how many were opened, and the URL
            // is in the trace rather than only in the caller's payload.
            let _ = runtime.metrics.counter_inc(
                "window.created",
                &ncui_observability::metrics::Labels::new(),
                1,
            );
            tracing::info!(
                window = %id,
                url = %window_request.url,
                remembered = ?remembered_key,
                "window opened"
            );
            serde_json::to_value(json!({ "id": id.to_string() })).map_err(|e| json_error(&e))
        }
        "ncui.window.close" => {
            let id = window_id_param(&request.params)?;
            let closed = runtime.windows.close(id).map_err(window_rejection)?;
            if closed {
                let _ = runtime.metrics.counter_inc(
                    "window.closed",
                    &ncui_observability::metrics::Labels::new(),
                    1,
                );
                tracing::info!(window = %id, "window closed");
                serde_json::to_value(json!({ "closed": true })).map_err(|e| json_error(&e))
            } else {
                Err(NcError::new(ErrorKind::NotFound {
                    kind: "window".to_owned(),
                    id: id.to_string(),
                })
                .operation("dispatch::window_close"))
            }
        }
        "ncui.window.theme" => {
            let id = window_id_param(&request.params)?;
            // A window whose engine cannot ask the platform is a refusal rather than a report of
            // "light": the second is a claim about the user's machine that nothing checked.
            let Some(theme) = runtime.windows.window_theme(id) else {
                return Err(NcError::new(ErrorKind::Conflict {
                    reason: format!(
                        "the engine for window {id} cannot report the platform's theme"
                    ),
                })
                .operation("dispatch::window_theme"));
            };
            tracing::debug!(
                window = %id,
                theme = theme.as_str(),
                "a page asked which theme the platform is drawing"
            );
            Ok(serde_json::json!({ "window": id, "theme": theme.as_str() }))
        }
        "ncui.window.state" => {
            let id = window_id_param(&request.params)?;
            let change = window_change_param(&request.params)?;
            let state = match change {
                Some(change) => runtime
                    .windows
                    .set_window_state(id, change)
                    .map_err(window_rejection)?,
                None => runtime.windows.window_state(id),
            };
            // A window whose engine cannot ask the platform is a refusal rather than a report of
            // "windowed, unfocused": the second is a claim about the screen that nothing checked.
            let Some(state) = state else {
                return Err(NcError::new(ErrorKind::Conflict {
                    reason: format!(
                        "the engine for window {id} cannot report how the window is presented"
                    ),
                })
                .operation("dispatch::window_state"));
            };
            if let Some(change) = change {
                let _ = runtime.metrics.counter_inc(
                    "window.state_changes",
                    &ncui_observability::metrics::Labels::new(),
                    1,
                );
                tracing::info!(
                    window = %id,
                    change = change.field(),
                    fullscreen = state.fullscreen,
                    maximized = state.maximized,
                    minimized = state.minimized,
                    "the window's presentation changed"
                );
            }
            serde_json::to_value(json!({
                "window": id.to_string(),
                "state": {
                    "fullscreen": state.fullscreen,
                    "maximized": state.maximized,
                    "minimized": state.minimized,
                    "focused": state.focused,
                }
            }))
            .map_err(|e| json_error(&e))
        }
        "ncui.window.shape" => {
            let id = window_id_param(&request.params)?;
            let shape = window_shape_param(&request.params)?;
            runtime
                .windows
                .set_window_shape(id, &shape)
                .map_err(window_rejection)?;
            let summary = runtime
                .windows
                .summaries()
                .map_err(window_rejection)?
                .into_iter()
                .find(|summary| summary.id == id)
                .ok_or_else(|| {
                    NcError::new(ErrorKind::NotFound {
                        kind: "window".to_owned(),
                        id: id.to_string(),
                    })
                    .operation("dispatch::window_shape")
                })?;
            let _ = runtime.metrics.counter_inc(
                "window.reshapes",
                &ncui_observability::metrics::Labels::new(),
                1,
            );
            tracing::info!(
                window = %id,
                title = %summary.title,
                width = summary.width,
                height = summary.height,
                "the window was reshaped"
            );
            serde_json::to_value(json!({
                "window": id.to_string(),
                "title": summary.title,
                "x": summary.x,
                "y": summary.y,
                "width": summary.width,
                "height": summary.height,
                "always_on_top": summary.always_on_top,
                "resizable": summary.resizable,
            }))
            .map_err(|e| json_error(&e))
        }
        "ncui.window.post" => invoke_window_post(runtime, &request.params),
        "ncui.window.drag.region" => invoke_window_drag_region(runtime),
        "ncui.components.list" => {
            let components = runtime.windows.manifests().map_err(window_rejection)?;
            serde_json::to_value(json!({ "components": components })).map_err(|e| json_error(&e))
        }
        "ncui.gpu.inspect" => Ok(runtime.gpu.inspect()),
        "ncui.capabilities.describe" => {
            use ncui_core::Capability;
            let mut by_scope: serde_json::Map<String, Value> = serde_json::Map::new();
            for cap in Capability::ALL {
                let scope = cap.scope().as_str().to_owned();
                let entry = by_scope.entry(scope).or_insert_with(|| json!([]));
                if let Some(arr) = entry.as_array_mut() {
                    arr.push(json!(cap.as_str()));
                }
            }
            serde_json::to_value(json!({ "capabilities": by_scope })).map_err(|e| json_error(&e))
        }
        "ncui.component.inspect" => invoke_component_inspect(runtime, &request.params),
        "ncui.state.inspect" => invoke_state_inspect(runtime),
        "ncui.trace.start" => {
            runtime.tracing_active = true;
            serde_json::to_value(json!({ "started": true, "active": runtime.tracing_active }))
                .map_err(|e| json_error(&e))
        }
        "ncui.trace.stop" => {
            runtime.tracing_active = false;
            serde_json::to_value(json!({ "stopped": true, "active": runtime.tracing_active }))
                .map_err(|e| json_error(&e))
        }
        "ncui.profile.start" => {
            runtime.profiling_active = true;
            serde_json::to_value(json!({ "started": true, "active": runtime.profiling_active }))
                .map_err(|e| json_error(&e))
        }
        "ncui.profile.stop" => {
            runtime.profiling_active = false;
            serde_json::to_value(json!({ "stopped": true, "active": runtime.profiling_active }))
                .map_err(|e| json_error(&e))
        }
        "ncui.test.run" => invoke_test_run(runtime, &request.params),
        "ncui.benchmark.run" => invoke_benchmark_run(runtime, &request.params),
        "ncui.ipc.inspect" => serde_json::to_value(json!({
            "protocol_version": ncui_ipc::PROTOCOL_VERSION,
            "frame_magic": "NCUI",
            "max_frame_bytes": runtime.limits.max_ipc_frame_bytes,
            "max_in_flight_requests": runtime.limits.max_in_flight_requests,
            "request_timeout_ms": runtime.limits.request_timeout_ms,
        }))
        .map_err(|e| json_error(&e)),
        "ncui.app.emit" => invoke_app_emit(runtime, &request.params),
        "ncui.devtools.tree" => {
            invoke_devtools_tree(runtime)
        }
        "ncui.devtools.events" => {
            invoke_devtools_events(runtime, &request.params)
        }
        "ncui.devtools.profile" => {
            invoke_devtools_profile(runtime)
        }
        "ncui.devtools.profile.reset" => {
            runtime.devtools.reset_profile();
            serde_json::to_value(json!({ "reset": true }))
                .map_err(|e| json_error(&e))
        }
        "ncui.devtools.summary" => {
            serde_json::to_value(runtime.devtools.summary())
                .map_err(|e| json_error(&e))
        }
        "ncui.metrics.query" => {
            let series = runtime.metrics.snapshot();
            serde_json::to_value(json!({
                "series": series,
                "evicted_total": runtime.metrics.evicted_total(),
            }))
            .map_err(|e| json_error(&e))
        }
        other => Err(NcError::new(ErrorKind::Protocol {
            reason: format!("method `{other}` is in the table but has no handler"),
        })
        .operation("dispatch::invoke")),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::Limits;
    use ncui_core::WindowId;
    use ncui_observability::logring::{LogLevel, LogRecord};
    use ncui_security::authz::{AgentRole, ConsentToken};

    fn runtime() -> Runtime {
        Runtime::start(Limits::default()).expect("runtime")
    }

    /// The credential every test session presents.
    fn test_token() -> ncui_security::token::SecretToken {
        ncui_security::token::SecretToken::new(vec![0x11; 32]).expect("token")
    }

    /// An authenticated session, which dispatch requires.
    fn session(role: AgentRole) -> AgentSession {
        let mut session = AgentSession::new("agent-test", role, 1000, 1000).expect("session");
        session
            .authenticate(&test_token(), &test_token())
            .expect("test session authenticates");
        session
    }

    fn call(
        runtime: &mut Runtime,
        session: &mut AgentSession,
        method: &str,
        params: Value,
    ) -> Response {
        let request = Request::new(json!(1), method, params);
        dispatch(runtime, session, &request, Duration::from_secs(0), None)
            .expect("a call is not a notification")
    }

    #[test]
    fn a_request_whose_params_are_not_an_object_is_refused() {
        // The specification's request shape, enforced rather than assumed: positional parameters are
        // not accepted, and a request that sent one is refused with the field named rather than read
        // as a request that named no parameters at all.
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        for params in [json!([1, 2]), json!("hello"), json!(7)] {
            let request = Request::new(json!(1), "ncui.health", params.clone());
            let response = dispatch(
                &mut runtime,
                &mut session,
                &request,
                Duration::from_secs(0),
                None,
            )
            .expect("a call is not a notification");
            let error = response.error.expect("a positional request is refused");
            assert_eq!(
                error.data.ncui_code, "validation",
                "{params} gave {error:?}"
            );
            assert!(error.message.contains("params"), "{error:?}");
        }

        // Absent and `null` are both "no parameters", which is a request the runtime serves.
        for params in [json!(null), json!({})] {
            let request = Request::new(json!(1), "ncui.health", params);
            let response = dispatch(
                &mut runtime,
                &mut session,
                &request,
                Duration::from_secs(0),
                None,
            )
            .expect("a call is not a notification");
            assert!(response.error.is_none(), "{:?}", response.error);
        }
    }

    #[test]
    fn a_call_that_runs_past_its_deadline_is_answered_as_having_timed_out() {
        // The budget is a report rather than an interruption: this pipeline is synchronous, so a
        // handler runs to completion and what the caller is owed is the truth about how long it
        // took. The enforcement is a pure function of the outcome, the start instant and the
        // budget, so it is exercised directly with a start instant already in the past. A real
        // long handler here would be a nested cargo build, which cannot run inside the suite
        // that is itself a cargo build: the outer cargo holds the target-directory lock the
        // nested one waits on.
        let metrics = ncui_observability::metrics::MetricsRegistry::new(Limits::default())
            .expect("metrics registry");
        let error = enforce_deadline(
            Ok(json!({ "ok": true })),
            Instant::now()
                .checked_sub(Duration::from_secs(10))
                .expect("ten seconds is in the past"),
            Some(Duration::from_millis(1)),
            "ncui.example",
            &metrics,
            CorrelationId::new(),
        )
        .expect_err("a call past its budget is not a success");
        assert!(
            matches!(error.kind(), ErrorKind::Timeout { .. }),
            "{error:?}"
        );
        assert_eq!(error.operation_name(), "dispatch::deadline", "{error:?}");
        let labels = ncui_observability::metrics::Labels::new();
        assert_eq!(
            metrics.counter_value("request.timeouts", &labels),
            Some(1),
            "a timeout must be visible rather than silent"
        );
    }

    #[test]
    fn a_deadline_that_is_not_a_budget_is_refused() {
        // Zero and a year are both numbers a caller typed by mistake, and neither is a budget: the
        // first would read as "no deadline" and the second as "no limit".
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        for deadline in [json!(0), json!(99_999_999), json!("soon"), json!(-1)] {
            let response = call(
                &mut runtime,
                &mut session,
                "ncui.health",
                json!({ "deadline_ms": deadline }),
            );
            let error = response
                .error
                .expect("a deadline that is not a budget is refused");
            assert_eq!(
                error.data.ncui_code, "validation",
                "{deadline} gave {error:?}"
            );
            assert!(error.message.contains("deadline_ms"), "{error:?}");
        }
    }

    #[test]
    fn a_call_within_its_deadline_is_not_touched() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        let response = call(
            &mut runtime,
            &mut session,
            "ncui.health",
            json!({ "deadline_ms": 3_600_000 }),
        );
        assert!(response.error.is_none(), "{:?}", response.error);
    }

    #[test]
    fn describe_works_for_any_authenticated_agent() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Observer);
        let response = call(&mut runtime, &mut session, "ncui.describe", json!({}));
        assert!(response.error.is_none(), "got: {:?}", response.error);
        let result = response.result.expect("result");
        assert_eq!(result["runtime_version"], crate::RUNTIME_VERSION);
        assert!(!result["methods"].as_array().expect("methods").is_empty());
    }

    #[test]
    fn health_reports_without_any_capability() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Observer);
        let response = call(
            &mut runtime,
            &mut session,
            "ncui.health",
            json!({ "deep": true }),
        );
        let result = response.result.expect("result");
        assert_eq!(result["status"], "healthy");
        assert_eq!(result["deep"], true);
    }

    #[test]
    fn an_unknown_method_is_reported_as_not_found() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        let response = call(&mut runtime, &mut session, "ncui.does_not_exist", json!({}));
        let error = response.error.expect("error");
        assert_eq!(error.data.ncui_code, "not_found");
        assert!(
            error.message.contains("ncui.does_not_exist"),
            "got: {}",
            error.message
        );
    }

    /// A method table holding one method gated on authority no non-administrator holds.
    fn gated_table() -> Vec<MethodDescription> {
        vec![MethodDescription {
            name: "ncui.data.purge".to_owned(),
            summary: "Permanently remove user data.".to_owned(),
            requires: CapabilitySet::empty().with(Capability::DeleteData),
            destructive: true,
        }]
    }

    #[test]
    fn a_capability_the_agent_lacks_is_denied_before_the_handler_runs() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Developer);
        let request = Request::new(json!(1), "ncui.data.purge", json!({}));
        let response = dispatch_with(
            &mut runtime,
            &mut session,
            &request,
            Duration::from_secs(0),
            None,
            &gated_table(),
        )
        .expect("response");

        let error = response.error.expect("error");
        assert_eq!(
            error.data.ncui_code, "capability_denied",
            "a Developer does not hold delete.data"
        );
        assert_eq!(error.code, -32001);
        assert!(
            error.message.contains("ncui.data.purge"),
            "the denial must name what was refused: {}",
            error.message
        );
    }

    #[test]
    fn every_role_can_reach_the_diagnostic_surface_when_consenting() {
        // The window methods require `Window`, which only Administrator holds; every other
        // role must be denied. A host is installed so the Administrator's window calls reach
        // their handlers rather than being refused for want of a factory.
        let registry = json!({"schema_version":1,"features":[]});
        for role in AgentRole::ALL {
            let mut runtime = runtime();
            if *role == AgentRole::Administrator {
                runtime.windows = runtime
                    .windows
                    .with_registry(ncui_webview::WindowRegistry::new().with_default_navigation(
                        ncui_webview::NavigationPolicy::new(&[ncui_webview::Origin::new(
                            ncui_webview::Scheme::Https,
                            "example.com",
                            None,
                        )]),
                    ))
                    .with_factory(Box::new(AlwaysReady));
            }
            let mut session = session(*role);
            let mut created_window: Option<WindowId> = None;
            for method in method_table() {
                if method.name == "ncui.test.run" || method.name == "ncui.benchmark.run" {
                    continue;
                }
                let params = if method.destructive {
                    let correlation = CorrelationId::new();
                    let token = runtime
                        .mint_consent(correlation, &method.name, method.requires)
                        .expect("mint");
                    json!({ "consent_token": token.to_value() })
                } else {
                    match method.name.as_str() {
                        "ncui.window.create" => json!({ "url": "https://example.com" }),
                        // Closing a window and asking about its theme take the same parameter and
                        // nothing else, so they are one arm.
                        "ncui.window.close" | "ncui.window.theme" => match created_window {
                            Some(id) => json!({ "window": id.to_string() }),
                            None => json!({ "window": "00000000000000000000000000000001" }),
                        },
                        "ncui.window.post" => match created_window {
                            Some(id) => json!({ "window": id.to_string(), "script": "1;" }),
                            None => {
                                json!({ "window": "00000000000000000000000000000001", "script": "1;" })
                            }
                        },
                        "ncui.app.emit" => match created_window {
                            Some(id) => json!({ "window": id.to_string(), "event": "test" }),
                            None => {
                                json!({ "window": "00000000000000000000000000000001", "event": "test" })
                            }
                        },
                        "ncui.window.state" => match created_window {
                            Some(id) => json!({ "window": id.to_string(), "maximized": true }),
                            None => json!({ "window": "00000000000000000000000000000001" }),
                        },
                        "ncui.window.shape" => match created_window {
                            Some(id) => json!({ "window": id.to_string(), "title": "renamed" }),
                            None => json!({ "window": "00000000000000000000000000000001" }),
                        },
                        _ => json!({ "consent": true }),
                    }
                };
                let request = Request::new(json!(1), &method.name, params);
                let response = dispatch_with(
                    &mut runtime,
                    &mut session,
                    &request,
                    Duration::from_secs(0),
                    Some(&registry),
                    &method_table(),
                )
                .expect("response");
                let authorized = method.requires.is_subset_of(session.granted);
                if authorized {
                    let ok = response.error.is_none();
                    // A window method whose handler needs a webview host is answered
                    // `not_found` when the role's runtime has none; that is a missing host,
                    // not a grant refusal.
                    let unavailable_without_a_host =
                        matches!(
                            method.name.as_str(),
                            "ncui.window.close" | "ncui.window.list"
                        ) && response.error.as_ref().map(|e| e.data.ncui_code.as_str())
                            == Some("not_found");
                    assert!(
                        ok || unavailable_without_a_host,
                        "role {role:?} holds the grant for {} but was refused: {:?}",
                        method.name,
                        response.error
                    );
                    if method.name == "ncui.window.create"
                        && let Some(id) = response
                            .result
                            .as_ref()
                            .and_then(|value| value.get("id").and_then(|value| value.as_str()))
                            .and_then(|text| text.parse::<WindowId>().ok())
                    {
                        created_window = Some(id);
                    }
                } else {
                    let error = response
                        .error
                        .expect("a role without the grant must be denied");
                    assert_eq!(error.data.ncui_code, "capability_denied");
                }
            }
        }
    }

    #[test]
    fn an_administrator_may_query_metrics() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        let response = call(&mut runtime, &mut session, "ncui.metrics.query", json!({}));
        assert!(response.error.is_none(), "got: {:?}", response.error);
        assert!(response.result.expect("result").get("series").is_some());
    }

    #[test]
    fn a_denied_request_is_still_audited() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Developer);
        let before = runtime.audit.len();
        let request = Request::new(json!(1), "ncui.data.purge", json!({}));
        let _ = dispatch_with(
            &mut runtime,
            &mut session,
            &request,
            Duration::from_secs(0),
            None,
            &gated_table(),
        );
        assert_eq!(
            runtime.audit.len(),
            before + 1,
            "a refused request must still produce an audit event"
        );
    }

    #[test]
    fn a_successful_request_is_also_audited() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        let before = runtime.audit.len();
        let _ = call(&mut runtime, &mut session, "ncui.health", json!({}));
        assert_eq!(runtime.audit.len(), before + 1);
    }

    #[test]
    fn a_notification_returns_no_response_but_is_still_audited() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        let before = runtime.audit.len();

        let request: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"ncui.health"}"#).expect("valid");
        let response = dispatch(
            &mut runtime,
            &mut session,
            &request,
            Duration::from_secs(0),
            None,
        );

        assert!(response.is_none(), "a notification must not be answered");
        assert_eq!(
            runtime.audit.len(),
            before + 1,
            "a notification must not bypass the audit trail"
        );
    }

    #[test]
    fn rate_limiting_is_enforced_across_calls() {
        let mut runtime = runtime();
        let mut session =
            AgentSession::new("agent-test", AgentRole::Administrator, 1, 2).expect("session");
        session
            .authenticate(&test_token(), &test_token())
            .expect("authenticates");
        let request = Request::new(json!(1), "ncui.health", json!({}));

        assert!(
            dispatch(
                &mut runtime,
                &mut session,
                &request,
                Duration::from_secs(0),
                None
            )
            .expect("response")
            .error
            .is_none()
        );
        assert!(
            dispatch(
                &mut runtime,
                &mut session,
                &request,
                Duration::from_secs(0),
                None
            )
            .expect("response")
            .error
            .is_none()
        );
        let refused = dispatch(
            &mut runtime,
            &mut session,
            &request,
            Duration::from_secs(0),
            None,
        )
        .expect("response");
        assert_eq!(
            refused.error.expect("error").data.ncui_code,
            "limit_exceeded"
        );
    }

    #[test]
    fn an_excessive_limit_is_refused_rather_than_silently_clamped() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        let response = call(
            &mut runtime,
            &mut session,
            "ncui.logs.query",
            json!({ "limit": 1_000_000u64 }),
        );
        let error = response.error.expect("error");
        assert_eq!(error.data.ncui_code, "validation");
        assert!(error.message.contains("10000"), "got: {}", error.message);
    }

    #[test]
    fn a_zero_limit_is_refused() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        let response = call(
            &mut runtime,
            &mut session,
            "ncui.logs.query",
            json!({ "limit": 0 }),
        );
        assert_eq!(response.error.expect("error").data.ncui_code, "validation");
    }

    #[test]
    fn every_error_carries_the_request_correlation_id() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Developer);
        let request = Request::new(json!(1), "ncui.data.purge", json!({}));
        let response = dispatch_with(
            &mut runtime,
            &mut session,
            &request,
            Duration::from_secs(0),
            None,
            &gated_table(),
        )
        .expect("response");
        let correlation = response
            .error
            .expect("error")
            .data
            .correlation
            .expect("a correlated error must carry a correlation id");
        assert_eq!(
            correlation.len(),
            32,
            "a correlation id is 32 hex characters"
        );
    }

    #[test]
    fn the_feature_status_method_needs_a_registry_document() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Observer);
        let response = call(&mut runtime, &mut session, "ncui.feature.status", json!({}));
        assert_eq!(
            response.error.expect("error").data.ncui_code,
            "not_found",
            "without a loaded registry the method must say so rather than invent an answer"
        );

        let registry = json!({ "schema_version": 1, "features": [] });
        let request = Request::new(json!(2), "ncui.feature.status", json!({}));
        let response = dispatch(
            &mut runtime,
            &mut session,
            &request,
            Duration::from_secs(0),
            Some(&registry),
        )
        .expect("response");
        assert!(response.error.is_none(), "got: {:?}", response.error);
        assert_eq!(response.result.expect("result")["schema_version"], 1);
    }

    #[test]
    fn every_advertised_method_has_a_handler() {
        let mut runtime = runtime();
        // Install a host so the window methods reach their handlers instead of being refused
        // for want of a factory; a registry with no windows makes `window.list` succeed empty.
        runtime.windows = runtime
            .windows
            .with_registry(ncui_webview::WindowRegistry::new().with_default_navigation(
                ncui_webview::NavigationPolicy::new(&[ncui_webview::Origin::new(
                    ncui_webview::Scheme::Https,
                    "example.com",
                    None,
                )]),
            ))
            .with_factory(Box::new(AlwaysReady));
        let mut session = session(AgentRole::Administrator);
        let registry = json!({ "schema_version": 1, "features": [] });
        // `ncui.window.create` is listed before `ncui.window.close`, so its id can be reused.
        let mut created_window: Option<WindowId> = None;
        for method in method_table() {
            if method.name == "ncui.test.run" || method.name == "ncui.benchmark.run" {
                // These two spawn a nested cargo build. Running them from inside the suite
                // would wait on the target-directory lock the outer cargo holds, so they are
                // covered by `the_nested_build_methods_are_guarded` instead.
                continue;
            }
            let params = if method.destructive {
                let correlation = CorrelationId::new();
                let token = runtime
                    .mint_consent(correlation, &method.name, method.requires)
                    .expect("mint");
                json!({ "consent_token": token.to_value() })
            } else {
                match method.name.as_str() {
                    "ncui.window.create" => json!({ "url": "https://example.com" }),
                    "ncui.window.close" => json!({
                        "window": created_window
                            .expect("create is listed before close")
                            .to_string()
                    }),
                    "ncui.window.post" => json!({
                        "window": created_window
                            .expect("create is listed before post")
                            .to_string(),
                        "script": "window.ncui.receive({});",
                    }),
                    "ncui.app.emit" => json!({
                        "window": created_window
                            .expect("create is listed before emit")
                            .to_string(),
                        "event": "test",
                    }),
                    "ncui.window.theme" => json!({
                        "window": created_window
                            .expect("create is listed before theme")
                            .to_string(),
                    }),
                    "ncui.window.state" => json!({
                        "window": created_window
                            .expect("create is listed before state")
                            .to_string(),
                        "maximized": true,
                    }),
                    "ncui.window.shape" => json!({
                        "window": created_window
                            .expect("create is listed before shape")
                            .to_string(),
                        "title": "renamed",
                    }),
                    "ncui.test.run" => {
                        json!({ "package": "ncui-core", "filter": "ids_round_trip", "consent": true })
                    }
                    "ncui.benchmark.run" => {
                        json!({ "package": "ncui-bench", "filter": "core_operations", "consent": true })
                    }
                    _ => json!({ "deep": true, "consent": true }),
                }
            };
            let request = Request::new(json!(1), &method.name, params);
            let response = dispatch(
                &mut runtime,
                &mut session,
                &request,
                Duration::from_secs(0),
                Some(&registry),
            )
            .expect("response");
            if method.name == "ncui.window.create"
                && let Some(id) = response
                    .result
                    .as_ref()
                    .and_then(|value| value.get("id").and_then(|value| value.as_str()))
                    .and_then(|text| text.parse::<WindowId>().ok())
            {
                created_window = Some(id);
            }
            assert!(
                response.error.is_none(),
                "advertised method `{}` failed: {:?}",
                method.name,
                response.error
            );
        }
    }

    #[test]
    fn every_seam_action_is_authorized_by_the_method_it_maps_to() {
        // A page action is offered only when the page's grant covers the seam's `requires`;
        // the method it maps to must accept the same capability, or the action is offered
        // to a page that is then refused. The two tables are maintained separately, so the
        // agreement is asserted rather than assumed.
        let table = method_table();
        for action in crate::window::SEAM_ACTIONS {
            let method = table
                .iter()
                .find(|method| method.name == action.method)
                .expect("the seam action's method exists");
            assert_eq!(
                method.requires, action.requires,
                "seam action `{}` is offered with `{:?}` but `{}` authorizes `{:?}`",
                action.name, action.requires, action.method, method.requires
            );
        }
    }

    #[test]
    fn the_nested_build_methods_are_guarded() {
        // `ncui.test.run` and `ncui.benchmark.run` spawn a nested cargo build. The suite
        // cannot run them: the outer cargo holds the target-directory lock the nested one
        // waits on. What the suite can prove is that both are routed to their handler and
        // that the handler refuses them when the runtime was not started with the opt-in.
        let mut runtime = runtime();
        runtime.bench_allowed = false;
        let mut session = session(AgentRole::Administrator);
        for method in ["ncui.test.run", "ncui.benchmark.run"] {
            let response = call(
                &mut runtime,
                &mut session,
                method,
                json!({ "consent": true }),
            );
            let error = response.error.expect("the guard refuses a nested build");
            assert_eq!(error.data.ncui_code, "validation", "{method}: {error:?}");
            assert!(
                error.message.contains("NCUI_RUN_BENCH"),
                "{method}: {error:?}"
            );
        }
    }

    #[test]
    fn a_log_query_pages_with_cursor() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        // Push enough records to span two pages.
        for i in 0..5u32 {
            runtime.logs.push(LogRecord::new(
                0,
                LogLevel::Info,
                "test",
                "op",
                format!("record {i}"),
            ));
        }
        let first = call(
            &mut runtime,
            &mut session,
            "ncui.logs.query",
            json!({ "limit": 2 }),
        );
        let first_result = first.result.expect("result");
        assert_eq!(
            first_result["entries"].as_array().expect("entries").len(),
            2
        );
        assert_eq!(first_result["truncated"], true);
        let cursor = first_result["next_cursor"]
            .as_str()
            .expect("next_cursor")
            .to_owned();
        assert!(!cursor.is_empty());

        let second = call(
            &mut runtime,
            &mut session,
            "ncui.logs.query",
            json!({ "limit": 2, "cursor": cursor }),
        );
        let second_result = second.result.expect("result");
        assert_eq!(
            second_result["entries"].as_array().expect("entries").len(),
            2
        );
        // The two pages must not overlap.
        let first_ids: Vec<_> = first_result["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["message"].clone())
            .collect();
        let second_ids: Vec<_> = second_result["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["message"].clone())
            .collect();
        for id in &first_ids {
            assert!(!second_ids.contains(id), "pages overlapped");
        }
    }

    /// A test factory whose hosts always come up alive.
    #[derive(Debug, Default)]
    struct AlwaysReady;

    impl ncui_webview::HostFactory for AlwaysReady {
        fn spawn(
            &self,
            _window: WindowId,
            _request: &ncui_webview::WindowRequest,
            _seam: &std::sync::Arc<ncui_webview::HostSeam>,
        ) -> Option<Box<dyn ncui_webview::WebViewBackend>> {
            Some(Box::new(AlwaysAlive))
        }
    }

    #[derive(Debug)]
    struct AlwaysAlive;

    impl ncui_webview::WebViewBackend for AlwaysAlive {
        fn window_theme(&self) -> Option<ncui_webview::Theme> {
            Some(ncui_webview::Theme::Light)
        }

        fn last_error(&self) -> Option<String> {
            None
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn poll_page_message(&self) -> Option<ncui_webview::PageMessage> {
            None
        }
        fn evaluate_script(&self, _script: &str) -> bool {
            true
        }
        // A real engine asks the platform; this one answers the way a windowed, unfocused window
        // does, so a method that reports the presentation reaches its handler rather than being
        // refused for an engine that cannot speak.
        fn window_state(&self) -> Option<ncui_webview::WindowState> {
            Some(ncui_webview::WindowState::default())
        }
        fn set_window_state(
            &self,
            _change: ncui_webview::WindowChange,
        ) -> Option<ncui_webview::WindowState> {
            Some(ncui_webview::WindowState::default())
        }
        fn set_window_shape(&self, _shape: &ncui_webview::WindowShape) -> bool {
            true
        }
    }

    #[test]
    fn a_destructive_call_with_only_consent_true_is_refused() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        let response = call(
            &mut runtime,
            &mut session,
            "ncui.logs.purge",
            json!({ "consent": true }),
        );
        let error = response.error.expect("a bare consent must be refused");
        assert_eq!(error.data.ncui_code, "capability_denied");
        assert!(error.message.contains("consent_token"));
    }

    #[test]
    fn a_token_reused_for_a_different_method_is_refused() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        let correlation = CorrelationId::new();
        let token = runtime
            .mint_consent(
                correlation,
                "ncui.logs.purge",
                method_table()
                    .iter()
                    .find(|m| m.name == "ncui.logs.purge")
                    .expect("p")
                    .requires,
            )
            .expect("mint");
        // Present the purge token for a different destructive method.
        let table = gated_table();
        let request = Request::new(
            json!(1),
            "ncui.data.purge",
            json!({ "consent_token": token.to_value() }),
        );
        let response = dispatch_with(
            &mut runtime,
            &mut session,
            &request,
            Duration::from_secs(0),
            None,
            &table,
        )
        .expect("response");
        let error = response
            .error
            .expect("a token for another method must be refused");
        assert_eq!(error.data.ncui_code, "capability_denied");
    }

    #[test]
    fn a_consent_token_is_consume_once() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        let requires = CapabilitySet::empty().with(Capability::ReadLogs);
        let correlation = CorrelationId::new();
        let token = runtime
            .mint_consent(correlation, "ncui.logs.purge", requires)
            .expect("mint");
        let value = token.to_value();
        let first = call(
            &mut runtime,
            &mut session,
            "ncui.logs.purge",
            json!({ "consent_token": value.clone() }),
        );
        assert!(first.error.is_none(), "{:?}", first.error);
        let second = call(
            &mut runtime,
            &mut session,
            "ncui.logs.purge",
            json!({ "consent_token": value }),
        );
        let error = second.error.expect("a second use must be refused");
        assert_eq!(error.data.ncui_code, "capability_denied");
    }

    #[test]
    fn a_forged_consent_nonce_is_refused() {
        let mut runtime = runtime();
        let mut session = session(AgentRole::Administrator);
        let correlation = CorrelationId::new();
        // Well-formed JSON, but the nonce was never minted here.
        let forged = ConsentToken::mint(
            correlation,
            "ncui.logs.purge",
            CapabilitySet::empty().with(Capability::ReadLogs),
            0xdead_beef,
        );
        let response = call(
            &mut runtime,
            &mut session,
            "ncui.logs.purge",
            json!({ "consent_token": forged.to_value() }),
        );
        let error = response.error.expect("a forged nonce must be refused");
        assert_eq!(error.data.ncui_code, "capability_denied");
    }
}
