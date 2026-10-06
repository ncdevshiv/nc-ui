//! The worker pool: bounded queues, lifecycle, cancellation and metrics.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ncui_core::limits::Limits;
use ncui_core::{ErrorKind, LimitsBuilder, NcError, Result, TaskId, WorkerId};
use ncui_observability::metrics::{Labels, MetricsRegistry, duration_millis};

use crate::task::{TaskOutcome, WorkerTask};

/// Metric names emitted by the pool. They are part of the observability contract.
const QUEUE_DEPTH: &str = "worker_queue_depth";
const QUEUE_HIGH_WATER: &str = "worker_queue_high_water";
const TASK_DURATION: &str = "worker_task_duration_ms";
const TASKS_TOTAL: &str = "worker_tasks_total";
const TASKS_FAILED: &str = "worker_task_failures_total";
const TASKS_EVICTED: &str = "worker_tasks_evicted_total";
const WORKERS_ACTIVE: &str = "workers_active";
/// Worker threads that outlived the shutdown deadline and were left to exit on their own.
const WORKERS_STALLED: &str = "worker_shutdown_stalled_total";

/// Locking that survives a poisoned mutex.
///
/// A poisoned mutex means some other thread panicked while holding the lock. Everything
/// behind these locks is a counter, a queue, or a state word, so recovering is safe and
/// strictly better than propagating the poison: losing the whole pool because an unrelated
/// task panicked would turn one bug into a total outage.
trait LockExt<T> {
    /// Acquires the lock, recovering from poisoning.
    fn lock_recovering(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_recovering(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What a pool does when a queue is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressurePolicy {
    /// Refuse the submission. The caller learns immediately that the pool is saturated.
    ///
    /// The default, because a silent drop of indexing work produces a subtly wrong
    /// application rather than an obvious failure.
    Reject,
    /// Evict the oldest queued task to make room.
    ///
    /// Only appropriate when the work is idempotent and older work is strictly less
    /// valuable than newer work, such as re-indexing a file that just changed.
    DropOldest,
}

/// The lifecycle state of a single worker (spec 95).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    /// Constructed but not yet accepting work.
    Created,
    /// Accepting work and executing it.
    Active,
    /// Alive but not executing; queued work accumulates up to the queue bound.
    Suspended,
    /// No longer accepting new work.
    Stopped,
    /// Torn down. Terminal: a destroyed worker never becomes active again.
    Destroyed,
}

impl WorkerState {
    /// Whether `self -> next` is a legal transition.
    ///
    /// Rejecting illegal transitions rather than tolerating them is the point: a worker
    /// that silently resumes after being destroyed hides a lifecycle defect.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        use WorkerState::{Active, Created, Destroyed, Stopped, Suspended};
        matches!(
            (self, next),
            (Created, Active | Stopped | Destroyed)
                | (Active | Suspended, Active | Suspended | Stopped | Destroyed)
                | (Stopped, Destroyed)
        )
    }

    /// Whether the worker executes tasks in this state.
    #[must_use]
    pub const fn is_executing(self) -> bool {
        matches!(self, Self::Active)
    }
}

impl fmt::Display for WorkerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Created => "created",
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Stopped => "stopped",
            Self::Destroyed => "destroyed",
        };
        f.write_str(name)
    }
}

/// Observable state of one worker, for `ncui.workers.inspect`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkerSnapshot {
    /// Which worker this describes.
    pub id: WorkerId,
    /// Current lifecycle state.
    pub state: WorkerState,
    /// Tasks that ran to completion.
    pub completed: u64,
    /// Tasks that returned a failure.
    pub failed: u64,
    /// Tasks skipped because they were cancelled.
    pub cancelled: u64,
}

/// A task waiting in the queue, with the instant it arrived.
struct QueuedTask {
    task: WorkerTask,
    submitted_at: Instant,
}

/// Per-thread counters, read by the inspector without disturbing the worker.
#[derive(Debug)]
struct WorkerRuntime {
    id: WorkerId,
    state: Mutex<WorkerState>,
    completed: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
}

struct Shared {
    queue: Mutex<VecDeque<QueuedTask>>,
    signal: Condvar,
    cancelled: Mutex<HashSet<TaskId>>,
    shutdown: AtomicBool,
    high_water: AtomicU64,
    metric_failures: AtomicU64,
}

