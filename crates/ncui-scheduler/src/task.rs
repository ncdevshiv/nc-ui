//! The unit of scheduled work: a [`Task`], its [`PriorityClass`], and the [`Work`] it
//! carries.

use core::time::Duration;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ncui_core::CorrelationId;
use ncui_core::TaskId;

use crate::priority::{Priority, PriorityClass};

/// The payload of a [`Task`].
///
/// A task is a *description* of work, not a closure. The scheduler selects and accounts
/// for work; the frame driver decides what "executing" it means, because that decision
/// belongs to the subsystem that owns the work — an animation frame, a GPU submission,
/// a layout pass. Carrying a boxed closure here instead would make [`Task`] opaque to
/// [`Debug`], un-serialisable for diagnostics, and impossible to account for, because
/// the scheduler could not know what a task costs before deciding to run it.
///
/// Every variant therefore declares its cost in the same currency: a [`Duration`]
/// charged against the frame budget by [`crate::Scheduler::run_budgeted`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    /// Work with no measurable cost: a dispatch, a queue hop, a bookkeeping step.
    ///
    /// Charged nothing, so a stream of immediate work does not consume the frame
    /// budget. That is honest rather than convenient — the budget bounds what the
    /// frame *commits to*, and an immediate task commits to nothing.
    Immediate {
        /// What the work is, for diagnostics and tracing spans.
        label: String,
    },
    /// Work with a known cost, charged against the frame budget in full.
    Timed {
        /// What the work is, for diagnostics and tracing spans.
        label: String,
        /// How long the work is expected to take.
        cost: Duration,
    },
    /// Work whose cost the scheduler was not told.
    ///
    /// Charged [`Work::UNMEASURED_COST`]. Without a nominal charge an unknown-cost
    /// task would run for free and a stream of them would make the frame budget
    /// meaningless; the charge is a declared estimate, not a measurement, and callers
    /// that can measure should say so.
    Unmeasured {
        /// What the work is, for diagnostics and tracing spans.
        label: String,
    },
}

impl Work {
    /// The nominal budget charged for [`Work::Unmeasured`] work.
    ///
    /// One millisecond: long enough that a backlog of unmeasured work still terminates
    /// within a frame budget, short enough that it does not materially delay a frame on
    /// its own.
    pub const UNMEASURED_COST: Duration = Duration::from_millis(1);

    /// Zero-cost work.
    #[must_use]
    pub fn immediate(label: impl Into<String>) -> Self {
        Self::Immediate {
            label: label.into(),
        }
    }

    /// Work whose cost is known.
    #[must_use]
    pub fn timed(label: impl Into<String>, cost: Duration) -> Self {
        Self::Timed {
            label: label.into(),
            cost,
        }
    }

    /// Work whose cost is unknown and will be charged [`Work::UNMEASURED_COST`].
    #[must_use]
    pub fn unmeasured(label: impl Into<String>) -> Self {
        Self::Unmeasured {
            label: label.into(),
        }
    }

    /// The diagnostic label.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Immediate { label } | Self::Timed { label, .. } | Self::Unmeasured { label } => {
                label
            }
        }
    }

    /// What this work costs against a frame budget.
    #[must_use]
    pub const fn cost(&self) -> Duration {
        match self {
            Self::Immediate { .. } => Duration::ZERO,
            Self::Timed { cost, .. } => *cost,
            Self::Unmeasured { .. } => Self::UNMEASURED_COST,
        }
    }
}

/// A unit of work the scheduler can dispatch.
///
/// A task is a plain value: it can be cloned, inspected and moved between the scheduler
/// and the frame driver. The one piece of shared state is the cancellation flag, which
/// is an [`Arc`]ed [`AtomicBool`] so that a holder *outside* the scheduler — the
/// component that submitted it, a cancellation request travelling over IPC — observes
/// the same flag the scheduler reads, and so that cancelling a task the caller still
/// holds is observable from both sides.
#[derive(Debug, Clone)]
pub struct Task {
    id: TaskId,
    priority: PriorityClass,
    work: Work,
    deadline: Option<Duration>,
    cancelled: Arc<AtomicBool>,
    correlation: Option<CorrelationId>,
}

