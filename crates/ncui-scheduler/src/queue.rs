//! The bounded ready queue.
//!
//! The queue holds tasks that are ready but not yet dispatched. It is *bounded* on
//! purpose: an unbounded ready queue turns a producer that outruns the frame loop into
//! unbounded memory growth, which spec 96 forbids. When the queue is full the push is
//! refused with [`ErrorKind::Backpressure`], which is a retryable classification, so the
//! producer's correct response is to come back later — not to grow the queue.
//!
//! # Why the pop is a linear scan and not a heap
//!
//! A binary heap is the obvious structure, and it is the wrong one here. Ageing means a
//! task's effective priority *changes while it sits in the queue*: a heap is an ordering
//! over keys captured at insert time, so a heap would have to be re-heapified on every
//! change or the ageing would never take effect. Re-ranking is inherent to this design,
//! not an optimisation that was skipped.
//!
//! The scan is linear in the number of *ready* tasks, which is exactly the quantity the
//! capacity bound caps. The bound is what makes the scan affordable: it is the same
//! bound that keeps memory flat and that turns a runaway producer into a backpressure
//! signal instead of an out-of-memory kill.

use core::cmp::Reverse;
use core::time::Duration;

use ncui_core::ErrorKind;
use ncui_core::NcError;
use ncui_core::Result;
use ncui_core::TaskId;
use ncui_core::error::{not_found, validation};

use crate::task::Task;

/// The channel name reported in [`ErrorKind::Backpressure`] and in metrics.
pub const READY_QUEUE_CHANNEL: &str = "scheduler.ready_queue";

/// One queued task plus its submission sequence number.
///
/// The sequence is queue metadata, not task metadata: it records *when this scheduler
/// accepted the task*, which is the tie-break that keeps equal-priority work in
/// first-in, first-out order. It deliberately is not stored on [`Task`], because a task
/// that has been dispatched has no place in a queue any more.
#[derive(Debug, Clone)]
struct Entry {
    task: Task,
    sequence: u64,
}

/// The outcome of asking the queue for its best task.
///
/// `PartialEq` is deliberately not derived: [`Task`] is not comparable (it holds shared
/// cancellation state, so equality would be a claim about a race, not about the value).
/// Compare the identifiers instead.
#[derive(Debug, Clone)]
pub struct PopResult {
    /// The best ready task, or `None` when the queue held nothing runnable.
    pub task: Option<Task>,
    /// How many cancelled entries were discarded while producing this result.
    ///
    /// Reported separately from "nothing was ready" so that a caller can distinguish an
    /// idle queue from a queue that was being drained by cancellations, which are very
    /// different states to report in a frame metric.
    pub discarded_cancelled: usize,
}

impl PopResult {
    /// Whether the queue had nothing runnable left.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.task.is_none()
    }

    /// The selected task, discarding the cancellation count.
    #[must_use]
    pub fn into_task(self) -> Option<Task> {
        self.task
    }
}

/// A bounded, priority-ordered store of ready tasks.
#[derive(Debug)]
pub struct ReadyQueue {
    entries: Vec<Entry>,
    capacity: usize,
    next_sequence: u64,
}

