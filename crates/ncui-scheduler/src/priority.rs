//! Static task priority, and the dynamic priority class that ages with wait time.
//!
//! A [`Priority`] is the fixed class a task is submitted with: it never changes, and it
//! is the only thing that decides which of two tasks of different classes runs first.
//! A [`PriorityClass`] is the *effective* priority at one instant: the static class plus
//! an ageing bonus that grows with how long the task has been ready but unrun.
//!
//! # Why the score is an integer, not a float
//!
//! The effective priority is a `u32` score, `rank * LEVEL_STRIDE + bonus`. Two
//! properties fall out of the integer encoding for free, and both are load-bearing:
//!
//! 1. The bonus is strictly less than one level (`LEVEL_STRIDE`), so a task can never
//!    reach — let alone pass — the base score of the next class up. Static priority can
//!    be diluted by wait time, but it can never be *overturned* by it, at any wait time.
//! 2. The bonus ceiling is expressed in the same units, so the whole model is
//!    checkable by ordinary integer comparisons, with no floating-point tolerance
//!    questions at the boundary where a starved task finally overtakes a fresh one.
//!
//! # The ageing function
//!
//! With `w` = time spent ready, `H` = the ageing horizon, `c` = the bonus ceiling and
//! `R` = the saturation ratio (see [`MAX_AGE_BOOST`]):
//!
//! ```text
//! score(p, w) = base_score(p) + min(ceil(R * min(w, H) / H), c(p))
//! ```
//!
//! * **Monotonic.** `min(w, H)` is non-decreasing in `w` and the ratio is non-negative,
//!   so `score` is non-decreasing in wait time. A task can only ever gain rank by
//!   waiting; it can never lose rank to a task that arrived later at the same class.
//! * **Bounded.** `min(w, H)` caps growth at the horizon, and `c(p)` caps it again
//!   below the next class. The bonus is constant for all `w >= H`, so an ancient task
//!   and a task that has waited exactly the horizon are indistinguishable — waiting
//!   forever buys nothing extra, which is what stops an old background task from
//!   eventually displacing interactive work.
//! * **Clock-injected.** Every function takes `now` explicitly and never reads
//!   [`SystemTime`](std::time::SystemTime) or an [`Instant`](std::time::Instant).
//!   Ageing, deadlines and frame budgets are therefore fully deterministic in tests and
//!   reproducible in a recorded diagnostic trace.
//!
//! # Why the ceiling is rank-dependent
//!
//! A single global ceiling cannot be right. The lowest class,
//! [`Priority::Maintenance`], has *two* levels below the protected band, so it needs a
//! much larger ceiling than [`Priority::Background`] to climb at all; every protected class
//! needs only `LEVEL_STRIDE - 1`. The rule below produces all of them, and it is the entire
//! statement of the anti-starvation guarantee at the class level:
//!
//! * no protected class ever overtakes the class directly above it,
//! * neither unprotected class ([`Priority::Maintenance`], [`Priority::Background`]) can
//!   ever reach [`Priority::Interactive`], the lowest protected class, no matter how long it
//!   waits, and
//! * [`Priority::Maintenance`] at full ageing strictly outranks [`Priority::Background`] at
//!   full ageing — it is lifted *past* background work, not level with it. "Level with" was
//!   not good enough: an equal score is a tie, the tie is broken FIFO, and a maintenance
//!   task behind a permanent background backlog then starved — which is the one outcome
//!   ageing exists to prevent.

use core::cmp::Ordering;
use core::time::Duration;
use serde::{Deserialize, Serialize};

/// Score units reserved for one static priority level.
///
/// The value is deliberately not a power of two smaller than the type: the only
/// properties the model relies on are `bonus < LEVEL_STRIDE` and exact integer
/// arithmetic, both of which any positive stride satisfies. A round number is used so
/// that a score in a bug report can be read by hand.
pub const LEVEL_STRIDE: u32 = 1024;