impl Shared {
    fn lock_queue(&self) -> std::sync::MutexGuard<'_, VecDeque<QueuedTask>> {
        self.queue.lock_recovering()
    }

    fn lock_cancelled(&self) -> std::sync::MutexGuard<'_, HashSet<TaskId>> {
        self.cancelled.lock_recovering()
    }
}

/// A pool of native workers sharing one bounded queue.
pub struct WorkerPool {
    shared: Arc<Shared>,
    workers: Vec<Arc<WorkerRuntime>>,
    limits: Limits,
    metrics: Arc<MetricsRegistry>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl fmt::Debug for WorkerPool {
    /// Reports the pool's observable shape rather than its internals.
    ///
    /// A `Debug` that printed every worker and queue would itself become a way to stall
    /// the pool, so this is deliberately a summary.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (completed, failed, cancelled) = self.task_counters();
        f.debug_struct("WorkerPool")
            .field("workers", &self.workers.len())
            .field("queue_depth", &self.queue_depth())
            .field("queue_high_water", &self.queue_high_water())
            .field("completed", &completed)
            .field("failed", &failed)
            .field("cancelled", &cancelled)
            .finish_non_exhaustive()
    }
}

impl WorkerPool {
    /// Starts a pool sized from `limits`, recording metrics into `metrics`.
    ///
    /// Returns `Err` rather than clamping when the limits are incoherent, or when the
    /// operating system refuses the worker threads. A pool that silently starts with fewer
    /// workers than configured would under-deliver the concurrency it promised, so thread
    /// creation failure is an error the caller must handle.
    pub fn new(limits: Limits, metrics: Arc<MetricsRegistry>) -> Result<Self> {
        limits.validate()?;
        if limits.max_workers == 0 {
            return Err(NcError::new(ErrorKind::Validation {
                field: "max_workers".to_owned(),
                reason: "a pool needs at least one worker".to_owned(),
            })
            .operation("workers::pool::new"));
        }

        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::with_capacity(limits.max_worker_queue_depth)),
            signal: Condvar::new(),
            cancelled: Mutex::new(HashSet::new()),
            shutdown: AtomicBool::new(false),
            high_water: AtomicU64::new(0),
            metric_failures: AtomicU64::new(0),
        });

        let mut workers = Vec::with_capacity(limits.max_workers);
        let mut threads = Vec::with_capacity(limits.max_workers);

        for _ in 0..limits.max_workers {
            let runtime = Arc::new(WorkerRuntime {
                id: WorkerId::new(),
                state: Mutex::new(WorkerState::Created),
                completed: AtomicU64::new(0),
                failed: AtomicU64::new(0),
                cancelled: AtomicU64::new(0),
            });
            let handle = {
                let shared = Arc::clone(&shared);
                let metrics = Arc::clone(&metrics);
                let thread_runtime = Arc::clone(&runtime);
                let name = format!("ncui-worker-{}", runtime.id);
                std::thread::Builder::new().name(name).spawn(move || {
                    worker_loop(&shared, &metrics, &thread_runtime);
                })?
            };
            threads.push(handle);
            workers.push(runtime);
        }

        let pool = Self {
            shared,
            workers,
            limits,
            metrics,
            threads: Mutex::new(threads),
        };
        pool.set_all_state(WorkerState::Active);
        pool.record_gauge(WORKERS_ACTIVE, pool.workers.len() as f64);
        Ok(pool)
    }

    /// Starts a pool with limits chosen for `workers` count, for tests and small tools.
    pub fn with_worker_count(workers: usize, metrics: Arc<MetricsRegistry>) -> Result<Self> {
        let limits = LimitsBuilder::new()
            .max_workers(workers)
            .max_worker_queue_depth(8)
            .build()?;
        Self::new(limits, metrics)
    }

    /// Submits a task.
    ///
    /// `policy` decides what happens when the queue is full. Every rejection is an error
    /// the caller can observe, never a silent drop.
    pub fn submit(&self, task: WorkerTask, policy: BackpressurePolicy) -> Result<TaskId> {
        if self.shared.shutdown.load(Ordering::Acquire) {
            return Err(NcError::new(ErrorKind::Conflict {
                reason: "the pool is shutting down and accepts no further work".to_owned(),
            })
            .operation("workers::submit"));
        }
        if task.payload_bytes > self.limits.max_worker_payload_bytes {
            return Err(NcError::new(ErrorKind::LimitExceeded {
                resource: "worker.payload.bytes".to_owned(),
                limit: self.limits.max_worker_payload_bytes as u64,
                observed: task.payload_bytes as u64,
            })
            .operation("workers::submit"));
        }

        let id = task.id;
        let mut evicted = false;
        {
            let mut queue = self.shared.lock_queue();
            if queue.len() >= self.limits.max_worker_queue_depth {
                match policy {
                    BackpressurePolicy::Reject => {
                        return Err(NcError::new(ErrorKind::Backpressure {
                            channel: "worker.queue".to_owned(),
                            capacity: self.limits.max_worker_queue_depth,
                        })
                        .operation("workers::submit"));
                    }
                    BackpressurePolicy::DropOldest => {
                        // An evicted task never reaches `worker_loop`, which is the only
                        // place a cancellation marker is removed. Dropping the task
                        // therefore has to drop its marker too, or the entry survives for
                        // the life of the process: `cancelled` is a plain `HashSet` with no
                        // bound in `Limits`, so a cancel-then-evict cycle leaked one entry
                        // each time and nothing ever reclaimed it.
                        if let Some(dropped) = queue.pop_front() {
                            self.shared.lock_cancelled().remove(&dropped.task.id);
                        }
                        evicted = true;
                    }
                }
            }
            queue.push_back(QueuedTask {
                task,
                submitted_at: Instant::now(),
            });
            let depth = queue.len() as u64;
            self.shared.high_water.fetch_max(depth, Ordering::Relaxed);
            self.record_gauge(QUEUE_DEPTH, queue.len() as f64);
        }
        self.shared.signal.notify_one();

        self.record_gauge(QUEUE_HIGH_WATER, self.queue_high_water() as f64);
        if evicted {
            // Eviction is reported rather than silent, so a caller watching the queue
            // depth can tell work was discarded on purpose.
            self.record_counter(TASKS_EVICTED, 1, &Labels::new());
        }
        Ok(id)
    }

    /// Cancels a task that has not started executing.
    ///
    /// Returns `Ok(false)` when the task has already been dequeued, because at that point
    /// the only honest answer is that cancellation lost the race — reporting success there
    /// would be a lie the caller cannot detect.
    pub fn cancel(&self, task_id: TaskId) -> Result<bool> {
        let queue = self.shared.lock_queue();
        if queue.iter().any(|queued| queued.task.id == task_id) {
            self.shared.lock_cancelled().insert(task_id);
            self.record_gauge(QUEUE_DEPTH, queue.len() as f64);
            return Ok(true);
        }
        Ok(false)
    }

    /// How many tasks are waiting.
    #[must_use]
    pub fn queue_depth(&self) -> usize {
        self.shared.lock_queue().len()
    }

    /// How many tasks are currently marked cancelled.
    ///
    /// Observable because this set is the one resource in the pool with **no bound in
    /// [`Limits`]**, so a path that added a marker without removing it grew without limit and
    /// nothing reported it. The eviction path had exactly that defect: a cancelled task
    /// popped by `DropOldest` never reached `worker_loop`, which is the only place markers
    /// were removed, so the entry survived for the life of the process.
    ///
    /// A non-zero count while nothing is queued means markers are stranded and the caller has
    /// found a leak.
    #[must_use]
    pub fn cancellation_marker_count(&self) -> usize {
        self.shared.lock_cancelled().len()
    }

    /// The deepest the queue has ever been.
    #[must_use]
    pub fn queue_high_water(&self) -> u64 {
        self.shared.high_water.load(Ordering::Relaxed)
    }

    /// The number of workers in the pool.
    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Total `(completed, failed, cancelled)` across every worker.
    #[must_use]
    pub fn task_counters(&self) -> (u64, u64, u64) {
        let sum = |counter: &dyn Fn(&Arc<WorkerRuntime>) -> u64| -> u64 {
            self.workers.iter().map(counter).sum()
        };
        (
            sum(&|w| w.completed.load(Ordering::Relaxed)),
            sum(&|w| w.failed.load(Ordering::Relaxed)),
            sum(&|w| w.cancelled.load(Ordering::Relaxed)),
        )
    }

    /// How many metric writes failed.
    ///
    /// Exposed rather than swallowed: a pool whose instrumentation has silently stopped
    /// working is exactly the kind of failure that hides for months.
    #[must_use]
    pub fn metric_failures(&self) -> u64 {
        self.shared.metric_failures.load(Ordering::Relaxed)
    }

    /// A snapshot of every worker, for the agent inspection API.
    #[must_use]
    pub fn snapshot(&self) -> Vec<WorkerSnapshot> {
        self.workers
            .iter()
            .map(|worker| WorkerSnapshot {
                id: worker.id,
                state: *worker.state.lock_recovering(),
                completed: worker.completed.load(Ordering::Relaxed),
                failed: worker.failed.load(Ordering::Relaxed),
                cancelled: worker.cancelled.load(Ordering::Relaxed),
            })
            .collect()
    }

    /// Moves every worker to `next`, validating the transition first.
    ///
    /// The whole call is rejected if any worker cannot make the transition, so the pool is
    /// never left in a half-applied state.
    pub fn transition(&self, next: WorkerState) -> Result<()> {
        for worker in &self.workers {
            let current = *worker.state.lock_recovering();
            if !current.can_transition_to(next) {
                return Err(NcError::new(ErrorKind::Conflict {
                    reason: format!("a worker in state {current} cannot become {next}"),
                })
                .operation("workers::transition"));
            }
        }

        self.set_all_state(next);
        if next == WorkerState::Stopped {
            self.shared.shutdown.store(true, Ordering::Release);
        }
        // Any state change must wake every worker: a suspended worker must re-evaluate
        // whether it may now run, and a stopped one must observe the shutdown flag.
        self.shared.signal.notify_all();
        Ok(())
    }

    /// Stops accepting work, lets in-flight tasks finish, and joins the threads.
    ///
    /// Bounded by [`Limits::worker_shutdown_timeout_ms`]. A task payload is a plain closure
    /// with no cancellation token, so a task that blocks forever would otherwise block
    /// shutdown forever — and since [`Drop`] calls this, it would block the drop too,
    /// including at process exit. Rust cannot kill a thread, so past the deadline shutdown
    /// stops waiting, reports the threads that did not stop, and leaves them to exit when
    /// their task returns. Nothing is aborted: a task that finishes normally still runs to
    /// completion, it is simply no longer waited for.
    pub fn shutdown(&self) -> ShutdownReport {
        if !self.shared.shutdown.swap(true, Ordering::AcqRel) {
            self.set_all_state(WorkerState::Stopped);
            self.record_gauge(WORKERS_ACTIVE, 0.0);
        }
        self.shared.signal.notify_all();

        let handles = std::mem::take(&mut *self.threads.lock_recovering());
        let deadline =
            Instant::now() + Duration::from_millis(self.limits.worker_shutdown_timeout_ms);
        let mut report = ShutdownReport {
            threads: handles.len(),
            ..ShutdownReport::default()
        };
        for handle in handles {
            // `is_finished` rather than a bare `join`, because `join` has no deadline and
            // the whole point of this change is to stop having an unbounded wait.
            while !handle.is_finished() {
                if Instant::now() >= deadline {
                    report.stalled += 1;
                    // Dropping a `JoinHandle` detaches the thread rather than blocking on it.
                    // The task keeps running and the thread exits on its own; the counter and
                    // the report are how a caller learns shutdown returned before the pool did.
                    self.record_counter(WORKERS_STALLED, 1, &Labels::new());
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            if handle.is_finished() {
                // A panicking task thread is reported rather than swallowed: the task
                // payload is user-supplied, so a panic there is a real signal worth counting.
                if handle.join().is_err() {
                    report.panicked += 1;
                    self.shared.metric_failures.fetch_add(1, Ordering::Relaxed);
                } else {
                    report.joined += 1;
                }
            }
        }
        self.set_all_state(WorkerState::Destroyed);
        report
    }

    fn set_all_state(&self, state: WorkerState) {
        for worker in &self.workers {
            *worker.state.lock_recovering() = state;
        }
    }

    fn record_counter(&self, name: &str, by: u64, labels: &Labels) {
        if self.metrics.counter_inc(name, labels, by).is_err() {
            self.shared.metric_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_gauge(&self, name: &str, value: f64) {
        if self.metrics.gauge_set(name, &Labels::new(), value).is_err() {
            self.shared.metric_failures.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// What a [`WorkerPool::shutdown`] actually managed to do.
///
/// Reported rather than assumed, because the answer is not always "everything stopped": a
/// task that outlasts the deadline leaves its thread running, and a caller that needs to
/// know should not have to infer it from a missing panic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Worker threads the pool was responsible for.
    pub threads: usize,
    /// Threads observed finished and joined.
    pub joined: usize,
    /// Threads that ended in a panic, which the pool counts rather than swallows.
    pub panicked: usize,
    /// Threads still running when the deadline passed and left to exit on their own.
    pub stalled: usize,
}

impl ShutdownReport {
    /// Whether every worker thread stopped before the deadline.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.stalled == 0
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        // The report is discarded: `Drop` has no channel to return it through. A caller
        // that needs the answer calls `shutdown` explicitly before dropping the pool.
        let _report = self.shutdown();
    }
}

/// The body of a worker thread.
///
/// It borrows the pool's shared state; the spawning closure owns the `Arc`s and outlives
/// the call, which is why ownership does not have to be threaded through here.
fn worker_loop(shared: &Arc<Shared>, metrics: &MetricsRegistry, runtime: &Arc<WorkerRuntime>) {
    loop {
        let Some(queued) = take_task(shared, runtime) else {
            return;
        };

        let was_cancelled = shared.lock_cancelled().remove(&queued.task.id);
        let task = queued.task;
        let operation = task.operation.clone();

        if was_cancelled {
            // A cancelled task never executes, so it is not timed: reporting a duration for
            // work that did not happen would corrupt the latency histogram.
            runtime.cancelled.fetch_add(1, Ordering::Relaxed);
        } else {
            let started = Instant::now();
            let outcome = (task.run)();
            let elapsed = duration_millis(started.elapsed());

            let labels = operation_labels(&operation);
            let _ = duration_from_submission(queued.submitted_at);
            if metrics
                .histogram_observe(TASK_DURATION, &labels, elapsed)
                .is_err()
            {
                shared.metric_failures.fetch_add(1, Ordering::Relaxed);
            }
            if metrics.counter_inc(TASKS_TOTAL, &labels, 1).is_err() {
                shared.metric_failures.fetch_add(1, Ordering::Relaxed);
            }

            match &outcome {
                TaskOutcome::Completed { .. } => {
                    runtime.completed.fetch_add(1, Ordering::Relaxed);
                }
                TaskOutcome::Cancelled => {
                    runtime.cancelled.fetch_add(1, Ordering::Relaxed);
                }
                TaskOutcome::Failed { .. } => {
                    runtime.failed.fetch_add(1, Ordering::Relaxed);
                    if metrics.counter_inc(TASKS_FAILED, &labels, 1).is_err() {
                        shared.metric_failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        let depth = shared.lock_queue().len();
        if metrics
            .gauge_set(QUEUE_DEPTH, &Labels::new(), depth as f64)
            .is_err()
        {
            shared.metric_failures.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// How long a task waited in the queue before it started.
///
/// Queue residency is the number that explains "the pool looks idle but the UI is
/// sluggish", so it is recorded rather than discarded.
fn duration_from_submission(submitted: Instant) -> u128 {
    submitted.elapsed().as_millis()
}

/// Blocks until a task is available and the worker may execute, or the pool is shutting
/// down. Returns `None` when the worker should exit.
fn take_task(shared: &Arc<Shared>, runtime: &Arc<WorkerRuntime>) -> Option<QueuedTask> {
    let mut queue = shared.lock_queue();
    loop {
        let state = *runtime.state.lock_recovering();
        let shutting_down = shared.shutdown.load(Ordering::Acquire);

        if shutting_down && queue.is_empty() {
            return None;
        }
        if state.is_executing()
            && let Some(queued) = queue.pop_front()
        {
            return Some(queued);
        }
        if shutting_down {
            return None;
        }
        queue = shared
            .signal
            .wait(queue)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

fn operation_labels(operation: &str) -> Labels {
    Labels::from_pairs([("operation", operation)]).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn metrics() -> Arc<MetricsRegistry> {
        Arc::new(MetricsRegistry::new(Limits::default()).expect("registry"))
    }

    fn pool_with(workers: usize, queue_depth: usize) -> WorkerPool {
        let limits = LimitsBuilder::new()
            .max_workers(workers)
            .max_worker_queue_depth(queue_depth)
            .build()
            .expect("limits");
        WorkerPool::new(limits, metrics()).expect("pool")
    }

    /// Polls `condition` until it holds or the deadline passes.
    ///
    /// Polling with a short sleep rather than one long sleep keeps the suite fast when the
    /// condition is already true, and the timeout gives a clear failure instead of a hang.
    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if condition() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn a_submitted_task_runs_and_is_counted() {
        let pool = pool_with(2, 8);
        let (tx, rx) = mpsc::channel();
        let id = pool
            .submit(
                WorkerTask::new("index", 0, move || {
                    let _ = tx.send(42);
                    TaskOutcome::Completed { bytes: 1 }
                }),
                BackpressurePolicy::Reject,
            )
            .expect("submission should succeed");

        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).ok(), Some(42));
        wait_until("the completed counter to advance", || {
            pool.task_counters().0 == 1
        });
        assert_ne!(id.as_raw(), 0, "a submission returns a real id");
    }

    #[test]
    fn a_cancelled_task_never_executes() {
        let pool = pool_with(1, 8);
        // Suspend first so the task cannot be dequeued before it is cancelled.
        pool.transition(WorkerState::Suspended).expect("suspend");

        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let id = pool
            .submit(
                WorkerTask::new("search", 0, move || {
                    flag.store(true, Ordering::SeqCst);
                    TaskOutcome::Completed { bytes: 0 }
                }),
                BackpressurePolicy::Reject,
            )
            .expect("submission should succeed");

        assert!(pool.cancel(id).expect("cancel should succeed"));
        pool.transition(WorkerState::Active).expect("resume");

        wait_until("the cancelled counter to advance", || {
            pool.task_counters().2 == 1
        });
        assert!(
            !ran.load(Ordering::SeqCst),
            "a cancelled task must not execute"
        );
    }

    #[test]
    fn cancelling_an_unknown_task_reports_that_it_lost_the_race() {
        let pool = pool_with(1, 8);
        assert!(
            !pool.cancel(TaskId::new()).expect("cancel should not error"),
            "cancelling an unknown task must report false, not pretend success"
        );
    }

    #[test]
    fn a_full_queue_rejects_rather_than_dropping_silently() {
        let pool = pool_with(1, 2);
        pool.transition(WorkerState::Suspended).expect("suspend");

        for _ in 0..2 {
            pool.submit(
                WorkerTask::new("fill", 0, || TaskOutcome::Completed { bytes: 0 }),
                BackpressurePolicy::Reject,
            )
            .expect("queue has room");
        }
        let err = pool
            .submit(
                WorkerTask::new("overflow", 0, || TaskOutcome::Completed { bytes: 0 }),
                BackpressurePolicy::Reject,
            )
            .expect_err("a full queue must reject");
        assert_eq!(err.code(), "backpressure");
        assert_eq!(pool.queue_depth(), 2, "the bound must still hold");
    }

    #[test]
    fn drop_oldest_keeps_the_bound_and_reports_the_eviction() {
        let pool = pool_with(1, 2);
        pool.transition(WorkerState::Suspended).expect("suspend");

        for _ in 0..2 {
            pool.submit(
                WorkerTask::new("fill", 0, || TaskOutcome::Completed { bytes: 0 }),
                BackpressurePolicy::DropOldest,
            )
            .expect("queue has room");
        }
        pool.submit(
            WorkerTask::new("newest", 0, || TaskOutcome::Completed { bytes: 0 }),
            BackpressurePolicy::DropOldest,
        )
        .expect("drop-oldest makes room");

        assert_eq!(pool.queue_depth(), 2, "the bound must still hold");
        let registry = pool.metrics.clone();
        wait_until("the eviction counter to advance", || {
            registry.counter_value(TASKS_EVICTED, &Labels::new()) == Some(1)
        });
    }

    #[test]
    fn an_oversized_payload_is_refused_before_it_is_queued() {
        let limits = LimitsBuilder::new()
            .max_workers(1)
            .max_worker_payload_bytes(16)
            .build()
            .expect("limits");
        let pool = WorkerPool::new(limits, metrics()).expect("pool");
        let err = pool
            .submit(
                WorkerTask::new("big", 1_000, || TaskOutcome::Completed { bytes: 0 }),
                BackpressurePolicy::Reject,
            )
            .expect_err("an oversized payload must be refused");
        assert_eq!(err.code(), "limit_exceeded");
        assert_eq!(pool.queue_depth(), 0, "nothing may be queued");
    }

    #[test]
    fn queue_depth_never_exceeds_the_configured_bound() {
        let pool = pool_with(2, 4);
        pool.transition(WorkerState::Suspended).expect("suspend");

        for _ in 0..64 {
            let _ = pool.submit(
                WorkerTask::new("flood", 0, || TaskOutcome::Completed { bytes: 0 }),
                BackpressurePolicy::Reject,
            );
        }
        assert!(
            pool.queue_depth() <= 4,
            "the queue bound must never be exceeded"
        );
        assert!(pool.queue_high_water() <= 4);
    }

    #[test]
    fn illegal_lifecycle_transitions_are_rejected() {
        assert!(WorkerState::Created.can_transition_to(WorkerState::Active));
        assert!(WorkerState::Active.can_transition_to(WorkerState::Suspended));
        assert!(WorkerState::Suspended.can_transition_to(WorkerState::Active));
        assert!(WorkerState::Stopped.can_transition_to(WorkerState::Destroyed));

        // A destroyed worker is terminal, and a stopped one may not run again.
        assert!(!WorkerState::Destroyed.can_transition_to(WorkerState::Active));
        assert!(!WorkerState::Stopped.can_transition_to(WorkerState::Active));
    }

    #[test]
    fn a_destroyed_worker_cannot_be_reactivated() {
        let pool = pool_with(1, 8);
        pool.transition(WorkerState::Stopped).expect("stop");
        pool.transition(WorkerState::Destroyed).expect("destroy");
        let err = pool
            .transition(WorkerState::Active)
            .expect_err("a destroyed worker must stay destroyed");
        assert_eq!(err.code(), "conflict");
    }

    #[test]
    fn a_failing_task_is_counted_separately_from_a_completed_one() {
        let pool = pool_with(1, 8);
        pool.submit(
            WorkerTask::new("boom", 0, || TaskOutcome::Failed {
                code: "io".to_owned(),
                message: "disk went away".to_owned(),
            }),
            BackpressurePolicy::Reject,
        )
        .expect("submission should succeed");

        wait_until("the failure counter to advance", || {
            pool.task_counters().1 == 1
        });
        assert_eq!(pool.task_counters().0, 0, "a failure is not a completion");
    }

    #[test]
    fn task_durations_are_recorded_as_a_histogram() {
        let registry = metrics();
        let pool = WorkerPool::with_worker_count(1, Arc::clone(&registry)).expect("pool");
        for _ in 0..4 {
            pool.submit(
                WorkerTask::new("timed", 0, || TaskOutcome::Completed { bytes: 0 }),
                BackpressurePolicy::Reject,
            )
            .expect("submission should succeed");
        }
        wait_until("all four tasks to finish", || pool.task_counters().0 == 4);

        let labels = Labels::from_pairs([("operation", "timed")]).expect("labels");
        let stats = registry
            .histogram_stats(TASK_DURATION, &labels)
            .expect("a duration histogram should exist for the task");
        assert_eq!(stats.count, 4, "every task duration should be observed");
        assert_eq!(
            pool.metric_failures(),
            0,
            "no metric write should have failed"
        );
    }

    #[test]
    fn snapshots_report_every_worker_with_distinct_ids() {
        let pool = pool_with(3, 8);
        let snapshot = pool.snapshot();
        assert_eq!(snapshot.len(), 3);
        assert!(snapshot.iter().all(|w| w.state == WorkerState::Active));
        let unique: HashSet<_> = snapshot.iter().map(|w| w.id).collect();
        assert_eq!(unique.len(), 3, "worker ids must be distinct");
    }

    #[test]
    fn submitting_after_shutdown_is_a_conflict() {
        let pool = pool_with(1, 8);
        pool.shutdown();
        let err = pool
            .submit(
                WorkerTask::new("late", 0, || TaskOutcome::Completed { bytes: 0 }),
                BackpressurePolicy::Reject,
            )
            .expect_err("a shut-down pool must refuse work");
        assert_eq!(err.code(), "conflict");
    }

    /// A cancelled task that is later evicted must not leave its marker behind.
    ///
    /// `cancelled` is a plain `HashSet` with no bound in `Limits`, and its only removal was
    /// in `worker_loop` — reached only by tasks that get *dequeued*. Under `DropOldest` a
    /// queued task is popped and discarded without ever getting there, so every
    /// cancel-then-evict cycle leaked one entry for the life of the process, bounded by
    /// nothing. The eviction path now drops the marker with the task.
    #[test]
    fn an_evicted_cancellation_does_not_leak_its_marker() {
        let pool = pool_with(1, 2);
        // Suspend the worker so tasks queue instead of running, making eviction reachable.
        pool.transition(WorkerState::Suspended).expect("suspend");

        for round in 0..50 {
            let task = WorkerTask::new("cancellable", 0, || TaskOutcome::Completed { bytes: 0 });
            let id = task.id;
            // `DropOldest` throughout: the queue is already at its bound, so a `Reject`
            // submission would fail and the marker under test would never be created.
            pool.submit(task, BackpressurePolicy::DropOldest)
                .expect("queued");
            assert!(pool.cancel(id).expect("cancel reported"), "round {round}");
            // Push past the bound so the cancelled task is evicted before it is dequeued,
            // which is the path that used to strand its marker.
            for _ in 0..4 {
                pool.submit(
                    WorkerTask::new("flood", 0, || TaskOutcome::Completed { bytes: 0 }),
                    BackpressurePolicy::DropOldest,
                )
                .expect("queued");
            }
        }

        // The count is taken once into a local. Taking it again inside the assertion's
        // message would re-acquire the same non-reentrant mutex while the first guard was
        // still live — a deadlock that fires only when the assertion *fails*, turning a
        // clear failure into a hang.
        let leaked = pool.shared.lock_cancelled().len();
        assert_eq!(
            leaked, 0,
            "every cancelled task was also evicted, so no marker may survive;              {leaked} leaked"
        );
    }

    /// Shutdown returns even when a task never finishes, and says so.
    ///
    /// A task payload is a bare closure with no cancellation token, so the pool cannot
    /// interrupt one. `shutdown` used to `join` unconditionally, so a task that blocked
    /// blocked shutdown — and since `Drop` shuts down, it blocked the drop, including at
    /// process exit. The deadline makes the wait bounded and the report honest about it.
    #[test]
    fn shutdown_reports_a_worker_that_outlives_the_deadline() {
        let limits = LimitsBuilder::new()
            .max_workers(1)
            .max_worker_queue_depth(4)
            .worker_shutdown_timeout_ms(150)
            .build()
            .expect("limits");
        let pool = WorkerPool::new(limits, metrics()).expect("pool");

        // A task that never returns on its own, released by a flag the test controls.
        let gate = Arc::new(AtomicBool::new(false));
        let release = Arc::clone(&gate);
        let task = WorkerTask::new("blocks", 0, move || {
            while !release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            TaskOutcome::Completed { bytes: 0 }
        });
        pool.submit(task, BackpressurePolicy::Reject)
            .expect("submitted");
        // Shutdown must find the task *running*. A task still sitting in the queue is
        // dropped rather than executed, which would stall nothing and prove nothing.
        std::thread::sleep(Duration::from_millis(100));

        let started = Instant::now();
        let report = pool.shutdown();
        let elapsed = started.elapsed();

        // The deadline is 150ms; an unbounded join would have hung here until the release
        // below, which never runs before this assertion.
        assert!(
            elapsed < Duration::from_secs(5),
            "shutdown took {elapsed:?}, so the deadline is not being honoured"
        );
        assert_eq!(report.stalled, 1, "the blocked worker must be reported");
        assert!(
            !report.is_complete(),
            "a stalled worker means an incomplete shutdown"
        );

        // Release the task so the detached thread can exit and the test leaves nothing behind.
        gate.store(true, Ordering::Release);
    }

    /// A pool whose tasks all finish shuts down completely.
    ///
    /// The deadline must not turn ordinary shutdown into a partial one: with nothing
    /// blocking, every thread finishes and is joined.
    #[test]
    fn shutdown_completes_when_every_task_returns() {
        let pool = pool_with(2, 4);
        for _ in 0..8 {
            let _ = pool.submit(
                WorkerTask::new("quick", 0, || TaskOutcome::Completed { bytes: 0 }),
                BackpressurePolicy::DropOldest,
            );
        }
        let report = pool.shutdown();
        assert!(
            report.is_complete(),
            "nothing blocked, so every worker should have been joined: {report:?}"
        );
        assert_eq!(report.stalled, 0);
        assert_eq!(report.joined + report.panicked, report.threads);
    }
}
