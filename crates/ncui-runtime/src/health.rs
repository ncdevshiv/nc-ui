//! Health reporting.
//!
//! `ncui health` answers two different questions and conflating them is a common mistake:
//!
//! * **Is the process alive?** (`/health/live`) — a liveness probe. Answering "no" causes
//!   a supervisor to restart the process, so it must fail only when the process genuinely
//!   cannot serve.
//! * **Is the process ready to do useful work?** (`/health/ready`) — a readiness probe.
//!   A runtime with zero workers can be alive but not ready, and reporting it as ready
//!   would send traffic at a system that cannot handle it.
//!
//! [`HealthReport`] carries both, plus per-subsystem detail under [`HealthReport::deep`].

use serde::{Deserialize, Serialize};

use crate::Runtime;

/// The verdict for one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// Working as expected.
    Ok,
    /// Degraded: usable, but something is wrong.
    Degraded,
    /// Not usable.
    Failing,
}

/// The overall verdict, aggregated from the individual checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverallHealth {
    /// Every check passed.
    Healthy,
    /// Something is degraded but the runtime is serving.
    Degraded,
    /// At least one check is failing.
    Unhealthy,
}

impl OverallHealth {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }
}

/// One named check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthCheck {
    /// Check name, e.g. `"workers"`.
    pub name: String,
    /// Verdict.
    pub status: HealthStatus,
    /// What was observed, in a form an operator can act on.
    pub detail: String,
}

impl HealthCheck {
    /// Builds a passing check.
    #[must_use]
    pub fn ok(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            status: HealthStatus::Ok,
            detail: detail.into(),
        }
    }

    /// Builds a degraded check.
    #[must_use]
    pub fn degraded(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            status: HealthStatus::Degraded,
            detail: detail.into(),
        }
    }

    /// Builds a failing check.
    #[must_use]
    pub fn failing(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            status: HealthStatus::Failing,
            detail: detail.into(),
        }
    }
}

/// The full health report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthReport {
    /// Aggregate verdict.
    pub status: OverallHealth,
    /// Whether the process is alive. Only `false` when the runtime cannot serve at all.
    pub live: bool,
    /// Whether the runtime can do useful work.
    pub ready: bool,
    /// The individual checks.
    pub checks: Vec<HealthCheck>,
    /// Whether this report includes per-subsystem detail.
    pub deep: bool,
}

impl HealthReport {
    /// Builds a report from `checks`, deriving the aggregate verdict.
    ///
    /// Readiness follows the workers specifically, because a runtime with no workers can
    /// serve introspection but cannot do the work it exists to do.
    #[must_use]
    pub fn from_checks(checks: Vec<HealthCheck>, deep: bool) -> Self {
        let status = if checks.iter().any(|c| c.status == HealthStatus::Failing) {
            OverallHealth::Unhealthy
        } else if checks.iter().any(|c| c.status == HealthStatus::Degraded) {
            OverallHealth::Degraded
        } else {
            OverallHealth::Healthy
        };
        let ready = checks
            .iter()
            .filter(|c| c.name == "workers")
            .all(|c| c.status == HealthStatus::Ok);
        let live = !checks.is_empty();

        Self {
            status,
            live,
            ready,
            checks,
            deep,
        }
    }

    /// The named checks, for a caller that wants one in particular.
    #[must_use]
    pub fn check(&self, name: &str) -> Option<&HealthCheck> {
        self.checks.iter().find(|c| c.name == name)
    }
}