/// The largest ageing bonus, as a fraction of a class's own bonus ceiling.
///
/// The ceiling is per-class (see [`PriorityClass::boost_ceiling`]); this constant only
/// fixes the *shape* of the curve. At `1.0` the bonus reaches the full ceiling exactly
/// at the horizon. It is kept as a named ratio rather than hard-coded into the formula
/// so the shape can be retuned — to a concave curve, say — without changing the bound
/// that the anti-starvation proof depends on.
pub const AGEING_SATURATION: u32 = 1024;

/// The default wait after which a task's ageing bonus stops growing.
///
/// 250 ms is roughly fifteen display frames at 60 Hz. Long enough that a burst of fresh
/// work at the same class is preferred over an old one (so ageing only kicks in when
/// there genuinely is nothing fresher), and short enough that a maintenance task
/// submitted during an idle period runs within a quarter second instead of waiting
/// behind whatever background work happens to be queued.
pub const DEFAULT_AGEING_HORIZON: Duration = Duration::from_millis(250);

/// The rank of [`Priority::Interactive`], the lowest class that must never be starved.
///
/// Everything at or above this rank is *protected*: input, visible UI, GPU rendering,
/// animation and interactive application work. The scheduler guarantees that no
/// unprotected work is ever dispatched ahead of protected work, at any wait time.
pub const PROTECTED_FLOOR_RANK: u8 = Priority::Interactive as u8;

/// The base score of [`Priority::Interactive`], the ceiling for all unprotected ageing.
///
/// Equal to `PROTECTED_FLOOR_RANK * LEVEL_STRIDE`. Exposed because it is the number the
/// anti-starvation proof is written against.
pub const PROTECTED_FLOOR_SCORE: u32 = PROTECTED_FLOOR_RANK as u32 * LEVEL_STRIDE;

/// The static priority class of a task.
///
/// The derived [`Ord`] follows *declaration* order, which is ascending: the classes are
/// declared lowest-first, so `Priority::Input` is the greatest value. This is the
/// reverse of the order they are listed in the specification, which lists them
/// highest-first for readability; the reverse is what makes `max()` select the more
/// urgent task.
///
/// The textual forms are part of the agent-facing contract: they appear in `ncui` JSON
/// output and in audit records, so renaming one is a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Housekeeping with no user-visible deadline: cache eviction, retiring dead
    /// handles, flushing idle-period bookkeeping. Runs only when nothing wants the CPU.
    Maintenance,
    /// Bulk work with no user waiting on it: indexing, prewarming, cache warming,
    /// speculative prefetch. Must never delay a protected class.
    Background,
    /// Application logic initiated by, and blocking, a user or an agent: event
    /// handlers, command execution, model steps. Protected.
    Interactive,
    /// Per-frame interpolation and other time-based visual continuity work. Protected:
    /// a dropped animation is a visible artefact, not a latency trade.
    Animation,
    /// GPU submission and readback for the current frame. Protected: missing a frame's
    /// draw calls is a dropped frame.
    GpuRendering,
    /// Layout, paint-record building and hit testing for what the user can see.
    /// Protected: the user is looking directly at the result of this work.
    VisibleUi,
    /// Input decoding, routing and the platform's own event delivery. The most urgent
    /// class there is, because every other class is downstream of it.
    Input,
}

impl Priority {
    /// Every class, lowest first. Useful for exhaustive tests and for metric labels.
    pub const ALL: [Self; 7] = [
        Self::Maintenance,
        Self::Background,
        Self::Interactive,
        Self::Animation,
        Self::GpuRendering,
        Self::VisibleUi,
        Self::Input,
    ];

    /// The dense rank of this class, ascending with urgency.
    ///
    /// The rank is the declaration ordinal, so it agrees with the derived
    /// [`Ord`] and with [`Priority::base_score`]. It is exposed so that metrics can
    /// report a compact priority number without depending on the enum's name.
    #[must_use]
    pub const fn rank(self) -> u8 {
        self as u8
    }