impl Task {
    /// Creates a task of the given static class, ready as of `ready_at`.
    ///
    /// `ready_at` is scheduler-relative time, not wall-clock time: the scheduler is
    /// driven by whoever owns the frame loop, and that owner supplies the clock. Nothing
    /// here reads the system clock, so a whole schedule is reproducible from its inputs.
    #[must_use]
    pub fn new(priority: Priority, ready_at: Duration, work: Work) -> Self {
        Self {
            id: TaskId::new(),
            priority: PriorityClass::new(priority, ready_at),
            work,
            deadline: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            correlation: None,
        }
    }

    /// Replaces the priority class wholesale, for a caller that needs a custom ageing
    /// horizon or a ready-at instant other than the constructor's.
    #[must_use]
    pub const fn with_priority_class(mut self, priority: PriorityClass) -> Self {
        self.priority = priority;
        self
    }

    /// Gives the task a deadline: the scheduler-relative instant after which it is
    /// withdrawn as expired and never runs.
    ///
    /// A deadline is a liveness promise in the other direction — "this work is only
    /// useful before then" — not a request to be interrupted. A task that has already
    /// started is not preempted; see [`crate::Scheduler::run_budgeted`].
    #[must_use]
    pub const fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Correlates every trace, metric and error the task produces with the request that
    /// caused it (spec 50).
    #[must_use]
    pub const fn with_correlation(mut self, correlation: CorrelationId) -> Self {
        self.correlation = Some(correlation);
        self
    }

    /// The task's identifier, unique within the process.
    #[must_use]
    pub const fn id(&self) -> TaskId {
        self.id
    }

    /// The work this task carries.
    #[must_use]
    pub const fn work(&self) -> &Work {
        &self.work
    }

    /// The static class, ignoring ageing.
    #[must_use]
    pub const fn priority(&self) -> Priority {
        self.priority.base()
    }

    /// The effective priority at `now`, ageing included.
    ///
    /// The stored class is time-invariant, so `now` cannot change what *class* the task
    /// is in — only the ageing it has accrued. It is still taken here, and used, because
    /// this is the boundary where the task's own clock is reconciled with the caller's:
    /// a `now` earlier than the task's ready-at instant is a clock regression, and
    /// re-anchoring the ready-at to `now` makes that guarantee explicit once, here,
    /// rather than leaving every downstream `age`, `bonus` and `score` call to repeat it.
    /// [`PriorityClass::age`] saturates as well, so the two agree exactly; this is the
    /// place that says so.
    #[must_use]
    pub fn priority_class(&self, now: Duration) -> PriorityClass {
        let ready_at = self.priority.ready_at().min(now);
        PriorityClass::with_horizon(self.priority.base(), ready_at, self.priority.horizon())
    }

    /// The effective score at `now`, which is what the scheduler orders by.
    #[must_use]
    pub const fn effective_priority(&self, now: Duration) -> u32 {
        self.priority.score(now)
    }

    /// The instant the work became ready.
    #[must_use]
    pub const fn ready_at(&self) -> Duration {
        self.priority.ready_at()
    }

    /// The deadline, if the task has one.
    #[must_use]
    pub fn deadline(&self) -> Option<Duration> {
        self.deadline
    }

    /// The correlation identifier, if the task was submitted under a correlated request.
    #[must_use]
    pub const fn correlation(&self) -> Option<CorrelationId> {
        self.correlation
    }