impl ReadyQueue {
    /// Creates an empty queue that holds at most `capacity` tasks.
    ///
    /// A zero capacity is rejected rather than clamped. Clamping would silently invent a
    /// bound the caller did not ask for, and the alternative — accepting it — produces a
    /// queue that refuses every push with `Backpressure { capacity: 0 }`, which reads
    /// like a bug somewhere else entirely. Failing here names the mistake.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] if `capacity` is zero.
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(
                validation("capacity", "a ready queue must hold at least one task")
                    .operation("queue::ReadyQueue::new"),
            );
        }
        Ok(Self {
            entries: Vec::new(),
            capacity,
            next_sequence: 0,
        })
    }

    /// The configured capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many entries are currently held, including entries that are cancelled but
    /// have not yet been purged.
    ///
    /// Use [`ReadyQueue::live_len`] for the number that matters to a producer deciding
    /// whether it can submit.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the queue holds no entries at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the queue is at capacity.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    /// The number of entries that are not cancelled.
    #[must_use]
    pub fn live_len(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| !entry.task.is_cancelled())
            .count()
    }

    /// Adds a task to the ready set.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Backpressure`] when the queue is already at capacity. The task is
    /// not consumed on failure beyond the caller's own ownership: the error carries the
    /// capacity so the producer can decide when to retry.
    pub fn push(&mut self, task: Task) -> Result<()> {
        if self.is_full() {
            return Err(capacity_exceeded(self.capacity));
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.entries.push(Entry { task, sequence });
        Ok(())
    }

    /// Selects and removes the best runnable task.
    ///
    /// Cancelled entries are discarded first, never returned: this is the point at which
    /// the "a cancelled task never runs" invariant is enforced. Among runnable tasks the
    /// highest [`PriorityClass::score`](crate::priority::PriorityClass::score) at `now`
    /// wins, and equal scores are broken by submission order, so equal-priority work is
    /// first-in, first-out.
    pub fn pop_best(&mut self, now: Duration) -> PopResult {
        let discarded_cancelled = self.purge_cancelled();
        let best = self
            .entries
            .iter()
            .enumerate()
            .max_by(|left, right| rank_at(left.1, now).cmp(&rank_at(right.1, now)))
            .map(|(index, _)| index);

        match best {
            Some(index) => {
                let entry = self.entries.swap_remove(index);
                PopResult {
                    task: Some(entry.task),
                    discarded_cancelled,
                }
            }
            None => PopResult {
                task: None,
                discarded_cancelled,
            },
        }
    }

    /// Discards every cancelled entry and reports how many were removed.
    ///
    /// Called at the start of every pop and every submit: a queue whose entries are all
    /// dead must not report itself as full, or a cancelled backlog would block the very
    /// interactive work that caused the cancellations.
    pub fn purge_cancelled(&mut self) -> usize {
        let before = self.entries.len();
        self.entries.retain(|entry| !entry.task.is_cancelled());
        before - self.entries.len()
    }

    /// Withdraws every task whose deadline has passed at `now`.
    ///
    /// Expired tasks are cancelled as well as removed, so that a holder of a clone sees
    /// the withdrawal, and are reported in submission order so the result is stable
    /// regardless of the internal layout. Calling this twice in a row is harmless: the
    /// second call finds nothing left to withdraw.
    pub fn expire(&mut self, now: Duration) -> Vec<TaskId> {
        let mut expired: Vec<(u64, TaskId)> = self
            .entries
            .iter()
            .filter(|entry| entry.task.is_expired(now))
            .map(|entry| (entry.sequence, entry.task.id()))
            .collect();
        expired.sort_unstable_by_key(|(sequence, _)| *sequence);

        let ids: Vec<TaskId> = expired.into_iter().map(|(_, id)| id).collect();
        for id in &ids {
            if let Some(entry) = self.find_mut(*id) {
                entry.task.cancel();
            }
        }
        self.entries.retain(|entry| !entry.task.is_expired(now));
        ids
    }

    /// Cancels the queued task with `id`.
    ///
    /// Idempotent: cancelling an already-cancelled task succeeds without changing
    /// anything. Cancelling a task that is no longer queued is reported as
    /// [`ErrorKind::NotFound`] — the scheduler does not track dispatched work, so there
    /// is nothing left to acknowledge and saying otherwise would be a lie the caller
    /// would act on.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotFound`] when no queued task has that id.
    pub fn cancel(&mut self, id: TaskId) -> Result<()> {
        match self.find_mut(id) {
            Some(entry) => {
                entry.task.cancel();
                Ok(())
            }
            None => Err(not_found("task", id).operation("queue::ReadyQueue::cancel")),
        }
    }

    /// Whether a task with `id` is currently queued.
    #[must_use]
    pub fn contains(&self, id: TaskId) -> bool {
        self.entries.iter().any(|entry| entry.task.id() == id)
    }

    fn find_mut(&mut self, id: TaskId) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|entry| entry.task.id() == id)
    }
}

/// The ordering key of one entry at `now`: highest score first, then earliest
/// submission first.
fn rank_at(entry: &Entry, now: Duration) -> (u32, Reverse<u64>) {
    (entry.task.effective_priority(now), Reverse(entry.sequence))
}