    /// The stable, machine-readable name used on the wire and in audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Maintenance => "maintenance",
            Self::Background => "background",
            Self::Interactive => "interactive",
            Self::Animation => "animation",
            Self::GpuRendering => "gpu_rendering",
            Self::VisibleUi => "visible_ui",
            Self::Input => "input",
        }
    }

    /// Whether this class is protected from being starved by lower-priority work.
    ///
    /// Everything at or above [`Priority::Interactive`] is protected. The two
    /// unprotected classes are the only ones an ageing task can overtake.
    #[must_use]
    pub const fn is_protected(self) -> bool {
        self.rank() >= PROTECTED_FLOOR_RANK
    }

    /// The score of this class with no ageing bonus applied.
    ///
    /// Two classes' base scores differ by exactly [`LEVEL_STRIDE`], which is what makes
    /// "a bonus is smaller than one level" equivalent to "a bonus can never overtake
    /// the next class".
    #[must_use]
    pub const fn base_score(self) -> u32 {
        self.rank() as u32 * LEVEL_STRIDE
    }
}

/// The effective priority of one task at one instant.
///
/// A priority class pairs a static [`Priority`] with the instant the task became ready
/// and the horizon over which its ageing bonus grows. It is `Copy` and cheap to
/// evaluate, so a scheduler can re-rank every queued task on every pop; the whole point
/// of the design is that ageing is applied *at comparison time*, not frozen when the
/// task was enqueued.
///
/// The type deliberately does not implement [`Ord`]. Its order depends on the instant
/// asked about, so a value-ordered comparison would be meaningless; use
/// [`PriorityClass::score`] with an explicit `now`, or [`PriorityClass::cmp_at`], which
/// make the instant part of the comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PriorityClass {
    base: Priority,
    ready_at: Duration,
    horizon: Duration,
}

impl PriorityClass {
    /// A class for work that became ready at `ready_at`, using
    /// [`DEFAULT_AGEING_HORIZON`].
    ///
    /// `ready_at` is *not* necessarily the instant the task was submitted: work that was
    /// prepared ahead of time, lifted from a finished continuation, or re-queued after a
    /// partial yield has genuinely been ready since `ready_at`, and the scheduler must
    /// not pretend otherwise. That distinction is the whole reason ageing is not
    /// redundant with the first-in, first-out tie-break.
    #[must_use]
    pub const fn new(base: Priority, ready_at: Duration) -> Self {
        Self {
            base,
            ready_at,
            horizon: DEFAULT_AGEING_HORIZON,
        }
    }

    /// A class with an explicit ageing horizon.
    ///
    /// A zero horizon is *not* an error: it degenerates to "no ageing", leaving the
    /// static priority and the submission order to decide. That is a meaningful policy
    /// for a caller that wants strictly static dispatch, so it is allowed rather than
    /// rejected.
    #[must_use]
    pub const fn with_horizon(base: Priority, ready_at: Duration, horizon: Duration) -> Self {
        Self {
            base,
            ready_at,
            horizon,
        }
    }

    /// The static class, ignoring any ageing.
    #[must_use]
    pub const fn base(&self) -> Priority {
        self.base
    }

    /// The instant the work became ready.
    #[must_use]
    pub const fn ready_at(&self) -> Duration {
        self.ready_at
    }

    /// The wait after which the ageing bonus stops growing.
    #[must_use]
    pub const fn horizon(&self) -> Duration {
        self.horizon
    }

    /// How long the work has been ready at `now`.
    ///
    /// Saturating: a `now` earlier than `ready_at` — a clock that went backwards, or a
    /// caller that mixed up frame timestamps — reads as zero wait rather than wrapping
    /// into a huge one. A wrapped value would hand the task a full ageing bonus it did
    /// not earn, which is exactly the failure the cap cannot catch.
    #[must_use]
    pub const fn age(&self, now: Duration) -> Duration {
        now.saturating_sub(self.ready_at)
    }

