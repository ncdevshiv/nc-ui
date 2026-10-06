//! The native worker runtime.
//!
//! Expensive work — filesystem indexing, search, parsing, compression, database work,
//! AI inference — must not run on the UI thread (spec 4.3). This crate provides the
//! workers that run it, with the properties that make them safe to leave running
//! unattended:
//!
//! * **Bounded queues.** Every worker has a bounded task queue with an explicit
//!   backpressure policy. No unbounded queue exists, because an unbounded queue converts a
//!   memory leak into an out-of-memory kill (spec 96).
//! * **Observable.** Queue depth, task duration percentiles, cancellations and failures are
//!   recorded in the shared `MetricsRegistry`, so a worker that is silently doing nothing
//!   is distinguishable from a healthy one.
//! * **Cancellable and explicit about failure.** A task can be cancelled before it runs; a
//!   worker that dies is reported rather than silently shrinking the pool.
//!
//! # Lifecycle
//!
//! Workers follow the state machine in spec 95. Invalid transitions are rejected rather
//! than tolerated; see [`WorkerState`].
//!
//! # What this crate does not do
//!
//! It does not schedule work against the UI frame budget. Dispatch is a *runtime*
//! responsibility so that the priority model in `ncui-scheduler` remains the single place
//! where priority order is defined (ADR-005).

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

pub mod pool;
pub mod task;

pub use pool::{BackpressurePolicy, ShutdownReport, WorkerPool, WorkerSnapshot, WorkerState};
pub use task::{TaskOutcome, WorkerTask};