/// Produces a health report for a runtime.
///
/// The `deep` flag controls whether per-subsystem counters are included. The shallow form
/// stays cheap enough for a probe to call every second.
#[must_use]
pub fn report(runtime: &Runtime, deep: bool) -> HealthReport {
    let mut checks = vec![HealthCheck::ok(
        "runtime",
        format!("ncui {} alive", crate::RUNTIME_VERSION),
    )];

    let workers = runtime.workers.worker_count();
    let (completed, failed, cancelled) = runtime.workers.task_counters();
    let workers_check = if workers == 0 {
        HealthCheck::failing("workers", "no workers are available")
    } else {
        HealthCheck::ok(
            "workers",
            format!(
                "{workers} workers, queue depth {}, completed {completed}, failed {failed}, cancelled {cancelled}",
                runtime.workers.queue_depth()
            ),
        )
    };
    checks.push(workers_check);

    let metric_failures = runtime.workers.metric_failures();
    checks.push(if metric_failures == 0 {
        HealthCheck::ok("metrics", "all metric writes are succeeding")
    } else {
        // Instrumenting failures are degraded, not fatal: the runtime still works, but its
        // observability is unreliable, which is itself an incident.
        HealthCheck::degraded(
            "metrics",
            format!("{metric_failures} metric writes have failed"),
        )
    });

    if runtime.metrics.evicted_total() > 0 {
        checks.push(HealthCheck::degraded(
            "metrics_capacity",
            format!(
                "{} metric series were evicted by the configured cap",
                runtime.metrics.evicted_total()
            ),
        ));
    }

    if deep {
        checks.push(HealthCheck::ok(
            "limits",
            format!(
                "max {} workers, queue {}, frame {} bytes, in-flight {}",
                runtime.limits.max_workers,
                runtime.limits.max_worker_queue_depth,
                runtime.limits.max_ipc_frame_bytes,
                runtime.limits.max_in_flight_requests
            ),
        ));
        checks.push(HealthCheck::ok(
            "scheduler",
            format!("{} tasks ready", runtime.scheduler.depth()),
        ));
        checks.push(HealthCheck::ok(
            "logs",
            format!(
                "{} of {} ring entries retained",
                runtime.logs.len(),
                runtime.logs.capacity()
            ),
        ));
    }

    HealthReport::from_checks(checks, deep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::Limits;

    fn runtime() -> Runtime {
        Runtime::start(Limits::default()).expect("runtime")
    }

    #[test]
    fn a_fresh_runtime_is_healthy_and_both_live_and_ready() {
        let runtime = runtime();
        let report = report(&runtime, false);
        assert_eq!(report.status, OverallHealth::Healthy);
        assert!(report.live);
        assert!(report.ready);
        assert!(!report.deep);
    }

    #[test]
    fn the_deep_report_adds_subsystem_detail() {
        let runtime = runtime();
        let shallow = report(&runtime, false);
        let deep = report(&runtime, true);
        assert!(!shallow.deep);
        assert!(deep.deep);
        assert!(
            deep.checks.len() > shallow.checks.len(),
            "the deep report must carry strictly more detail"
        );
        assert!(deep.check("scheduler").is_some());
        assert!(deep.check("limits").is_some());
    }

    #[test]
    fn a_failing_check_makes_the_whole_report_unhealthy() {
        let report = HealthReport::from_checks(
            vec![
                HealthCheck::ok("runtime", "fine"),
                HealthCheck::failing("workers", "none"),
            ],
            false,
        );
        assert_eq!(report.status, OverallHealth::Unhealthy);
        assert!(
            report.live,
            "a degraded subsystem still means the process is alive"
        );
        assert!(!report.ready, "a runtime with no workers is not ready");
    }

    #[test]
    fn a_degraded_check_degrades_without_failing() {
        let report = HealthReport::from_checks(
            vec![
                HealthCheck::ok("workers", "fine"),
                HealthCheck::degraded("metrics", "one write failed"),
            ],
            false,
        );
        assert_eq!(report.status, OverallHealth::Degraded);
        assert!(report.ready, "a degraded metric is not a readiness failure");
    }

    #[test]
    fn an_empty_check_list_is_not_live() {
        let report = HealthReport::from_checks(Vec::new(), false);
        assert!(
            !report.live,
            "a runtime that produced no checks cannot be called alive"
        );
    }

    #[test]
    fn reports_serialise_to_machine_readable_json() {
        let runtime = runtime();
        let value = serde_json::to_value(report(&runtime, true)).expect("serializable");
        assert_eq!(value["status"], "healthy");
        assert!(value["checks"].is_array());
        assert!(value["live"].as_bool().is_some());
    }
}
