//! A priority-aware cooperative scheduler with ageing, so that interactive work cannot
//! be starved.
//!
//! The runtime has work of wildly different urgency — a keystroke, a frame's draw
//! calls, a layout pass, an autocomplete index rebuild — arriving into one thread that
//! must not stall. This crate is the mechanism that decides what runs, in what order,
//! and for how long, and it makes two promises that the rest of the runtime is allowed
//! to rely on.
//!
//! # Invariant 1 — interactive work cannot be permanently starved
//!
//! Background and maintenance work must never delay input, visible UI, GPU rendering,
//! animation, or interactive application work — not even under a permanently saturated
//! backlog of lower-priority work, and not for any wait time.
//!
//! This holds by construction, not by tuning. Each static [`Priority`] class occupies a
//! fixed band of [`LEVEL_STRIDE`] score units, and a task's ageing bonus is capped
//! strictly below the band of the class above it (see [`crate::priority::PriorityClass`]). The result is an
//! order that no amount of waiting can overturn:
//!
//! ```text
//! score(Background,  ∞) = 2047  <  2048 = score(Interactive, 0)
//! ```
//!
//! A backlog of background work can therefore delay itself indefinitely without ever
//! displacing interactive work, and there is no configuration of any parameter that
//! makes it so. The ageing bonus instead guarantees the opposite guarantee that
//! ageing is *for*: a task waiting within the unprotected band cannot be starved by
//! younger work in that same band, which a first-in, first-out rule alone would not
//! give it.
//!
//! # Invariant 2 — a cancelled task never runs after `cancel()` returns
//!
//! Cancellation is a shared [`AtomicBool`](std::sync::atomic::AtomicBool) behind an
//! [`std::sync::Arc`], so every holder of a [`Task`] — the scheduler, the component that submitted
//! it, a cancellation request arriving over IPC — reads the same flag. The flag is
//! checked on every selection, and cancelled entries are purged before a task is
//! chosen, so a cancelled task is never returned by [`Scheduler::next_task`] nor
//! dispatched by [`Scheduler::run_budgeted`], however high its priority and wherever it
//! sits in the queue.
//!
//! `Scheduler::cancel` returning `Ok` is the acknowledgement. Returning `Err` means the
//! scheduler never had the task — it was already dispatched — and nothing was claimed.
//!
//! # Determinism
//!
//! Nothing in this crate reads the system clock. Time enters every operation as an
//! explicit `now: Duration`, measured on the scheduler's own timeline by whoever owns
//! the frame loop. Ageing, deadlines, frame budgets and the property tests are therefore
//! reproducible from their inputs alone, and a schedule can be replayed exactly from a
//! recorded trace.
//!
//! # Example
//!
//! ```
//! use core::time::Duration;
//! use ncui_scheduler::{Priority, Scheduler, Task, Work};
//!
//! let mut scheduler = Scheduler::new(64)?;
//! let now = Duration::from_millis(0);
//!
//! scheduler.schedule(Task::new(Priority::Background, now, Work::timed("index", Duration::from_millis(4))))?;
//! scheduler.schedule(Task::new(Priority::Input, now, Work::timed("key", Duration::from_millis(1))))?;
//!
//! // One frame of 8 ms dispatches the keystroke and the index, in that order.
//! let report = scheduler.run_budgeted(now, Duration::from_millis(8));
//! assert_eq!(report.ran_count(), 2);
//! # Ok::<(), ncui_core::NcError>(())
//! ```

// Test code deliberately exercises panicking paths: a failed assertion or an
// `unwrap` in a test must abort so the failure is visible.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

mod priority;
mod queue;
mod scheduler;
mod task;

pub use priority::{
    AGEING_SATURATION, DEFAULT_AGEING_HORIZON, LEVEL_STRIDE, PROTECTED_FLOOR_RANK,
    PROTECTED_FLOOR_SCORE, Priority, PriorityClass,
};
pub use queue::{PopResult, READY_QUEUE_CHANNEL, ReadyQueue};
pub use scheduler::{ScheduleReport, Scheduler};
pub use task::{Task, Work};

/// Deterministic test helpers, shared by the module test suites.
#[cfg(test)]
mod testing {
    /// A `xorshift64*` generator with a fixed, explicit seed.
    ///
    /// Property tests here must be reproducible from their seed alone: a scheduler bug
    /// that only shows up on one run in ten thousand is not a bug anyone can fix, and a
    /// test that draws from the system entropy source cannot be replayed. `xorshift64*`
    /// is a whole generator in six lines, has no hidden state beyond the `u64` it is
    /// handed, and is well known to have a long period and no short cycles — everything
    /// a scheduler property test needs and nothing it does not.
    #[derive(Debug, Clone)]
    pub struct Rng(u64);

    impl Rng {
        /// Creates a generator from an explicit seed. A zero seed is replaced, because
        /// `xorshift` is stuck at zero forever.
        #[must_use]
        pub const fn new(seed: u64) -> Self {
            Self(if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            })
        }

        /// The next 64-bit value.
        pub const fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// A value in `0..bound`.
        ///
        /// The modulo bias is negligible for the small bounds used here and is not
        /// worth a rejection loop in test code.
        pub const fn below(&mut self, bound: u64) -> u64 {
            if bound == 0 {
                return 0;
            }
            self.next_u64() % bound
        }

        /// A value in `low..=high`.
        pub fn in_range(&mut self, low: u64, high: u64) -> u64 {
            debug_assert!(low <= high, "empty range");
            low + self.below(high - low + 1)
        }

        /// Picks an element uniformly.
        pub fn choose<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
            if items.is_empty() {
                return None;
            }
            items.get(usize::try_from(self.below(items.len() as u64)).ok()?)
        }
    }
}