    /// The largest ageing bonus this class may ever receive, in score units.
    ///
    /// This is the anti-starvation constant:
    ///
    /// * for a protected class it is `LEVEL_STRIDE - 1`, so a task of this class can
    ///   approach but never reach the next class up;
    /// * for an unprotected class it is `PROTECTED_FLOOR_SCORE - base_score - 1 - rank`,
    ///   which keeps it strictly below [`Priority::Interactive`] *and* one unit below the
    ///   best score the class one rank below it can reach.
    ///
    /// The `- rank` term fixes a starvation the original formula admitted. It gave both
    /// unprotected classes the same ceiling *relative to the floor*, so at full ageing
    /// [`Priority::Maintenance`] and [`Priority::Background`] both scored exactly 2047 —
    /// level with each other, which is what the old documentation described as "lifts
    /// Maintenance level with Background". A tie is broken by FIFO, so a maintenance task
    /// behind a permanent background backlog waited for all of it, and the ageing that was
    /// supposed to prevent starvation did not: the only test paired aged Maintenance
    /// against a *fresh* Background task, which is the one case where the old formula
    /// promoted anything.
    ///
    /// Subtracting one unit per rank breaks the tie in the direction the module intends —
    /// Maintenance saturates at 2047 and Background at 2046 — so an aged maintenance task
    /// outranks aged background work, while both stay strictly below Interactive at 2048.
    #[must_use]
    pub const fn boost_ceiling(&self) -> u32 {
        let rank = self.base.rank() as u32;
        if rank >= PROTECTED_FLOOR_RANK as u32 {
            return LEVEL_STRIDE - 1;
        }
        PROTECTED_FLOOR_SCORE
            .saturating_sub(self.base.base_score())
            .saturating_sub(1 + rank)
    }

    /// The ageing bonus accrued after `self.age(now)`, in score units.
    ///
    /// Saturating linear ramp: `0` at zero wait, [`Self::boost_ceiling`] once the
    /// horizon is reached, and exactly constant thereafter. Evaluated in `u128` so that
    /// an absurdly long wait cannot overflow before the `min` clamps it.
    #[must_use]
    pub const fn bonus(&self, now: Duration) -> u32 {
        if self.horizon.is_zero() {
            return 0;
        }
        let ceiling = self.boost_ceiling();
        let waited_ms = self.age(now).as_millis();
        let horizon_ms = self.horizon.as_millis();
        if waited_ms == 0 || horizon_ms == 0 {
            return 0;
        }
        let capped_wait_ms = if waited_ms < horizon_ms {
            waited_ms
        } else {
            horizon_ms
        };
        let scaled = (ceiling as u128 * capped_wait_ms) / horizon_ms;
        // `scaled` is at most `ceiling` because `capped_wait_ms <= horizon_ms`, so the
        // cast to `u32` is lossless. The clamp keeps that true if the formula is ever
        // retuned into a shape that can overshoot.
        if scaled >= ceiling as u128 {
            ceiling
        } else {
            scaled as u32
        }
    }

    /// The effective score at `now`: what the scheduler actually orders by.
    #[must_use]
    pub const fn score(&self, now: Duration) -> u32 {
        self.base.base_score() + self.bonus(now)
    }

