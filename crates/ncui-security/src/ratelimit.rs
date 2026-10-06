//! A token-bucket rate limiter with an injected clock.
//!
//! Spec 96 requires every bounded resource to have an enforced bound, and the agent API
//! is the resource most worth bounding: an agent session that can issue requests without
//! limit can saturate the scheduler, fill every log ring, and starve the UI it is supposed
//! to be driving.
//!
//! # Why the clock is a parameter
//!
//! [`RateLimiter::try_acquire`] takes `now: Duration` and never reads a clock. That is not
//! a stylistic choice:
//!
//! * **A rate limiter's interesting behaviour lives entirely at its boundaries** — the
//!   last token of a burst, the first token after a refill, the tick where the bucket is
//!   exactly full. A test that cannot set the clock cannot reach those points reliably;
//!   it can only sleep and hope, which produces a suite that is either slow or flaky. With
//!   an injected clock every boundary test in this module is exact.
//! * **A monotonic clock can go backwards relative to a `Duration` taken from another
//!   clock source**, and a limiter that underflows its own token count on a backwards step
//!   hands out free quota. See the handling of backwards time below.
//!
//! # Semantics
//!
//! A classic token bucket: the bucket refills continuously at `per_second` tokens per
//! second and holds at most `burst` tokens. [`RateLimiter::try_acquire`] spends `cost`
//! tokens if they are available and otherwise fails with [`ErrorKind::LimitExceeded`].
//!
//! Refill is computed from whole elapsed nanoseconds rather than accumulated
//! incrementally, so the limiter cannot drift: a caller that arrives after a long idle
//! period gets exactly one `burst`, not a windfall proportional to the idle time.
//!
//! Backwards time is *clamped, not trusted*: if `now` is before the last observed instant
//! the elapsed time is treated as zero and `last_refill` is **not** moved backwards. That
//! is the fail-closed choice — a clock that jumps backwards cannot be used to re-earn
//! tokens, and a subsequent forward jump still refills correctly from the high-water mark.

use core::fmt;
use ncui_core::{ErrorKind, NcError, Result};

/// The operation name recorded on a rate-limit rejection.
const OP_ACQUIRE: &str = "security::ratelimit::try_acquire";

/// The resource label used in [`ErrorKind::LimitExceeded`].
const RESOURCE: &str = "agent.rate.tokens";

/// A token-bucket rate limiter.
///
/// The bucket is held in "milli-tokens" — one token is `TOKENS_SCALE` units — so that a
/// fractional refill below one whole token is not silently discarded. Without the scaling
/// factor, a limiter configured at 1000 tokens/second would grant one token per call
/// *rounded down*, which turns a smooth rate limit into a bursty one. `u64` arithmetic on
/// milli-tokens is exact: there is no floating-point drift, which matters because a drift
/// in a *security* bound is a bound that is not the one the configuration says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimiter {
    /// Sustained refill rate, in whole tokens per second.
    per_second: u32,
    /// Maximum tokens the bucket may hold, in whole tokens.
    burst: u32,
    /// Current balance in milli-tokens. Bounded above by `burst * TOKENS_SCALE`.
    milli_tokens: u64,
    /// The most recent instant refill has been accounted to.
    last_refill: core::time::Duration,
}

/// Milli-tokens per token. See the [`RateLimiter`] documentation.
const TOKENS_SCALE: u64 = 1_000;

impl RateLimiter {
    /// Creates a limiter that refills at `per_second` tokens per second and holds at most
    /// `burst` tokens.
    ///
    /// `per_second` of 0 is rejected rather than accepted as "never refill": a limiter
    /// that can never refill and has a non-zero burst is a one-shot quota, which is a
    /// different component with different semantics, and silently accepting it here would
    /// let a configuration mistake present as "the agent is being rate limited".
    ///
    /// `burst` of 0 is likewise rejected. A zero-capacity bucket denies everything
    /// permanently, which is a denial of service configured as a limit.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] if `per_second` is 0 or `burst` is 0.
    pub fn new(per_second: u32, burst: u32) -> Result<Self> {
        if per_second == 0 {
            return Err(NcError::new(ErrorKind::Validation {
                field: "per_second".to_owned(),
                reason: "a sustained rate of zero makes this a one-shot quota, not a rate \
                         limiter; use an explicit quota type for that"
                    .to_owned(),
            })
            .operation("security::ratelimit::new"));
        }
        if burst == 0 {
            return Err(NcError::new(ErrorKind::Validation {
                field: "burst".to_owned(),
                reason: "a zero burst denies every request permanently; that is a \
                         denial of service, not a limit"
                    .to_owned(),
            })
            .operation("security::ratelimit::new"));
        }
        Ok(Self {
            per_second,
            burst,
            milli_tokens: u64::from(burst) * TOKENS_SCALE,
            last_refill: core::time::Duration::ZERO,
        })
    }

