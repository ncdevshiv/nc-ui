//! Worker tasks and their outcomes.

use ncui_core::CorrelationId;
use serde::{Deserialize, Serialize};

/// The result of running one task.
///
/// A task returns data or a *reason it did not run*. Distinguishing "cancelled" from
/// "failed" matters: a cancelled task is not an error, and a pool that logs cancellations
/// as failures will page someone for a user closing a tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TaskOutcome {
    /// The task ran and produced a result.
    Completed {
        /// Bytes the task produced, for accounting.
        bytes: u64,
    },
    /// The task was cancelled before it began executing.
    Cancelled,
    /// The task failed. The message is diagnostic, never user data.
    Failed {
        /// Machine-readable failure code from `ncui_core`.
        code: String,
        /// What went wrong.
        message: String,
    },
}

impl TaskOutcome {
    /// Whether the task produced a result.
    #[must_use]
    pub const fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    /// Whether the task was cancelled.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// Whether the task failed.
    #[must_use]
    pub const fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// One unit of work submitted to a worker.
pub struct WorkerTask {
    /// Identifies the task for cancellation and tracing.
    pub id: ncui_core::TaskId,
    /// Correlates the task with the request that produced it.
    pub correlation: CorrelationId,
    /// Name of the operation, used as a metric label so durations can be attributed.
    pub operation: String,
    /// Payload size in bytes, checked against the configured limit before execution.
    pub payload_bytes: usize,
    /// The work itself.
    ///
    /// Returns [`TaskOutcome`]. Returning `Err` is not an option: a task that cannot
    /// report a failure has to panic, and a panic in a worker thread is a far worse
    /// diagnostic than a structured error.
    pub run: Box<dyn FnOnce() -> TaskOutcome + Send>,
}

/// The work payload is a boxed closure and cannot be printed, so `Debug` reports the
/// task's identity and size instead. Printing a closure would mean executing it.
impl std::fmt::Debug for WorkerTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerTask")
            .field("id", &self.id)
            .field("correlation", &self.correlation)
            .field("operation", &self.operation)
            .field("payload_bytes", &self.payload_bytes)
            .finish_non_exhaustive()
    }
}

impl WorkerTask {
    /// Builds a task. `run` must be `Send` because it moves to a worker thread.
    pub fn new<F>(operation: impl Into<String>, payload_bytes: usize, run: F) -> Self
    where
        F: FnOnce() -> TaskOutcome + Send + 'static,
    {
        Self {
            id: ncui_core::TaskId::new(),
            correlation: CorrelationId::new(),
            operation: operation.into(),
            payload_bytes,
            run: Box::new(run),
        }
    }

    /// Overrides the correlation id, so a task can be traced back to the request that
    /// scheduled it rather than to the moment it was queued.
    #[must_use]
    pub fn with_correlation(mut self, correlation: CorrelationId) -> Self {
        self.correlation = correlation;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_classification_is_exhaustive() {
        assert!(TaskOutcome::Completed { bytes: 0 }.is_completed());
        assert!(TaskOutcome::Cancelled.is_cancelled());
        assert!(
            TaskOutcome::Failed {
                code: "io".to_owned(),
                message: "m".to_owned()
            }
            .is_failed()
        );

        // Each variant must answer exactly one of the three questions.
        for outcome in [
            TaskOutcome::Completed { bytes: 1 },
            TaskOutcome::Cancelled,
            TaskOutcome::Failed {
                code: "c".to_owned(),
                message: "m".to_owned(),
            },
        ] {
            let matches = [
                outcome.is_completed(),
                outcome.is_cancelled(),
                outcome.is_failed(),
            ]
            .iter()
            .filter(|hit| **hit)
            .count();
            assert_eq!(matches, 1, "exactly one classification must hold");
        }
    }

    #[test]
    fn outcome_serialises_with_a_stable_tag() {
        let json = serde_json::to_string(&TaskOutcome::Cancelled).expect("serializable");
        assert_eq!(json, r#"{"outcome":"cancelled"}"#);
    }

    #[test]
    fn correlation_can_be_supplied_by_the_caller() {
        let correlation = CorrelationId::new();
        let task =
            WorkerTask::new("index", 0, || TaskOutcome::Cancelled).with_correlation(correlation);
        assert_eq!(task.correlation, correlation);
    }
}
