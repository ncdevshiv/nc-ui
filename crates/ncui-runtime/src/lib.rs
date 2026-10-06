//! Runtime composition and the agent-facing interface.
//!
//! This crate is where the subsystems meet. It owns no algorithm of its own: the error
//! model comes from `ncui-core`, capability enforcement from `ncui-security`, metrics from
//! `ncui-observability`, scheduling from `ncui-scheduler`, state from `ncui-state`, workers
//! from `ncui-workers`, and framing from `ncui-ipc`. What this crate adds is the thing no
//! single subsystem can provide alone: **a single place where a request enters, is
//! authenticated, is authorized, is rate limited, is audited, and produces a correlated
//! result**.
//!
//! # The agent path
//!
//! ```text
//! agent request
//!    -> authenticate   (bearer token, constant-time)
//!    -> rate limit     (token bucket, per session)
//!    -> authorize      (capability set, destructive gate)
//!    -> dispatch       (method registry)
//!    -> audit          (always, success or failure)
//! ```
//!
//! Every one of those steps is enforced, not merely documented. [`agent::AgentSession`]
//! holds an authentication flag: a session that has not authenticated cannot authorize,
//! and therefore cannot dispatch at all. A request that fails authorization is still
//! audited, because "who tried what" is exactly the question an incident review asks
//! first (ADR-007, ADR-008).
//!
//! The one step a caller performs is authentication itself, because it is the step that
//! depends on a transport. The runtime owns the verification — constant-time comparison
//! against a presented credential — and owns the consequence of not having done it.
//!
//! # What this crate does not do
//!
//! [`rpc`] produces and consumes request and response values; [`transport`] binds them to a
//! loopback socket so an agent in another process can reach the same surface. The socket
//! dispatches through the pipeline above rather than beside it, so the transport adds
//! authentication and framing but no policy of its own.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

pub mod agent;
pub mod describe;
pub mod dispatch;
pub mod health;
pub mod mcp;
pub mod rpc;
pub mod transport;

use std::sync::Arc;

use ncui_core::{Limits, Result};
use ncui_observability::logring::LogRing;
use ncui_observability::metrics::MetricsRegistry;
use ncui_scheduler::Scheduler;
use ncui_security::audit::AuditLog;
use ncui_workers::WorkerPool;

/// The runtime version reported by `ncui.describe` and by `ncui status`.
pub const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Every subsystem the runtime composed, held in one place.
///
/// The runtime is shared by reference (`Arc<Runtime>`) rather than cloned, so a request
/// handler cannot accidentally produce a second runtime with its own, divergent state.
pub struct Runtime {
    /// The limits this runtime was started with.
    pub limits: Limits,
    /// Shared metrics.
    pub metrics: Arc<MetricsRegistry>,
    /// Bounded in-memory log ring.
    pub logs: Arc<LogRing>,
    /// Bounded audit trail.
    pub audit: AuditLog,
    /// The cooperative scheduler.
    pub scheduler: Scheduler,
    /// The native worker pool.
    pub workers: Arc<WorkerPool>,
}

impl Runtime {
    /// Starts a runtime from `limits`.
    ///
    /// Fails rather than silently degrading: incoherent limits or an unavailable worker
    /// pool is reported here, where it can be acted on, instead of surfacing later as
    /// mysterious behaviour.
    ///
    /// # Errors
    ///
    /// Returns [`ncui_core::ErrorKind::Validation`] if `limits` is incoherent, and any
    /// error the metrics registry, log ring, scheduler, audit log or worker pool reports at
    /// construction.
    pub fn start(limits: Limits) -> Result<Self> {
        let metrics = Arc::new(MetricsRegistry::new(limits)?);
        let logs = Arc::new(LogRing::new(limits)?);
        let audit = AuditLog::new(limits.log_ring_capacity)?;
        let scheduler = Scheduler::new(limits.max_worker_queue_depth)?;
        let workers = Arc::new(WorkerPool::new(limits, Arc::clone(&metrics))?);

        Ok(Self {
            limits,
            metrics,
            logs,
            audit,
            scheduler,
            workers,
        })
    }

    /// Stops the runtime, draining workers.
    ///
    /// Explicit so that a caller which knows it has work to flush can say so, rather than
    /// relying on `Drop` and having no way to express intent.
    pub fn shutdown(&self) {
        self.workers.shutdown();
    }
}

impl std::fmt::Debug for Runtime {
    /// Reports the runtime's observable shape rather than dumping every subsystem, which
    /// would itself stall a pool being inspected.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("version", &RUNTIME_VERSION)
            .field("workers", &self.workers.worker_count())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::LimitsBuilder;

    #[test]
    fn a_runtime_starts_with_the_limits_it_was_given() {
        let limits = LimitsBuilder::new()
            .max_workers(2)
            .max_worker_queue_depth(4)
            .build()
            .expect("limits");
        let runtime = Runtime::start(limits).expect("runtime should start");
        assert_eq!(runtime.limits.max_workers, 2);
        assert_eq!(runtime.workers.worker_count(), 2);
        runtime.shutdown();
    }

    #[test]
    fn incoherent_limits_are_refused_rather_than_clamped() {
        let broken = Limits {
            request_timeout_ms: 0,
            ..Limits::default()
        };
        let err = Runtime::start(broken).expect_err("incoherent limits must be refused");
        assert!(err.to_string().contains("request_timeout_ms"), "got: {err}");
    }

    #[test]
    fn runtime_debug_reports_shape_without_dumping_subsystems() {
        let runtime = Runtime::start(Limits::default()).expect("runtime");
        let text = format!("{runtime:?}");
        assert!(text.contains("Runtime"), "got: {text}");
        assert!(text.contains("workers"), "got: {text}");
    }
}