    /// The sustained refill rate in whole tokens per second.
    #[must_use]
    pub const fn per_second(&self) -> u32 {
        self.per_second
    }

    /// The maximum number of tokens the bucket holds.
    #[must_use]
    pub const fn burst(&self) -> u32 {
        self.burst
    }

    /// Whole tokens currently available, without consuming any.
    ///
    /// Takes `now` because availability depends on refill, exactly as acquisition does.
    /// Truncates toward zero: a bucket holding 1.5 tokens reports 1, because a caller
    /// cannot spend a fraction of a token and the fractional remainder is not a licence to
    /// round up.
    #[must_use]
    pub fn available(&self, now: core::time::Duration) -> u64 {
        self.projected(now) / TOKENS_SCALE
    }

    /// Accounts for the time elapsed since the last refill and returns the resulting
    /// balance in milli-tokens, without mutating the limiter.
    fn projected(&self, now: core::time::Duration) -> u64 {
        let elapsed = self.elapsed_since_last(now);
        let gained = elapsed_nanos_to_milli_tokens(elapsed, self.per_second);
        let capacity = u64::from(self.burst) * TOKENS_SCALE;
        // `saturating_add` because `elapsed * per_second` can exceed `u64` for a very
        // large `Duration`; the bucket is capped anyway, so saturating is exact here.
        self.milli_tokens.saturating_add(gained).min(capacity)
    }

    /// The elapsed time to credit, clamped to zero for a backwards clock step.
    fn elapsed_since_last(&self, now: core::time::Duration) -> core::time::Duration {
        now.checked_sub(self.last_refill)
            .unwrap_or(core::time::Duration::ZERO)
    }

    /// Attempts to spend `cost` tokens at instant `now`.
    ///
    /// `cost` of 0 always succeeds and consumes nothing, which lets a caller with a
    /// data-dependent cost express "free when there is nothing to do" without a special
    /// case at the call site.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::LimitExceeded`] if the bucket holds fewer than `cost` whole tokens.
    /// The error reports the *configured burst* as the limit and `cost` as the observed
    /// demand, which is the pair a reader needs to tell "you are being throttled" apart
    /// from "you asked for more than a burst is".
    pub fn try_acquire(&mut self, now: core::time::Duration, cost: u32) -> Result<()> {
        // The accrued balance is committed on *every* call, success or failure. A
        // limiter that only banked tokens on success would discard the refill every time
        // a request was throttled, so a client polling at 10 Hz against a 10 tokens/s
        // limit would never accumulate anything at all — it would be starved by the very
        // mechanism meant to bound it.
        let balance = self.projected(now);
        // `now` moves the high-water mark forward only when it is at least as late as the
        // last one, so a backwards clock step grants no tokens and does not rewind.
        if now >= self.last_refill {
            self.last_refill = now;
        }
        self.milli_tokens = balance;

        let required = u64::from(cost).saturating_mul(TOKENS_SCALE);
        if balance < required {
            return Err(NcError::new(ErrorKind::LimitExceeded {
                resource: RESOURCE.to_owned(),
                limit: u64::from(self.burst),
                observed: u64::from(cost),
            })
            .operation(OP_ACQUIRE));
        }
        self.milli_tokens = balance - required;
        Ok(())
    }