    /// Whether the task has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Cancels the task.
    ///
    /// Idempotent, and visible through every clone of the task and through the copy the
    /// scheduler holds: once this returns, the scheduler will never return the task from
    /// [`crate::Scheduler::next_task`] or dispatch it from
    /// [`crate::Scheduler::run_budgeted`]. This is the acknowledgement that
    /// cancellation has taken effect.
    ///
    /// Acquire/release ordering pairs with the [`Ordering::Acquire`] load in
    /// [`Task::is_cancelled`], so a caller that observes cancellation also observes
    /// whatever the canceller wrote before calling this.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Whether the deadline has passed at `now`.
    ///
    /// A task with no deadline never expires. The boundary is inclusive: a deadline is
    /// the last instant the task may still run, so `now == deadline` is not yet expired.
    #[must_use]
    pub fn is_expired(&self, now: Duration) -> bool {
        self.deadline.is_some_and(|deadline| now > deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_cost_matches_its_variant() {
        assert_eq!(Work::immediate("a").cost(), Duration::ZERO);
        assert_eq!(
            Work::timed("b", Duration::from_millis(7)).cost(),
            Duration::from_millis(7)
        );
        assert_eq!(Work::unmeasured("c").cost(), Work::UNMEASURED_COST);
    }

    #[test]
    fn work_labels_are_preserved() {
        for work in [
            Work::immediate("one"),
            Work::timed("two", Duration::ZERO),
            Work::unmeasured("three"),
        ] {
            assert!(!work.label().is_empty());
        }
    }

    #[test]
    fn a_fresh_task_has_no_ageing_bonus() {
        let ready_at = Duration::from_millis(5);
        let task = Task::new(Priority::Background, ready_at, Work::immediate("x"));
        assert_eq!(
            task.effective_priority(ready_at),
            Priority::Background.base_score()
        );
    }

    /// An aged task reports a higher effective priority than the same task when fresh.
    #[test]
    fn priority_class_reflects_ageing() {
        let task = Task::new(Priority::Background, Duration::ZERO, Work::immediate("x"));
        let fresh = task.effective_priority(Duration::ZERO);
        let aged = task.effective_priority(Duration::from_secs(10));
        assert!(aged > fresh);
    }

    #[test]
    fn priority_class_never_reports_a_negative_age() {
        // A clock that went backwards must not hand the task a bonus it did not earn.
        let task = Task::new(
            Priority::Background,
            Duration::from_millis(1_000),
            Work::immediate("x"),
        );
        let regressed = Duration::from_millis(10);
        assert_eq!(
            task.priority_class(regressed).age(regressed),
            Duration::ZERO
        );
        assert_eq!(
            task.effective_priority(regressed),
            Priority::Background.base_score()
        );
    }

    #[test]
    fn builder_methods_apply_in_order() {
        let task = Task::new(Priority::Input, Duration::ZERO, Work::immediate("x"))
            .with_priority_class(PriorityClass::with_horizon(
                Priority::Input,
                Duration::ZERO,
                Duration::ZERO,
            ))
            .with_deadline(Duration::from_millis(100))
            .with_correlation(CorrelationId::new());
        assert_eq!(task.deadline(), Some(Duration::from_millis(100)));
        assert!(task.correlation().is_some());
        assert_eq!(task.priority(), Priority::Input);
        assert_eq!(
            task.effective_priority(Duration::from_secs(60)),
            Priority::Input.base_score(),
            "a zero horizon disables ageing"
        );
    }

    #[test]
    fn ids_are_unique_across_tasks() {
        let a = Task::new(Priority::Input, Duration::ZERO, Work::immediate("a"));
        let b = Task::new(Priority::Input, Duration::ZERO, Work::immediate("b"));
        assert_ne!(a.id(), b.id());
    }

    /// Cancellation is shared, not per-copy: this is what makes the scheduler's
    /// "never run a cancelled task" guarantee observable from outside the scheduler.
    #[test]
    fn cancel_is_observed_through_every_clone() {
        let task = Task::new(Priority::Input, Duration::ZERO, Work::immediate("x"));
        let clone = task.clone();
        let later = task.clone();
        assert!(!task.is_cancelled());
        task.cancel();
        assert!(clone.is_cancelled());
        assert!(later.is_cancelled());
        // Idempotent.
        task.cancel();
        assert!(task.is_cancelled());
    }

    #[test]
    fn deadline_expiry_boundary_is_exclusive() {
        let task = Task::new(Priority::Input, Duration::ZERO, Work::immediate("x"))
            .with_deadline(Duration::from_millis(100));
        assert!(!task.is_expired(Duration::from_millis(99)));
        assert!(!task.is_expired(Duration::from_millis(100)));
        assert!(task.is_expired(Duration::from_millis(101)));
    }

    #[test]
    fn a_task_without_a_deadline_never_expires() {
        let task = Task::new(Priority::Input, Duration::ZERO, Work::immediate("x"));
        assert!(!task.is_expired(Duration::ZERO));
        assert!(!task.is_expired(Duration::from_hours(24)));
    }
}