    /// Orders two classes as they would be ordered at the instant `now`.
    #[must_use]
    pub fn cmp_at(&self, now: Duration, other: &Self) -> Ordering {
        self.score(now).cmp(&other.score(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Rng;

    /// The precedence order of spec 17, highest first.
    const SPEC_ORDER: [Priority; 7] = [
        Priority::Input,
        Priority::VisibleUi,
        Priority::GpuRendering,
        Priority::Animation,
        Priority::Interactive,
        Priority::Background,
        Priority::Maintenance,
    ];

    #[test]
    fn spec_order_is_exactly_the_reverse_of_derived_ord() {
        let mut ascending = Priority::ALL;
        ascending.sort_unstable();
        let mut expected = SPEC_ORDER;
        expected.reverse();
        assert_eq!(ascending, expected);
    }

    #[test]
    fn higher_priority_is_always_greater() {
        for pair in SPEC_ORDER.windows(2) {
            let (higher, lower) = (pair[0], pair[1]);
            assert!(higher > lower, "{higher:?} must outrank {lower:?}");
        }
    }

    #[test]
    fn rank_and_base_score_track_declaration_order() {
        for (index, class) in Priority::ALL.iter().enumerate() {
            assert_eq!(usize::from(class.rank()), index, "{class:?} rank");
            assert_eq!(
                class.base_score(),
                (index as u32) * LEVEL_STRIDE,
                "{class:?} base score"
            );
        }
    }

    #[test]
    fn protected_classes_are_interactive_and_above() {
        for class in Priority::ALL {
            assert_eq!(
                class.is_protected(),
                class.rank() >= Priority::Interactive.rank(),
                "{class:?} protection"
            );
        }
    }

    #[test]
    fn names_are_stable_and_unique() {
        let mut names: Vec<&str> = Priority::ALL.iter().map(|c| c.as_str()).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "priority names must be unique");
        assert_eq!(Priority::GpuRendering.as_str(), "gpu_rendering");
        assert_eq!(Priority::VisibleUi.as_str(), "visible_ui");
    }

    #[test]
    fn priority_serializes_to_its_stable_name() {
        let json = serde_json::to_string(&Priority::VisibleUi).expect("serializes");
        assert_eq!(json, "\"visible_ui\"");
        let back: Priority = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, Priority::VisibleUi);
    }

    #[test]
    fn fresh_work_has_no_bonus() {
        let class = PriorityClass::new(Priority::Background, Duration::from_millis(100));
        assert_eq!(class.bonus(Duration::from_millis(100)), 0);
    }

    #[test]
    fn bonus_is_monotonic_in_wait_time() {
        let class = PriorityClass::new(Priority::Background, Duration::ZERO);
        let mut previous = 0;
        for step in 0..=400 {
            let now = Duration::from_millis(step);
            let bonus = class.bonus(now);
            assert!(
                bonus >= previous,
                "bonus fell at {step}ms: {previous} -> {bonus}"
            );
            previous = bonus;
        }
    }

    #[test]
    fn bonus_saturates_at_the_horizon_and_never_grows_again() {
        let class = PriorityClass::new(Priority::Background, Duration::ZERO);
        let ceiling = class.boost_ceiling();
        assert_eq!(class.bonus(DEFAULT_AGEING_HORIZON), ceiling);
        for step in [250_u64, 251, 1_000, 60_000, 86_400, u64::MAX] {
            assert_eq!(
                class.bonus(Duration::from_millis(step)),
                ceiling,
                "bonus grew past the ceiling at {step}ms"
            );
        }
    }

    #[test]
    fn half_the_horizon_is_about_half_the_ceiling() {
        let class = PriorityClass::new(Priority::Background, Duration::ZERO);
        let ceiling = class.boost_ceiling();
        let half = class.bonus(DEFAULT_AGEING_HORIZON / 2);
        let slack = ceiling / 10 + 1;
        assert!(
            half.abs_diff(ceiling / 2) <= slack,
            "half-horizon bonus {half} is not half of ceiling {ceiling}"
        );
    }

    /// The core anti-starvation bound, in its strongest form: waiting forever never lifts a
    /// task into the band of the next *protected* class above it.
    ///
    /// Note the asymmetry for [`Priority::Maintenance`], which is deliberate: it has two
    /// levels of headroom and saturates *above* [`Priority::Background`] work, because a
    /// maintenance task that could never outrank any background task would be starved by a
    /// permanent background backlog — which is the very thing ageing exists to prevent.
    /// "Above" rather than "level with": an equal score is a tie, a tie is broken FIFO, and
    /// a maintenance task behind a permanent backlog would still wait for all of it. It
    /// still cannot reach [`Priority::Interactive`].
    #[test]
    fn no_class_ever_reaches_the_next_protected_class() {
        let mut rng = Rng::new(0x5EED_0001);
        for class in Priority::ALL {
            let Some(bound) = strict_upper_bound(class) else {
                // `Input` is the top of the ladder; there is nothing above it to reach.
                continue;
            };
            let instance = PriorityClass::new(class, Duration::ZERO);
            for _ in 0..64 {
                let now = Duration::from_millis(rng.below(1_000_000));
                assert!(
                    instance.score(now) < bound,
                    "{class:?} reached {bound} after {now:?}",
                );
            }
            // The bound must hold at the limit, not just on sampled waits.
            assert!(
                instance.score(Duration::MAX) < bound,
                "{class:?} at the limit"
            );
        }
    }

    /// The same bound, stated as a saturation value: every class except the top of the
    /// ladder saturates *strictly below* the first protected class above it.
    ///
    /// The margin is not uniform, and that is the point. [`Priority::Background`] saturates
    /// two units below the floor rather than one, so that [`Priority::Maintenance`] has room
    /// to saturate one unit above it. A uniform one-unit margin would make the two
    /// unprotected classes tie, and a tie is FIFO, so a maintenance task behind a permanent
    /// background backlog would starve. Every class is therefore held *below* its bound; only
    /// the protected ones get the full one-unit margin.
    #[test]
    fn every_class_saturates_strictly_below_its_bound() {
        for class in Priority::ALL {
            let Some(bound) = strict_upper_bound(class) else {
                continue;
            };
            let instance = PriorityClass::new(class, Duration::ZERO);
            let saturated = instance.score(Duration::MAX);
            assert!(
                saturated < bound,
                "{class:?} must saturate strictly below {bound}, got {saturated}"
            );
            // The classes that can be given the full margin do take it, so the margin is
            // never loosened further than it needs to be.
            if class.is_protected() {
                assert_eq!(
                    saturated,
                    bound - 1,
                    "a protected class should still take the full one-unit margin below {bound}"
                );
            }
        }
        // Maintenance keeps the full margin; Background gives up the unit it needs.
        let maintenance = PriorityClass::new(Priority::Maintenance, Duration::ZERO);
        let background = PriorityClass::new(Priority::Background, Duration::ZERO);
        assert_eq!(
            maintenance.score(Duration::MAX),
            PROTECTED_FLOOR_SCORE - 1,
            "maintenance should still take the full one-unit margin below the floor"
        );
        assert_eq!(
            background.score(Duration::MAX),
            PROTECTED_FLOOR_SCORE - 2,
            "background gives up the unit that lets maintenance outrank it"
        );
    }

    /// An aged maintenance task strictly outranks an aged background task.
    ///
    /// This is the invariant the tie defeated. At full ageing both used to score exactly
    /// 2047, so FIFO broke the tie and a maintenance task behind a permanent background
    /// backlog waited for all of it — the starvation ageing exists to prevent.
    #[test]
    fn an_aged_maintenance_task_outranks_an_aged_background_task() {
        let maintenance = PriorityClass::new(Priority::Maintenance, Duration::ZERO);
        let background = PriorityClass::new(Priority::Background, Duration::ZERO);
        // A wait far past both horizons, so both have fully saturated.
        let now = Duration::from_secs(3_600);
        assert!(
            maintenance.score(now) > background.score(now),
            "an aged maintenance task ({}) must outrank an aged background task ({})",
            maintenance.score(now),
            background.score(now)
        );
    }

    /// The other half of the same bound: an unprotected task cannot reach interactive
    /// work however long it waits, which is what makes a permanent background backlog
    /// harmless.
    #[test]
    fn unprotected_work_can_never_reach_interactive_work() {
        for class in [Priority::Maintenance, Priority::Background] {
            let instance = PriorityClass::new(class, Duration::ZERO);
            let ceiling = instance.boost_ceiling();
            let saturated = ceiling + class.base_score();
            assert!(
                saturated < PROTECTED_FLOOR_SCORE,
                "{class:?} saturates at {saturated}, which reaches the protected floor at \
                 {PROTECTED_FLOOR_SCORE}"
            );
            assert!(instance.score(Duration::MAX) < PROTECTED_FLOOR_SCORE);
        }
        let interactive = PriorityClass::new(Priority::Interactive, Duration::ZERO);
        assert_eq!(interactive.score(Duration::ZERO), PROTECTED_FLOOR_SCORE);
    }

    /// Ageing is not decoration: an old maintenance task outranks a *fresh* background
    /// task, which a pure first-in, first-out rule would never do.
    #[test]
    fn an_aged_maintenance_task_is_promoted_above_a_fresh_background_task() {
        let old_maintenance = PriorityClass::new(Priority::Maintenance, Duration::from_millis(0));
        let fresh_background = PriorityClass::new(Priority::Background, Duration::from_millis(500));
        let now = Duration::from_millis(500);
        assert!(
            old_maintenance.score(now) > fresh_background.score(now),
            "ageing failed to promote maintenance: {} vs {}",
            old_maintenance.score(now),
            fresh_background.score(now)
        );
    }

    /// The same promotion never reaches the protected band.
    #[test]
    fn the_promotion_never_reaches_interactive_work() {
        let old_maintenance = PriorityClass::new(Priority::Maintenance, Duration::ZERO);
        let fresh_interactive =
            PriorityClass::new(Priority::Interactive, Duration::from_millis(500));
        let now = Duration::from_millis(500);
        assert!(old_maintenance.score(now) < fresh_interactive.score(now));
    }

    #[test]
    fn a_clock_that_goes_backwards_yields_no_bonus() {
        let class = PriorityClass::new(Priority::Background, Duration::from_millis(1_000));
        let earlier = Duration::from_millis(10);
        assert_eq!(class.age(earlier), Duration::ZERO);
        assert_eq!(class.bonus(earlier), 0);
        assert_eq!(class.score(earlier), Priority::Background.base_score());
    }

    #[test]
    fn a_zero_horizon_disables_ageing_without_erroring() {
        let class =
            PriorityClass::with_horizon(Priority::Background, Duration::ZERO, Duration::ZERO);
        assert_eq!(class.bonus(Duration::from_hours(24)), 0);
        assert_eq!(
            class.score(Duration::from_hours(24)),
            Priority::Background.base_score()
        );
    }

    #[test]
    fn a_custom_horizon_only_changes_the_ramp_not_the_ceiling() {
        let slow = PriorityClass::with_horizon(
            Priority::Background,
            Duration::ZERO,
            Duration::from_secs(4),
        );
        let fast = PriorityClass::with_horizon(
            Priority::Background,
            Duration::ZERO,
            Duration::from_millis(1),
        );
        assert_eq!(slow.boost_ceiling(), fast.boost_ceiling());
        let probe = Duration::from_millis(100);
        assert!(slow.bonus(probe) < fast.bonus(probe));
        assert_eq!(slow.bonus(Duration::from_hours(24)), slow.boost_ceiling());
    }

    #[test]
    fn cmp_at_agrees_with_score_ordering() {
        let a = PriorityClass::new(Priority::Background, Duration::from_millis(0));
        let b = PriorityClass::new(Priority::Interactive, Duration::from_millis(0));
        let now = Duration::from_millis(10);
        assert_eq!(a.cmp_at(now, &b), Ordering::Less);
        assert_eq!(b.cmp_at(now, &a), Ordering::Greater);
        let same = PriorityClass::new(Priority::Interactive, Duration::from_millis(0));
        assert_eq!(b.cmp_at(now, &same), Ordering::Equal);
    }

    /// The base score of the lowest protected class *strictly above* `class` — the band
    /// that ageing must never lift `class` into. `None` for the top of the ladder.
    fn strict_upper_bound(class: Priority) -> Option<u32> {
        Priority::ALL
            .iter()
            .copied()
            .skip(usize::from(class.rank()) + 1)
            .find(|candidate| candidate.is_protected())
            .map(Priority::base_score)
    }
}
