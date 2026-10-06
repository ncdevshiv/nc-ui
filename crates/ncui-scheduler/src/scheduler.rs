//! The frame-loop-facing scheduler.

use core::time::Duration;

use ncui_core::Result;
use ncui_core::TaskId;

use crate::queue::ReadyQueue;
use crate::task::Task;

/// What one call to [`Scheduler::run_budgeted`] did.
///
/// The report is the frame's accounting: it is what a frame-time metric is built from,
/// and what a test asserts against. Every field is derived from the work actually
/// selected, never estimated, so a frame that overshot its budget can be diagnosed
/// from this alone.
///
/// `PartialEq` is deliberately not derived: [`Task`] holds shared cancellation state and
/// is therefore not comparable as a value. Compare [`ScheduleReport::ran_ids`] instead.
#[derive(Debug, Clone)]
pub struct ScheduleReport {
    /// The tasks selected for dispatch this frame, in the order they must run.
    ///
    /// The tasks are handed back rather than dropped so the frame driver can execute
    /// them; the scheduler owns selection and accounting, not execution, because what
    /// "running" a task means belongs to the subsystem that produced it.
    pub ran: Vec<Task>,
    /// Tasks withdrawn because their deadline had passed, in submission order.
    ///
    /// These were never dispatched: they were already too late to be useful.
    pub expired: Vec<TaskId>,
    /// Ready tasks skipped because they had been cancelled.
    pub skipped_cancelled: usize,
    /// How many entries are still queued when the call returned.
    pub remaining: usize,
    /// The budget actually charged: the summed cost of every task in `ran`.
    pub elapsed: Duration,
    /// The frame budget this call was given.
    pub budget: Duration,
    /// Whether the budget, rather than an empty queue, is what stopped the frame.
    ///
    /// `false` with a non-zero `remaining` is impossible; a false value together with
    /// `remaining == 0` means the queue drained. Keeping the two apart matters: "the
    /// frame was too busy" and "the frame had nothing to do" need different responses
    /// from whatever is driving the loop.
    pub budget_exhausted: bool,
    /// Whether the queue held nothing runnable when the call returned.
    pub queue_empty: bool,
}

impl ScheduleReport {
    /// How many tasks were dispatched.
    #[must_use]
    pub fn ran_count(&self) -> usize {
        self.ran.len()
    }

    /// The identifiers of the dispatched tasks, in dispatch order.
    pub fn ran_ids(&self) -> impl Iterator<Item = TaskId> + '_ {
        self.ran.iter().map(Task::id)
    }

    /// How much of the budget was left when the call returned.
    ///
    /// Saturating at zero: a single task can cost more than the whole frame budget, and
    /// reporting a negative remainder would be worse than reporting none.
    #[must_use]
    pub fn budget_remaining(&self) -> Duration {
        self.budget.saturating_sub(self.elapsed)
    }
}

/// A priority-aware, cooperative, cancellation-safe scheduler.
///
/// # The frame protocol
///
/// A frame driver calls [`Scheduler::run_budgeted`] once per frame. That call *is* the
/// frame tick, and it performs two steps in a fixed order:
///
/// 1. withdraw work whose deadline has passed, so that late work is never dispatched;
/// 2. dispatch ready work until the queue is empty or the frame budget is spent.
///
/// Both steps live in one call on purpose. Splitting them would make the caller
/// responsible for an ordering that is easy to get wrong, and a caller that forgets
/// would silently dispatch work it had already declared too late — a class of defect
/// that is invisible in testing and only shows up as a stale UI under load.
///
/// [`Scheduler::expire`] and [`Scheduler::next_task`] remain public for callers that
/// drive the steps themselves, and both are idempotent.
#[derive(Debug)]
pub struct Scheduler {
    queue: ReadyQueue,
}

impl Scheduler {
    /// Creates a scheduler whose ready queue holds at most `capacity` tasks.
    ///
    /// # Errors
    ///
    /// [`ncui_core::ErrorKind::Validation`] if `capacity` is zero.
    pub fn new(capacity: usize) -> Result<Self> {
        Ok(Self {
            queue: ReadyQueue::new(capacity)?,
        })
    }