/// Builds the capacity error in one place so the channel name cannot drift from the one
/// reported in metrics.
fn capacity_exceeded(capacity: usize) -> NcError {
    NcError::new(ErrorKind::Backpressure {
        channel: READY_QUEUE_CHANNEL.to_owned(),
        capacity,
    })
    .operation("queue::ReadyQueue::push")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::priority::Priority;
    use crate::task::Work;

    fn task(priority: Priority, ready_at: Duration) -> Task {
        Task::new(priority, ready_at, Work::immediate("t"))
    }

    fn queue(capacity: usize) -> ReadyQueue {
        ReadyQueue::new(capacity).expect("non-zero capacity")
    }

    #[test]
    fn zero_capacity_is_rejected() {
        let err = ReadyQueue::new(0).expect_err("zero capacity must be rejected");
        assert!(matches!(err.kind(), ErrorKind::Validation { .. }));
    }

    #[test]
    fn push_reports_backpressure_with_the_configured_capacity() {
        let mut q = queue(2);
        q.push(task(Priority::Input, Duration::ZERO)).expect("fits");
        q.push(task(Priority::Input, Duration::ZERO)).expect("fits");
        let err = q
            .push(task(Priority::Input, Duration::ZERO))
            .expect_err("must be refused");
        assert!(matches!(
            err.kind(),
            ErrorKind::Backpressure { channel, capacity: 2 }
                if channel == READY_QUEUE_CHANNEL
        ));
        assert!(err.is_retryable());
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn pop_returns_the_highest_priority_first() {
        let mut q = queue(8);
        q.push(task(Priority::Maintenance, Duration::ZERO))
            .expect("fits");
        q.push(task(Priority::Input, Duration::ZERO)).expect("fits");
        q.push(task(Priority::VisibleUi, Duration::ZERO))
            .expect("fits");
        q.push(task(Priority::Interactive, Duration::ZERO))
            .expect("fits");
        let now = Duration::ZERO;
        assert_eq!(
            q.pop_best(now).into_task().map(|t| t.priority()),
            Some(Priority::Input)
        );
        assert_eq!(
            q.pop_best(now).into_task().map(|t| t.priority()),
            Some(Priority::VisibleUi)
        );
        assert_eq!(
            q.pop_best(now).into_task().map(|t| t.priority()),
            Some(Priority::Interactive)
        );
        assert_eq!(
            q.pop_best(now).into_task().map(|t| t.priority()),
            Some(Priority::Maintenance)
        );
        assert!(q.is_empty());
    }

    #[test]
    fn equal_priority_is_served_in_submission_order() {
        let mut q = queue(8);
        let mut ids = Vec::new();
        for _ in 0..5 {
            let t = task(Priority::Interactive, Duration::ZERO);
            ids.push(t.id());
            q.push(t).expect("fits");
        }
        let popped: Vec<TaskId> = std::iter::from_fn(|| q.pop_best(Duration::ZERO).into_task())
            .map(|t| t.id())
            .collect();
        assert_eq!(popped, ids);
    }

    #[test]
    fn ageing_reorders_equal_class_work_by_ready_time_not_submission() {
        let mut q = queue(8);
        // Submitted second, but ready much earlier: it must come first. A plain
        // first-in, first-out rule would get this wrong.
        let fresh = task(Priority::Background, Duration::from_millis(500));
        let fresh_id = fresh.id();
        q.push(fresh).expect("fits");
        let old = task(Priority::Background, Duration::ZERO);
        let old_id = old.id();
        q.push(old).expect("fits");

        let now = Duration::from_millis(500);
        assert_eq!(q.pop_best(now).into_task().map(|t| t.id()), Some(old_id));
        assert_eq!(q.pop_best(now).into_task().map(|t| t.id()), Some(fresh_id));
    }

    #[test]
    fn a_background_backlog_never_overtakes_interactive_work_in_the_queue() {
        let mut q = queue(4);
        let old_background = task(Priority::Background, Duration::ZERO);
        let old_background_id = old_background.id();
        q.push(old_background).expect("fits");
        let interactive = task(Priority::Interactive, Duration::from_secs(10));
        let interactive_id = interactive.id();
        q.push(interactive).expect("fits");

        let now = Duration::from_secs(10);
        assert_eq!(
            q.pop_best(now).into_task().map(|t| t.id()),
            Some(interactive_id),
            "a maximally-aged background task outran interactive work"
        );
        assert_eq!(
            q.pop_best(now).into_task().map(|t| t.id()),
            Some(old_background_id)
        );
    }

    #[test]
    fn cancelled_tasks_are_never_returned_even_at_the_head() {
        let mut q = queue(8);
        let head = task(Priority::Input, Duration::ZERO);
        let head_id = head.id();
        q.push(head).expect("fits");
        q.push(task(Priority::VisibleUi, Duration::ZERO))
            .expect("fits");
        q.cancel(head_id).expect("the task is queued");

        let popped = q.pop_best(Duration::ZERO);
        assert_eq!(popped.discarded_cancelled, 1);
        assert_eq!(
            popped.into_task().map(|t| t.priority()),
            Some(Priority::VisibleUi)
        );
        assert!(!q.contains(head_id));
    }

    #[test]
    fn cancel_is_idempotent_and_unknown_ids_are_not_found() {
        let mut q = queue(4);
        let t = task(Priority::Input, Duration::ZERO);
        let id = t.id();
        q.push(t).expect("fits");
        q.cancel(id).expect("first cancel");
        q.cancel(id).expect("second cancel is a no-op");

        let missing = TaskId::new();
        let err = q.cancel(missing).expect_err("unknown id");
        assert!(matches!(err.kind(), ErrorKind::NotFound { kind, .. } if kind == "task"));
        assert_eq!(err.code(), "not_found");
    }

    #[test]
    fn purge_frees_capacity_for_new_work() {
        let mut q = queue(2);
        let a = task(Priority::Input, Duration::ZERO);
        let b = task(Priority::Input, Duration::ZERO);
        let a_id = a.id();
        q.push(a).expect("fits");
        q.push(b).expect("fits");
        q.cancel(a_id).expect("queued");
        assert_eq!(q.purge_cancelled(), 1);
        assert_eq!(q.purge_cancelled(), 0, "purging twice removes nothing");
        assert!(!q.is_full());
        q.push(task(Priority::Input, Duration::ZERO))
            .expect("capacity was freed");
    }

    #[test]
    fn live_len_ignores_cancelled_entries() {
        let mut q = queue(4);
        let t = task(Priority::Input, Duration::ZERO);
        let id = t.id();
        q.push(t).expect("fits");
        assert_eq!(q.live_len(), 1);
        q.cancel(id).expect("queued");
        assert_eq!(q.len(), 1);
        assert_eq!(q.live_len(), 0);
    }

    #[test]
    fn expire_withdraws_in_submission_order_and_cancels_the_task() {
        let mut q = queue(8);
        let keep = task(Priority::Input, Duration::ZERO);
        let keep_id = keep.id();
        let late =
            task(Priority::Background, Duration::ZERO).with_deadline(Duration::from_millis(50));
        let late_id = late.id();
        let handle = late.clone();
        q.push(late.clone()).expect("fits");
        q.push(keep).expect("fits");

        let expired = q.expire(Duration::from_millis(51));
        assert_eq!(expired, vec![late_id]);
        assert!(
            handle.is_cancelled(),
            "expired work must be acknowledged as cancelled"
        );
        assert!(q.contains(keep_id));
        assert!(
            q.expire(Duration::from_millis(51)).is_empty(),
            "expiry is idempotent"
        );
        assert_eq!(
            q.pop_best(Duration::from_millis(51))
                .into_task()
                .map(|t| t.id()),
            Some(keep_id)
        );
    }

    #[test]
    fn an_undrained_queue_of_only_cancelled_work_reports_the_drain() {
        let mut q = queue(8);
        let a = task(Priority::Input, Duration::ZERO);
        let b = task(Priority::Input, Duration::ZERO);
        let (a_id, b_id) = (a.id(), b.id());
        q.push(a).expect("fits");
        q.push(b).expect("fits");
        q.cancel(a_id).expect("queued");
        q.cancel(b_id).expect("queued");

        let popped = q.pop_best(Duration::ZERO);
        assert_eq!(popped.discarded_cancelled, 2);
        assert!(popped.is_empty());
        assert!(q.is_empty());
    }
}