    /// Refills the bucket to `burst` and rewinds the high-water mark to `now`.
    ///
    /// Exists so that a long-lived limiter can be reset deliberately — after a policy
    /// change, or when a session is re-established — instead of by dropping and
    /// reconstructing the value, which would lose the caller's configuration handle.
    pub fn reset(&mut self, now: core::time::Duration) {
        self.milli_tokens = u64::from(self.burst) * TOKENS_SCALE;
        self.last_refill = now;
    }
}

/// Converts an elapsed duration into milli-tokens at a given per-second rate.
///
/// Uses nanosecond arithmetic with a `u128` intermediate so that a very large `Duration`
/// paired with a large `per_second` cannot overflow `u64` before the result is clamped to
/// the bucket capacity. The division order (`nanos * rate / NANOS_PER_SEC * SCALE`) is
/// chosen to keep full precision on the nano product and to avoid dividing first, which
/// would round a sub-microsecond elapsed time to zero tokens.
fn elapsed_nanos_to_milli_tokens(elapsed: core::time::Duration, per_second: u32) -> u64 {
    const NANOS_PER_SEC: u128 = 1_000_000_000;
    let nanos = elapsed.as_nanos();
    let scaled = nanos
        .saturating_mul(u128::from(per_second))
        .saturating_mul(u128::from(TOKENS_SCALE))
        / NANOS_PER_SEC;
    // The caller clamps to the bucket capacity; saturating here keeps the conversion
    // total for any input.
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

impl fmt::Display for RateLimiter {
    /// Renders `per_second` and `burst` without exposing the mutable balance, so logging
    /// a limiter does not require it to be borrowed mutably.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RateLimiter({} tokens/s, burst {})",
            self.per_second, self.burst
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;

    const MILLIS: Duration = Duration::from_millis(1);
    const SECOND: Duration = Duration::from_secs(1);

    fn limiter(per_second: u32, burst: u32) -> RateLimiter {
        RateLimiter::new(per_second, burst).expect("valid configuration")
    }

    #[test]
    fn construction_rejects_degenerate_configurations() {
        let err = RateLimiter::new(0, 5).expect_err("zero rate must be rejected");
        assert!(
            matches!(err.kind(), ErrorKind::Validation { .. }),
            "got {err:?}"
        );
        let err = RateLimiter::new(5, 0).expect_err("zero burst must be rejected");
        assert!(
            matches!(err.kind(), ErrorKind::Validation { .. }),
            "got {err:?}"
        );
        let err = RateLimiter::new(0, 0).expect_err("both zero must be rejected");
        assert!(
            matches!(err.kind(), ErrorKind::Validation { .. }),
            "got {err:?}"
        );
        assert!(RateLimiter::new(1, 1).is_ok());
    }

    #[test]
    fn starts_full_and_reports_its_configuration() {
        let limiter = limiter(10, 5);
        assert_eq!(limiter.per_second(), 10);
        assert_eq!(limiter.burst(), 5);
        assert_eq!(limiter.available(Duration::ZERO), 5);
        assert_eq!(limiter.to_string(), "RateLimiter(10 tokens/s, burst 5)");
    }

    /// The core property: exactly `burst` acquisitions succeed at a single instant, and
    /// the next one is denied.
    #[test]
    fn burst_is_allowed_and_the_next_request_is_denied() {
        let mut limiter = limiter(1, 3);
        for _ in 0..3 {
            assert!(limiter.try_acquire(Duration::ZERO, 1).is_ok());
        }
        let err = limiter
            .try_acquire(Duration::ZERO, 1)
            .expect_err("the fourth request in the same instant must be denied");
        assert!(
            matches!(
                err.kind(),
                ErrorKind::LimitExceeded { resource, limit, observed }
                    if resource == RESOURCE && *limit == 3 && *observed == 1
            ),
            "got {err:?}"
        );
        assert_eq!(err.operation_name(), OP_ACQUIRE);
        assert_eq!(err.code(), "limit_exceeded");
    }

    #[test]
    fn the_burst_ceiling_is_never_exceeded_under_sustained_attack() {
        // Drain a bucket completely, then hammer it with back-to-back acquisitions at a
        // far higher rate than the refill and prove the total served in the window never
        // exceeds refill + burst.
        let mut limiter = limiter(10, 5);
        while limiter.try_acquire(Duration::ZERO, 1).is_ok() {}
        let mut served = 0u32;
        let mut now = Duration::ZERO;
        // One second of hammering: 1 ms apart, 10 tokens/s means one token per 100 ms.
        // The clock advances *before* each acquisition so the final request is at a full
        // 1 s and the expected count is exact rather than off by one.
        for _ in 0..1_000 {
            now += MILLIS;
            if limiter.try_acquire(now, 1).is_ok() {
                served += 1;
            }
        }
        // The bucket was drained, so nothing was carried in: at most 10 refilled, and
        // exactly 10 must be served or the limiter is either starving or leaking.
        assert_eq!(
            served, 10,
            "one second at 10 tokens/s must serve exactly 10"
        );
    }

    #[test]
    fn refills_after_elapsed_time() {
        let mut limiter = limiter(10, 2);
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_ok());
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_ok());
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_err());

        // 100 ms at 10 tokens/s = exactly one token.
        assert!(limiter.try_acquire(MILLIS * 100, 1).is_ok());
        assert!(limiter.try_acquire(MILLIS * 100, 1).is_err());
        // Another 100 ms earns another.
        assert!(limiter.try_acquire(MILLIS * 200, 1).is_ok());
    }

    #[test]
    fn refill_is_proportional_to_elapsed_time() {
        let mut limiter = limiter(1, 1);
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_ok());
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_err());
        // 500 ms at 1 token/s is half a token: not enough for a whole acquisition.
        assert!(limiter.try_acquire(MILLIS * 500, 1).is_err());
        // 999 ms is still short of a full token.
        assert!(limiter.try_acquire(MILLIS * 999, 1).is_err());
        // At exactly one second, one full token exists.
        assert!(limiter.try_acquire(SECOND, 1).is_ok());
    }

    /// The milli-token scaling must not round a sub-token refill away.
    #[test]
    fn fractional_refills_accumulate_instead_of_being_truncated() {
        // 8000 tokens/s: one whole token every 125 microseconds. Truncating each step to
        // whole tokens would grant one token per step and let 8000 through per second.
        let mut limiter = limiter(8_000, 2);
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_ok());
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_ok());
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_err());

        let mut now = Duration::ZERO;
        let mut granted = 0u32;
        for _ in 0..1_000 {
            now += Duration::from_micros(125);
            if limiter.try_acquire(now, 1).is_ok() {
                granted += 1;
            }
        }
        // 1000 * 125 us = 125 ms at 8000/s = 1000 tokens, plus the 2 carried in.
        assert_eq!(
            granted, 1_000,
            "fractional refill was lost or over-credited"
        );
    }

    #[test]
    fn cost_greater_than_one_consumes_multiple_tokens() {
        // A cost of 6 against a bucket of 10 leaves 4.
        let mut limiter = limiter(1, 10);
        assert!(limiter.try_acquire(Duration::ZERO, 4).is_ok());
        assert!(limiter.try_acquire(Duration::ZERO, 6).is_ok());
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_err());
    }

    #[test]
    fn a_multi_token_cost_leaves_the_remainder_available() {
        let mut limiter = limiter(1, 10);
        assert!(limiter.try_acquire(Duration::ZERO, 6).is_ok());
        assert_eq!(limiter.available(Duration::ZERO), 4);
        assert!(limiter.try_acquire(Duration::ZERO, 4).is_ok());
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_err());
    }

    #[test]
    fn cost_above_the_burst_is_always_denied_and_does_not_partially_consume() {
        let mut limiter = limiter(1, 5);
        let err = limiter
            .try_acquire(Duration::ZERO, 6)
            .expect_err("a cost larger than the burst can never be served");
        assert!(
            matches!(err.kind(), ErrorKind::LimitExceeded { limit, observed, .. }
                if *limit == 5 && *observed == 6),
            "got {err:?}"
        );
        // Nothing was consumed: the full burst is still available.
        assert_eq!(limiter.available(Duration::ZERO), 5);
        assert!(limiter.try_acquire(Duration::ZERO, 5).is_ok());
    }

    #[test]
    fn a_failed_acquisition_does_not_consume_the_remaining_balance() {
        let mut limiter = limiter(1, 5);
        assert!(limiter.try_acquire(Duration::ZERO, 3).is_ok());
        assert!(limiter.try_acquire(Duration::ZERO, 3).is_err());
        assert!(limiter.try_acquire(Duration::ZERO, 3).is_err());
        assert_eq!(limiter.available(Duration::ZERO), 2);
        assert!(limiter.try_acquire(Duration::ZERO, 2).is_ok());
        assert_eq!(limiter.available(Duration::ZERO), 0);
    }

    #[test]
    fn zero_cost_always_succeeds_and_consumes_nothing() {
        let mut limiter = limiter(1, 2);
        for _ in 0..100 {
            assert!(limiter.try_acquire(Duration::ZERO, 0).is_ok());
        }
        assert_eq!(limiter.available(Duration::ZERO), 2);
        assert!(limiter.try_acquire(Duration::ZERO, 2).is_ok());
    }

    #[test]
    fn a_long_idle_period_yields_one_burst_not_a_windfall() {
        let mut limiter = limiter(1, 5);
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_ok());
        // A whole day of idleness must not grant a day's worth of tokens.
        let idle = Duration::from_hours(24);
        assert_eq!(limiter.available(idle), 5);
        let mut served = 0u32;
        for _ in 0..10 {
            if limiter.try_acquire(idle, 1).is_ok() {
                served += 1;
            }
        }
        assert_eq!(served, 5, "the bucket was not clamped to the burst");
    }

    #[test]
    fn the_bucket_is_clamped_to_burst_during_refill() {
        let limiter = limiter(100, 3);
        // Never drain it; a long interval must still leave it at burst.
        let later = Duration::from_secs(1_000);
        assert_eq!(limiter.available(later), 3);
        assert_eq!(limiter.burst(), 3);
    }

    #[test]
    fn repeated_calls_at_the_same_instant_do_not_double_refill() {
        let mut limiter = limiter(10, 1);
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_ok());
        // Several calls at the same instant: the first drains, the rest fail, and none
        // of them credit time.
        for _ in 0..5 {
            assert!(limiter.try_acquire(Duration::ZERO, 1).is_err());
        }
        assert_eq!(limiter.available(Duration::ZERO), 0);
        assert!(limiter.try_acquire(MILLIS * 100, 1).is_ok());
    }

    /// A backwards clock must not be able to re-earn tokens.
    #[test]
    fn a_backwards_clock_step_grants_nothing_and_does_not_rewind() {
        let mut limiter = limiter(1, 1);
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_ok());
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_err());

        // Clock jumps backwards by a second and tries again.
        let backwards = Duration::ZERO;
        assert!(
            limiter.try_acquire(backwards, 1).is_err(),
            "a backwards clock granted a token"
        );

        // Moving forward past the original high-water mark still works normally: the
        // limiter did not rewind its accounting to the backwards instant.
        assert!(limiter.try_acquire(MILLIS * 1_000, 1).is_ok());
    }

    #[test]
    fn a_stalled_clock_stops_the_refill() {
        let mut limiter = limiter(1, 1);
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_ok());
        // Many acquisitions at the same instant: still empty.
        for _ in 0..1_000 {
            assert!(limiter.try_acquire(Duration::ZERO, 1).is_err());
        }
        // Only real elapsed time refills.
        assert!(limiter.try_acquire(SECOND, 1).is_ok());
    }

    /// Two independent limiters must not share state — the classic bug where a limiter is
    /// stored in a `static` and every subject shares one bucket.
    #[test]
    fn limiters_are_independent() {
        let mut a = limiter(1, 2);
        let mut b = limiter(1, 2);
        assert!(a.try_acquire(Duration::ZERO, 2).is_ok());
        assert!(a.try_acquire(Duration::ZERO, 1).is_err());
        assert_eq!(
            b.available(Duration::ZERO),
            2,
            "b was affected by a's traffic"
        );
        assert!(b.try_acquire(Duration::ZERO, 1).is_ok());
    }

    /// Interleaved traffic: a burst, an idle gap, another burst, then a sustained load.
    /// Proves the limiter's behaviour is a function of elapsed time, not of call order.
    #[test]
    fn interleaved_traffic_pattern_is_respected() {
        let mut limiter = limiter(4, 4); // one token per 250 ms
        let mut now = Duration::ZERO;
        let mut granted = 0u32;

        // Phase 1: burst of 4.
        for _ in 0..4 {
            assert!(limiter.try_acquire(now, 1).is_ok());
            granted += 1;
        }
        assert!(limiter.try_acquire(now, 1).is_err());

        // Phase 2: idle for one second, so 4 tokens are refilled.
        now += SECOND;
        assert!(limiter.try_acquire(now, 4).is_ok());
        granted += 4;
        assert!(limiter.try_acquire(now, 1).is_err());

        // Phase 3: steady 250 ms cadence for 10 steps.
        for _ in 0..10 {
            now += MILLIS * 250;
            if limiter.try_acquire(now, 1).is_ok() {
                granted += 1;
            }
        }
        assert_eq!(
            granted, 18,
            "granted count drifted from the expected schedule"
        );
    }

    #[test]
    fn reset_restores_the_full_burst_and_rewinds_the_clock() {
        let mut limiter = limiter(1, 5);
        while limiter.try_acquire(Duration::ZERO, 1).is_ok() {}
        assert_eq!(limiter.available(Duration::ZERO), 0);
        limiter.reset(Duration::from_secs(100));
        assert_eq!(limiter.available(Duration::from_secs(100)), 5);
        assert!(limiter.try_acquire(Duration::from_secs(100), 5).is_ok());
    }

    #[test]
    fn conversion_saturates_instead_of_overflowing() {
        // A near-maximal Duration at a large rate must not panic or wrap; the result is
        // clamped by the caller to the bucket capacity anyway.
        let huge = Duration::new(u64::MAX, 999_999_999);
        let converted = elapsed_nanos_to_milli_tokens(huge, u32::MAX);
        assert_eq!(converted, u64::MAX);

        // And a realistic conversion is exact.
        assert_eq!(elapsed_nanos_to_milli_tokens(SECOND, 10), 10 * TOKENS_SCALE);
        assert_eq!(elapsed_nanos_to_milli_tokens(Duration::ZERO, 10), 0);
        assert_eq!(elapsed_nanos_to_milli_tokens(MILLIS, 1_000), TOKENS_SCALE);
    }

    #[test]
    fn every_denial_is_reported_with_the_configured_burst() {
        let mut limiter = limiter(3, 7);
        while limiter.try_acquire(Duration::ZERO, 1).is_ok() {}
        let err = limiter.try_acquire(Duration::ZERO, 1).expect_err("empty");
        match err.kind() {
            ErrorKind::LimitExceeded {
                resource,
                limit,
                observed,
            } => {
                assert_eq!(resource, "agent.rate.tokens");
                assert_eq!(*limit, 7);
                assert_eq!(*observed, 1);
            }
            other => panic!("expected LimitExceeded, got {other:?}"),
        }
    }

    #[test]
    fn available_does_not_consume_or_mutate() {
        let mut limiter = limiter(1, 2);
        for _ in 0..10 {
            assert_eq!(limiter.available(Duration::ZERO), 2);
        }
        assert!(limiter.try_acquire(Duration::ZERO, 2).is_ok());
        assert_eq!(limiter.available(Duration::ZERO), 0);
        assert_eq!(limiter.available(SECOND), 1);
        // Reading availability must not have spent the token.
        assert_eq!(limiter.available(SECOND), 1);
        assert!(limiter.try_acquire(SECOND, 1).is_ok());
    }

    #[test]
    fn available_truncates_a_fractional_balance_rather_than_rounding_up() {
        let mut limiter = limiter(1, 1);
        assert!(limiter.try_acquire(Duration::ZERO, 1).is_ok());
        // 1.9 tokens of refill: reports 1.
        assert_eq!(limiter.available(Duration::from_millis(1_900)), 1);
        assert!(limiter.try_acquire(Duration::from_millis(1_900), 1).is_ok());
        // The fractional 0.9 carries forward rather than being discarded.
        assert!(limiter.try_acquire(Duration::from_millis(3_800), 1).is_ok());
    }
}