    /// The configured ready-queue capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.queue.capacity()
    }

    /// How many entries are queued, including cancelled entries not yet purged.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.queue.len()
    }

    /// Whether nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// A read-only view of the ready queue, for metrics and diagnostics.
    #[must_use]
    pub const fn queue(&self) -> &ReadyQueue {
        &self.queue
    }

    /// Accepts a task and returns its identifier.
    ///
    /// Cancelled entries are purged first, so a queue full of dead work does not report
    /// itself as full: a cancelled backlog must not be able to block the interactive
    /// work whose arrival caused the cancellations. That is an anti-starvation concern
    /// too, not just housekeeping.
    ///
    /// # Errors
    ///
    /// * [`ncui_core::ErrorKind::Cancelled`] if the task was already cancelled before it
    ///   was submitted. Accepting it would consume a queue slot for work that can never
    ///   run.
    /// * [`ncui_core::ErrorKind::Backpressure`] if the ready queue is at capacity.
    pub fn schedule(&mut self, task: Task) -> Result<TaskId> {
        if task.is_cancelled() {
            return Err(ncui_core::NcError::new(ncui_core::ErrorKind::Cancelled)
                .operation("scheduler::Scheduler::schedule"));
        }
        self.queue.purge_cancelled();
        let id = task.id();
        self.queue.push(task)?;
        Ok(id)
    }

    /// Removes and returns the best ready task, or `None` if nothing is runnable.
    ///
    /// Cancelled tasks are never returned, including when a cancelled task is the
    /// highest priority and sits at the head of the queue. Cancelled entries found on
    /// the way are discarded, so a caller that polls this in a loop makes progress
    /// rather than spinning over dead entries.
    pub fn next_task(&mut self, now: Duration) -> Option<Task> {
        self.queue.pop_best(now).into_task()
    }

    /// Runs the frame: withdraw expired work, then dispatch ready work within `budget`.
    ///
    /// The budget is *cooperative*, not preemptive. It bounds how much work the frame
    /// commits to; it cannot split a task. Two consequences follow, and both are
    /// deliberate:
    ///
    /// * A task whose cost exceeds the remaining budget still runs, and `elapsed` may
    ///   exceed `budget` by at most the cost of the last task dispatched. The
    ///   alternative — refusing to dispatch expensive work — would reorder the frame by
    ///   cost, so a cheap background task could displace an expensive input task. Frame
    ///   priority beats frame cost.
    /// * A task that has already been dispatched is not preempted by the budget; it runs
    ///   to completion. Cancellation is the only thing that can stop it early, and only
    ///   before it starts.
    ///
    /// `now` is the scheduler-relative instant of this frame. It is the only clock the
    /// scheduler reads; nothing here consults the system clock.
    pub fn run_budgeted(&mut self, now: Duration, budget: Duration) -> ScheduleReport {
        let expired = self.expire(now);
        let mut ran: Vec<Task> = Vec::new();
        let mut skipped_cancelled = 0_usize;
        let mut elapsed = Duration::ZERO;

        loop {
            if elapsed >= budget {
                break;
            }
            let popped = self.queue.pop_best(now);
            skipped_cancelled += popped.discarded_cancelled;
            let Some(task) = popped.task else {
                break;
            };
            elapsed = elapsed.saturating_add(task.work().cost());
            ran.push(task);
        }

        let remaining = self.queue.len();
        ScheduleReport {
            ran,
            expired,
            skipped_cancelled,
            remaining,
            elapsed,
            budget,
            budget_exhausted: remaining > 0 && elapsed >= budget,
            queue_empty: remaining == 0,
        }
    }

    /// Cancels the queued task with `id`.
    ///
    /// Once this returns `Ok`, the task will never be returned by
    /// [`Scheduler::next_task`] nor dispatched by [`Scheduler::run_budgeted`]. That is
    /// the acknowledgement the cancellation contract rests on, and it is why the flag is
    /// shared with every clone of the task: the caller can observe the same fact the
    /// scheduler does.
    ///
    /// Calling it again for a task that is still queued succeeds and changes nothing.
    /// Calling it for a task that has already been dispatched reports
    /// [`ncui_core::ErrorKind::NotFound`]: the scheduler does not track dispatched work,
    /// and the caller now holds the task and can cancel it directly. Reporting success
    /// there would acknowledge a cancellation of something the scheduler no longer has.
    ///
    /// # Errors
    ///
    /// [`ncui_core::ErrorKind::NotFound`] when no queued task has that id.
    pub fn cancel(&mut self, task_id: TaskId) -> Result<()> {
        self.queue.cancel(task_id)
    }

    /// Withdraws every queued task whose deadline has passed at `now`.
    ///
    /// Withdrawn tasks are cancelled as well as removed, and are returned in submission
    /// order. Idempotent: a second call finds nothing left to withdraw.
    pub fn expire(&mut self, now: Duration) -> Vec<TaskId> {
        self.queue.expire(now)
    }

    /// Discards every cancelled entry and reports how many were removed.
    pub fn purge_cancelled(&mut self) -> usize {
        self.queue.purge_cancelled()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::priority::{DEFAULT_AGEING_HORIZON, PROTECTED_FLOOR_SCORE, Priority};
    use crate::queue::READY_QUEUE_CHANNEL;
    use crate::task::Work;
    use crate::testing::Rng;
    use ncui_core::ErrorKind;

    const MS: Duration = Duration::from_millis(1);

    fn scheduler(capacity: usize) -> Scheduler {
        Scheduler::new(capacity).expect("non-zero capacity")
    }

    fn task(priority: Priority, ready_at: Duration) -> Task {
        Task::new(
            priority,
            ready_at,
            Work::timed("t", Duration::from_millis(1)),
        )
    }

    fn submit(scheduler: &mut Scheduler, priority: Priority, ready_at: Duration) -> TaskId {
        scheduler
            .schedule(task(priority, ready_at))
            .expect("capacity is not exceeded")
    }

    /// Pops one task and returns its static priority.
    fn pop_priority(scheduler: &mut Scheduler, now: Duration) -> Option<Priority> {
        scheduler.next_task(now).map(|t| t.priority())
    }

    #[test]
    fn zero_capacity_is_rejected() {
        let err = Scheduler::new(0).expect_err("zero capacity must be rejected");
        assert!(matches!(err.kind(), ErrorKind::Validation { .. }));
    }

    #[test]
    fn schedule_returns_the_task_id() {
        let mut s = scheduler(4);
        let t = task(Priority::Input, Duration::ZERO);
        let expected = t.id();
        assert_eq!(s.schedule(t).expect("fits"), expected);
        assert_eq!(s.depth(), 1);
    }

    #[test]
    fn strict_priority_ordering_holds_regardless_of_submission_order() {
        let mut s = scheduler(16);
        // Submit worst-first so submission order is the exact inverse of urgency.
        let mut expected: Vec<Priority> = Priority::ALL.to_vec();
        expected.reverse();
        for class in expected.iter().copied() {
            submit(&mut s, class, Duration::ZERO);
        }

        let mut popped = Vec::new();
        while let Some(class) = pop_priority(&mut s, Duration::ZERO) {
            popped.push(class);
        }
        assert_eq!(popped, expected);
    }

    #[test]
    fn fifo_holds_within_equal_priority() {
        let mut s = scheduler(16);
        let mut ids = Vec::new();
        for _ in 0..8 {
            ids.push(submit(&mut s, Priority::Interactive, Duration::ZERO));
        }
        let popped: Vec<TaskId> = std::iter::from_fn(|| s.next_task(Duration::ZERO))
            .map(|t| t.id())
            .collect();
        assert_eq!(popped, ids);
    }

    /// **Anti-starvation invariant 1.**
    ///
    /// A continuous, permanently-saturated stream of background work is submitted, and
    /// every task in it is made maximally old — ready at `t = 0` while the clock runs
    /// far past the ageing horizon — so every background task sits at its ceiling score.
    /// An input task is then submitted. The proof obligation is that it is dispatched
    /// within a *bounded* number of pops; the bound this crate claims is one.
    ///
    /// The loop runs ten thousand times, re-saturating the queue before every probe, so
    /// the property is checked against a genuinely permanent backlog rather than one
    /// that happens to drain.
    #[test]
    fn a_saturated_background_stream_never_starves_input() {
        const ROUNDS: usize = 10_000;
        const CAPACITY: usize = 32;
        let mut s = scheduler(CAPACITY);

        for round in 0..ROUNDS {
            // A far-future clock, so every background task is fully aged.
            let now = Duration::from_secs(3_600) + Duration::from_millis(round as u64);
            // Fill to exactly one slot short of capacity. Ready-at zero means a full
            // ageing bonus at any `now`, so the backlog is maximally aged — the worst
            // case the ageing policy can create.
            while s.depth() < CAPACITY - 1 {
                s.schedule(Task::new(
                    Priority::Background,
                    Duration::ZERO,
                    Work::timed("bg", MS),
                ))
                .expect("refilling a queue that is not full");
            }
            assert!(
                !s.queue().is_full(),
                "one slot must remain for the urgent task"
            );

            let input = s
                .schedule(Task::new(Priority::Input, now, Work::timed("input", MS)))
                .expect("the reserved slot");

            // Bounded: exactly one pop must be enough.
            let mut pops = 0_usize;
            loop {
                pops += 1;
                let dispatched = s
                    .next_task(now)
                    .expect("the input task is queued and uncancelled");
                if dispatched.id() == input {
                    break;
                }
                assert_ne!(
                    dispatched.priority(),
                    Priority::Input,
                    "a background task was dispatched while input was pending"
                );
                assert!(
                    pops <= 1,
                    "round {round}: input waited {pops} pops behind background work"
                );
            }
            assert_eq!(pops, 1, "round {round}: input was not dispatched first");
        }
    }

    /// The same proof for every protected class, not just input: none of them may ever
    /// be delayed by unprotected work.
    #[test]
    fn no_protected_class_is_ever_delayed_by_unprotected_work() {
        for protected in [
            Priority::Interactive,
            Priority::Animation,
            Priority::GpuRendering,
            Priority::VisibleUi,
            Priority::Input,
        ] {
            let mut s = scheduler(8);
            let now = Duration::from_secs(600);
            for _ in 0..7 {
                s.schedule(Task::new(
                    Priority::Background,
                    Duration::ZERO,
                    Work::timed("bg", MS),
                ))
                .expect("refilling");
            }
            let urgent = s
                .schedule(Task::new(protected, now, Work::timed("urgent", MS)))
                .expect("one slot was reserved");
            let dispatched = s.next_task(now).expect("the urgent task is queued");
            assert_eq!(dispatched.id(), urgent, "{protected:?} was delayed");
        }
    }

    /// The arithmetic behind the proof, asserted directly rather than only through the
    /// queue: no unprotected task, at any wait time, can reach the protected band.
    #[test]
    fn unprotected_ageing_is_bounded_below_the_protected_floor() {
        let now = Duration::from_secs(u64::MAX / 2);
        let background = crate::priority::PriorityClass::new(Priority::Background, Duration::ZERO);
        let maintenance =
            crate::priority::PriorityClass::new(Priority::Maintenance, Duration::ZERO);
        assert!(background.score(now) < PROTECTED_FLOOR_SCORE);
        assert!(maintenance.score(now) < PROTECTED_FLOOR_SCORE);
        // Maintenance takes the full one-unit margin below the floor; background gives up the
        // unit so that an aged maintenance task can strictly outrank aged background work
        // rather than tying with it (a tie is FIFO, which starves the maintenance task).
        assert_eq!(
            maintenance.score(now),
            PROTECTED_FLOOR_SCORE - 1,
            "maintenance must saturate exactly one unit below the floor"
        );
        assert_eq!(
            background.score(now),
            PROTECTED_FLOOR_SCORE - 2,
            "background saturates two units below the floor, leaving room for maintenance"
        );
        assert!(
            maintenance.score(now) > background.score(now),
            "an aged maintenance task must strictly outrank an aged background task"
        );
    }

    /// Ageing still does its job within the unprotected band: a long-waiting
    /// maintenance task overtakes a fresh background task, which submission order alone
    /// would not do.
    #[test]
    fn an_aged_maintenance_task_is_promoted_over_fresh_background_work() {
        let mut s = scheduler(8);
        let now = Duration::from_secs(1);
        s.schedule(Task::new(
            Priority::Background,
            now,
            Work::timed("fresh-bg", MS),
        ))
        .expect("fits");
        let old_maintenance = s
            .schedule(Task::new(
                Priority::Maintenance,
                Duration::ZERO,
                Work::timed("old-maint", MS),
            ))
            .expect("fits");

        let dispatched = s.next_task(now).expect("two tasks are queued");
        assert_eq!(dispatched.id(), old_maintenance);
        assert_eq!(dispatched.priority(), Priority::Maintenance);
    }

    /// **Anti-starvation invariant 2.** A cancelled task is never returned, even when it
    /// is the highest priority and at the head of the queue, and even when it is
    /// cancelled by an outside holder of a clone rather than through the scheduler.
    #[test]
    fn a_cancelled_task_is_never_returned_even_at_the_head() {
        let mut s = scheduler(8);
        let head = Task::new(Priority::Input, Duration::ZERO, Work::timed("head", MS));
        let head_id = head.id();
        let outside = head.clone();
        s.schedule(head).expect("fits");
        submit(&mut s, Priority::VisibleUi, Duration::ZERO);
        submit(&mut s, Priority::Interactive, Duration::ZERO);

        s.cancel(head_id).expect("queued");
        assert!(
            outside.is_cancelled(),
            "the flag is shared with outside holders"
        );

        let dispatched = s.next_task(Duration::ZERO).expect("two live tasks remain");
        assert_ne!(dispatched.id(), head_id);
        assert_eq!(dispatched.priority(), Priority::VisibleUi);
    }

    #[test]
    fn cancelling_every_task_empties_the_queue_without_returning_one() {
        let mut s = scheduler(8);
        let mut ids = Vec::new();
        for _ in 0..4 {
            ids.push(submit(&mut s, Priority::Input, Duration::ZERO));
        }
        for id in &ids {
            s.cancel(*id).expect("queued");
        }
        assert!(s.next_task(Duration::ZERO).is_none());
        assert!(s.is_empty());
    }

    #[test]
    fn cancel_is_idempotent_while_the_task_is_still_queued() {
        let mut s = scheduler(4);
        let id = submit(&mut s, Priority::Interactive, Duration::ZERO);
        s.cancel(id).expect("first cancel");
        s.cancel(id).expect("second cancel is a no-op");
        assert!(s.next_task(Duration::ZERO).is_none());
    }

    #[test]
    fn cancelling_a_dispatched_task_is_reported_as_not_found() {
        let mut s = scheduler(4);
        let id = submit(&mut s, Priority::Interactive, Duration::ZERO);
        let dispatched = s.next_task(Duration::ZERO).expect("queued");
        assert_eq!(dispatched.id(), id);
        let err = s.cancel(id).expect_err("already dispatched");
        assert!(matches!(err.kind(), ErrorKind::NotFound { .. }));
    }

    #[test]
    fn cancelling_an_unknown_id_is_reported_as_not_found() {
        let mut s = scheduler(4);
        let err = s.cancel(TaskId::new()).expect_err("unknown id");
        assert!(matches!(err.kind(), ErrorKind::NotFound { kind, .. } if kind == "task"));
        assert!(!err.is_retryable());
    }

    #[test]
    fn scheduling_an_already_cancelled_task_is_refused() {
        let mut s = scheduler(4);
        let t = task(Priority::Input, Duration::ZERO);
        t.cancel();
        let err = s.schedule(t).expect_err("cancelled work cannot run");
        assert!(matches!(err.kind(), ErrorKind::Cancelled));
        assert_eq!(s.depth(), 0);
    }

    #[test]
    fn backpressure_is_reported_when_the_queue_is_full() {
        let mut s = scheduler(2);
        submit(&mut s, Priority::Input, Duration::ZERO);
        submit(&mut s, Priority::Input, Duration::ZERO);
        let err = s
            .schedule(task(Priority::Input, Duration::ZERO))
            .expect_err("queue is full");
        assert!(matches!(
            err.kind(),
            ErrorKind::Backpressure { channel, capacity: 2 }
                if channel == READY_QUEUE_CHANNEL
        ));
        assert!(err.is_retryable());
    }

    #[test]
    fn a_cancelled_backlog_frees_capacity_for_new_work() {
        let mut s = scheduler(2);
        let a = submit(&mut s, Priority::Interactive, Duration::ZERO);
        submit(&mut s, Priority::Interactive, Duration::ZERO);
        s.cancel(a).expect("queued");
        // Without the purge, the queue would still be "full" of dead work and the input
        // task below would be refused — which is precisely the starvation this avoids.
        submit(&mut s, Priority::Input, Duration::ZERO);
        assert_eq!(
            s.next_task(Duration::ZERO).map(|t| t.priority()),
            Some(Priority::Input)
        );
    }

    #[test]
    fn run_budgeted_stops_within_budget_and_reports_accurately() {
        let mut s = scheduler(32);
        for _ in 0..20 {
            s.schedule(Task::new(
                Priority::Interactive,
                Duration::ZERO,
                Work::timed("t", Duration::from_millis(2)),
            ))
            .expect("fits");
        }

        // Ten milliseconds of budget against 2 ms tasks: the frame runs exactly five.
        let budget = Duration::from_millis(10);
        let report = s.run_budgeted(Duration::ZERO, budget);

        assert_eq!(report.ran_count(), 5);
        assert_eq!(report.elapsed, Duration::from_millis(10));
        assert!(
            report.elapsed <= budget,
            "the frame must stay inside its budget"
        );
        assert_eq!(report.budget_remaining(), Duration::ZERO);
        assert_eq!(report.remaining, 15);
        assert!(report.budget_exhausted);
        assert!(!report.queue_empty);
        assert_eq!(report.ran_ids().count(), 5);
        assert_eq!(report.skipped_cancelled, 0);
        assert!(report.expired.is_empty());
    }

    #[test]
    fn run_budgeted_empties_the_queue_when_the_budget_allows() {
        let mut s = scheduler(8);
        for _ in 0..3 {
            submit(&mut s, Priority::Interactive, Duration::ZERO);
        }
        let report = s.run_budgeted(Duration::ZERO, Duration::from_millis(1_000));
        assert_eq!(report.ran_count(), 3);
        assert_eq!(report.remaining, 0);
        assert!(report.queue_empty);
        assert!(
            !report.budget_exhausted,
            "an empty queue did not exhaust the budget"
        );
    }

    #[test]
    fn run_budgeted_with_a_zero_budget_dispatches_nothing() {
        let mut s = scheduler(8);
        submit(&mut s, Priority::Input, Duration::ZERO);
        let report = s.run_budgeted(Duration::ZERO, Duration::ZERO);
        assert_eq!(report.ran_count(), 0);
        assert_eq!(report.remaining, 1);
        assert!(report.budget_exhausted);
    }

    /// Cooperative means the budget bounds *what the frame commits to*, so a task that
    /// costs more than the whole budget still runs — and the overshoot is bounded by
    /// exactly that task's cost. Refusing it would order the frame by cost rather than
    /// by priority.
    #[test]
    fn an_expensive_task_overshoots_the_budget_by_at_most_its_own_cost() {
        let mut s = scheduler(8);
        s.schedule(Task::new(
            Priority::Input,
            Duration::ZERO,
            Work::timed("expensive", Duration::from_millis(50)),
        ))
        .expect("fits");
        s.schedule(Task::new(
            Priority::Interactive,
            Duration::ZERO,
            Work::timed("cheap", MS),
        ))
        .expect("fits");

        let budget = Duration::from_millis(5);
        let report = s.run_budgeted(Duration::ZERO, budget);
        assert_eq!(report.ran_count(), 1);
        assert_eq!(report.elapsed, Duration::from_millis(50));
        assert!(report.elapsed <= budget + Duration::from_millis(50));
        assert_eq!(report.remaining, 1);
    }

    #[test]
    fn the_budget_never_reorders_work_by_cost() {
        let mut s = scheduler(8);
        let expensive = Task::new(
            Priority::Interactive,
            Duration::ZERO,
            Work::timed("expensive", Duration::from_millis(50)),
        );
        let expensive_id = expensive.id();
        s.schedule(expensive).expect("fits");
        let cheap_id = s
            .schedule(Task::new(
                Priority::Background,
                Duration::ZERO,
                Work::timed("cheap", MS),
            ))
            .expect("fits");

        // The budget is far too small for the expensive task, but refusing it would
        // order the frame by cost rather than by priority. It runs, consumes the whole
        // budget, and the frame stops there.
        let report = s.run_budgeted(Duration::ZERO, MS);
        assert_eq!(
            report.ran_ids().collect::<Vec<_>>(),
            vec![expensive_id],
            "an expensive interactive task must still precede cheap background work"
        );
        assert_eq!(report.elapsed, Duration::from_millis(50));
        assert_eq!(report.remaining, 1);
        assert!(
            s.queue().contains(cheap_id),
            "the cheap background task is still queued behind it, not run instead"
        );
    }

    #[test]
    fn run_budgeted_skips_cancelled_work_and_counts_it() {
        let mut s = scheduler(8);
        let a = submit(&mut s, Priority::Input, Duration::ZERO);
        let b = submit(&mut s, Priority::Input, Duration::ZERO);
        s.cancel(a).expect("queued");
        s.cancel(b).expect("queued");

        let report = s.run_budgeted(Duration::ZERO, Duration::from_millis(100));
        assert_eq!(report.ran_count(), 0);
        assert_eq!(report.skipped_cancelled, 2);
        assert!(report.queue_empty);
        assert!(!report.budget_exhausted);
    }

    #[test]
    fn deadline_expiry_withdraws_work_before_it_is_dispatched() {
        let mut s = scheduler(8);
        let stale = Task::new(
            Priority::Interactive,
            Duration::ZERO,
            Work::timed("stale", MS),
        )
        .with_deadline(Duration::from_millis(16));
        let stale_id = stale.id();
        let handle = stale.clone();
        let live = submit(&mut s, Priority::Input, Duration::ZERO);
        s.schedule(stale).expect("fits");

        let report = s.run_budgeted(Duration::from_millis(17), Duration::from_millis(100));
        assert_eq!(report.expired, vec![stale_id]);
        assert!(
            handle.is_cancelled(),
            "expired work is acknowledged as cancelled"
        );
        assert_eq!(report.ran_ids().collect::<Vec<_>>(), vec![live]);
    }

    #[test]
    fn a_deadline_is_the_last_instant_the_task_may_run() {
        let mut s = scheduler(4);
        let on_deadline = Task::new(Priority::Interactive, Duration::ZERO, Work::timed("x", MS))
            .with_deadline(Duration::from_millis(16));
        let id = on_deadline.id();
        s.schedule(on_deadline).expect("fits");
        assert!(s.expire(Duration::from_millis(16)).is_empty());
        assert_eq!(
            s.next_task(Duration::from_millis(16)).map(|t| t.id()),
            Some(id)
        );
    }

    #[test]
    fn expire_returns_tasks_in_submission_order_and_is_idempotent() {
        let mut s = scheduler(8);
        let mut ids = Vec::new();
        for _ in 0..3 {
            let t = Task::new(Priority::Background, Duration::ZERO, Work::timed("x", MS))
                .with_deadline(Duration::from_millis(10));
            ids.push(t.id());
            s.schedule(t).expect("fits");
        }
        assert_eq!(s.expire(Duration::from_millis(11)), ids);
        assert!(s.expire(Duration::from_millis(11)).is_empty());
        assert!(s.is_empty());
    }

    /// **Property test.** Under randomised schedules the scheduler must never dispatch
    /// a task twice and never dispatch a cancelled one. The generator is seeded
    /// explicitly, so a failure is reproducible from the printed seed.
    #[test]
    fn randomised_schedules_never_dispatch_twice_or_after_cancellation() {
        for seed in [1_u64, 2, 3, 0x00C0_FFEE, u64::MAX] {
            let mut rng = Rng::new(seed);
            let mut s = scheduler(64);
            let mut dispatched: HashSet<TaskId> = HashSet::new();
            let mut live: HashSet<TaskId> = HashSet::new();
            let mut cancelled: HashSet<TaskId> = HashSet::new();
            let mut clock = Duration::ZERO;

            for _ in 0..2_000 {
                clock += Duration::from_millis(rng.in_range(0, 3));
                match rng.below(10) {
                    0..=4 => {
                        let Some(class) = rng.choose(&Priority::ALL).copied() else {
                            continue;
                        };
                        // Ready-at sometimes predates submission, exercising the case
                        // where ageing is not redundant with submission order.
                        let ready_at = clock
                            .checked_sub(Duration::from_millis(rng.below(400)))
                            .unwrap_or(Duration::ZERO);
                        let mut t = Task::new(
                            class,
                            ready_at,
                            Work::timed("t", Duration::from_millis(rng.in_range(0, 3))),
                        );
                        if rng.below(8) == 0 {
                            t = t.with_deadline(clock + Duration::from_millis(rng.below(8)));
                        }
                        let id = t.id();
                        if rng.below(6) == 0 {
                            t.cancel();
                            cancelled.insert(id);
                        }
                        if s.schedule(t).is_ok() && !cancelled.contains(&id) {
                            live.insert(id);
                        }
                    }
                    5..=6 => {
                        let candidates: Vec<TaskId> = live.iter().copied().collect();
                        let Some(id) = rng.choose(&candidates).copied() else {
                            continue;
                        };
                        if s.cancel(id).is_ok() {
                            live.remove(&id);
                            cancelled.insert(id);
                        }
                    }
                    _ => {
                        let Some(task) = s.next_task(clock) else {
                            continue;
                        };
                        let id = task.id();
                        assert!(
                            !cancelled.contains(&id),
                            "seed {seed:#x}: dispatched cancelled task {id}"
                        );
                        assert!(
                            dispatched.insert(id),
                            "seed {seed:#x}: dispatched task {id} twice"
                        );
                        live.remove(&id);
                    }
                }
            }
            assert!(
                !dispatched.is_empty(),
                "seed {seed:#x}: nothing was dispatched"
            );
        }
    }

    /// The same guarantee across whole frames, not just single pops: a task cancelled
    /// at any point must never appear in any frame's dispatch list.
    #[test]
    fn randomised_frames_never_dispatch_a_cancelled_task() {
        let mut rng = Rng::new(0xF00D_BEEF);
        let mut s = scheduler(64);
        let mut dispatched: HashSet<TaskId> = HashSet::new();
        let mut cancelled: HashSet<TaskId> = HashSet::new();
        let mut clock = Duration::ZERO;

        for _ in 0..500 {
            clock += Duration::from_millis(rng.in_range(0, 4));
            for _ in 0..rng.in_range(0, 8) {
                let Some(class) = rng.choose(&Priority::ALL).copied() else {
                    continue;
                };
                let ready_at = clock
                    .checked_sub(Duration::from_millis(
                        rng.below(DEFAULT_AGEING_HORIZON.as_millis() as u64),
                    ))
                    .unwrap_or(Duration::ZERO);
                let t = Task::new(
                    class,
                    ready_at,
                    Work::timed("t", Duration::from_millis(rng.in_range(1, 4))),
                );
                let _ = s.schedule(t);
            }
            if rng.below(3) == 0 {
                for _ in 0..rng.in_range(1, 4) {
                    let t = Task::new(Priority::Interactive, clock, Work::timed("t", MS));
                    let id = t.id();
                    t.cancel();
                    cancelled.insert(id);
                    let _ = s.schedule(t);
                }
            }

            let report = s.run_budgeted(clock, Duration::from_millis(rng.in_range(0, 10)));
            for id in report.ran_ids() {
                assert!(
                    !cancelled.contains(&id),
                    "frame dispatched cancelled task {id}"
                );
                assert!(dispatched.insert(id), "frame dispatched task {id} twice");
            }
            assert_eq!(
                report.elapsed,
                report.ran.iter().map(|t| t.work().cost()).sum::<Duration>(),
                "the reported elapsed time must be the exact sum of dispatched costs"
            );
        }
    }

    #[test]
    fn an_empty_scheduler_reports_an_empty_frame() {
        let mut s = scheduler(4);
        let report = s.run_budgeted(Duration::ZERO, Duration::from_millis(16));
        assert_eq!(report.ran_count(), 0);
        assert_eq!(report.elapsed, Duration::ZERO);
        assert!(report.queue_empty);
        assert!(!report.budget_exhausted);
    }

    #[test]
    fn accessors_reflect_queue_state() {
        let mut s = scheduler(3);
        assert_eq!(s.capacity(), 3);
        assert!(s.is_empty());
        submit(&mut s, Priority::Input, Duration::ZERO);
        assert_eq!(s.depth(), 1);
        assert_eq!(s.queue().live_len(), 1);
        assert!(!s.is_empty());
        assert_eq!(s.purge_cancelled(), 0);
        assert_eq!(s.depth(), 1);
    }
}
