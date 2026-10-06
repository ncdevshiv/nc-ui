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
use std::time::SystemTime;

use ncui_core::{Capability, CapabilitySet, CorrelationId, ErrorKind, NcError, Result};
use ncui_security::audit::AuditEvent;
use ncui_security::audit::AuditOutcome;
use serde_json::{Value, json};

use crate::Runtime;
use crate::agent::AgentSession;
use crate::describe::{MethodDescription, describe};
use crate::health;
use crate::rpc::{Request, Response};

/// The methods this runtime implements, with the authority each one needs.
///
/// The table is the single source of truth: `ncui.describe` reports it, and dispatch
/// enforces it. Keeping them in one place is what stops the advertised surface and the
/// enforced surface from drifting apart.
#[must_use]
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
            "Bounded log ring filtered by level.",
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
    ]
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

fn run_pipeline(
    runtime: &mut Runtime,
    session: &mut AgentSession,
    request: &Request,
    now: Duration,
    feature_registry: Option<&Value>,
    table: &[MethodDescription],
) -> Result<Value> {
    let correlation = CorrelationId::new();

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

    // Consent is an explicit parameter, not an implicit "the caller was allowed to try".
    // A request that does not state its consent is treated as withholding it.
    let consent = request
        .params
        .get("consent")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let outcome = (|| -> Result<Value> {
        session.rate_limit(now, 1)?;
        session.authorize(method.requires)?;
        session.require_consent(method.destructive, consent)?;
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
        "ncui.logs.query" => {
            let limit = bounded_limit(&request.params)?;
            let all = runtime.logs.to_vec();
            let truncated = all.len() > limit;
            let slice: Vec<_> = all.into_iter().rev().take(limit).collect();
            serde_json::to_value(json!({
                "entries": slice,
                "truncated": truncated,
                "ring_capacity": runtime.logs.capacity(),
            }))
            .map_err(|e| json_error(&e))
        }
        "ncui.logs.purge" => {
            // The consent gate has already run; reaching here means the call was
            // authorized. Clearing the ring is the whole operation.
            let purged = runtime.logs.len();
            runtime.logs.clear();
            serde_json::to_value(json!({ "purged": purged })).map_err(|e| json_error(&e))
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

/// Reads and bounds the `limit` parameter.
///
/// Exceeding the maximum is a refusal, not a silent clamp: a silently clamped result reads
/// as "this is everything", which is worse than an error.
fn bounded_limit(params: &Value) -> Result<usize> {
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

fn json_error(error: &serde_json::Error) -> NcError {
    NcError::new(ErrorKind::Codec {
        operation: "dispatch::serialize".to_owned(),
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::Limits;
    use ncui_security::authz::AgentRole;

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
        // Every role holds the read-only diagnostics, so all of them must be usable. The
        // one destructive method additionally requires consent, which is supplied here
        // because this test is about role coverage rather than about the consent gate
        // (that is asserted separately, in agent_surface.rs).
        let registry = json!({"schema_version":1,"features":[]});
        for role in AgentRole::ALL {
            let mut runtime = runtime();
            let mut session = session(*role);
            for method in method_table() {
                let request = Request::new(json!(1), &method.name, json!({ "consent": true }));
                let response = dispatch_with(
                    &mut runtime,
                    &mut session,
                    &request,
                    Duration::from_secs(0),
                    Some(&registry),
                    &method_table(),
                )
                .expect("response");
                assert!(
                    response.error.is_none(),
                    "role {role:?} could not call {}: {:?}",
                    method.name,
                    response.error
                );
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
        let mut session = session(AgentRole::Administrator);
        let registry = json!({ "schema_version": 1, "features": [] });
        for method in method_table() {
            let request = Request::new(
                json!(1),
                &method.name,
                json!({ "deep": true, "consent": true }),
            );
            let response = dispatch(
                &mut runtime,
                &mut session,
                &request,
                Duration::from_secs(0),
                Some(&registry),
            )
            .expect("response");
            assert!(
                response.error.is_none(),
                "advertised method `{}` failed: {:?}",
                method.name,
                response.error
            );
        }
    }
}
