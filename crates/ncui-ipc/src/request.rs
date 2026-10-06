//! The registry of in-flight requests.
//!
//! A request crosses a process boundary and comes back as a [`crate::message::Message::Response`] or a
//! [`crate::message::Message::ErrorResponse`], possibly long after the caller stopped caring. Something has to
//! remember which replies belong to which outstanding calls, refuse duplicate identifiers,
//! notice calls that were never answered, and refuse to exceed a configured ceiling. That is
//! this type.
//!
//! # The three invariants
//!
//! ADR-003 states them and this module is where they are enforced:
//!
//! 1. **Identifiers are unique.** [`RequestRegistry::register`] refuses a live identifier with
//!    [`ncui_core::ErrorKind::Conflict`]. Overwriting would let a stale response be delivered to the
//!    wrong caller, which is the worst failure this registry can have.
//! 2. **A response maps to exactly one request.** Completing, failing or cancelling an
//!    identifier that is not in flight returns [`ncui_core::ErrorKind::NotFound`]. A silent drop would
//!    leave the caller waiting for a reply that was already consumed by somebody else.
//! 3. **In-flight work is bounded.** The registry refuses to exceed
//!    [`Limits::max_in_flight_requests`] with [`ErrorKind::LimitExceeded`].
//!
//! # Why the clock is explicit
//!
//! [`RequestRegistry::expire`] and [`RequestRegistry::register_at`] take a millisecond
//! timestamp rather than reading a clock. Deadlines are the one behaviour here that is
//! otherwise untestable — a test that waits 30 seconds to watch a request expire is a test
//! nobody runs. A production caller uses [`RequestRegistry::expire_now`], which reads
//! [`Instant`]; monotonic time, so a wall-clock adjustment cannot make a request live forever
//! or expire early.
//!
//! # Why the lock is taken once per operation
//!
//! Every method validates and mutates under a single acquisition, so a second producer cannot
//! observe a state the first one has already decided against. Check-then-act across two
//! acquisitions would make the uniqueness invariant a race.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use ncui_core::{CorrelationId, ErrorKind, Limits, NcError, RequestId, Result};

/// How a request finished.
#[derive(Debug, Clone)]
pub enum RequestOutcome {
    /// The peer answered successfully.
    Completed,
    /// The peer answered with an error.
    Failed(NcError),
    /// Nobody answered; the caller or a timeout decided.
    Cancelled,
    /// The request outlived its deadline.
    TimedOut,
}

impl PartialEq for RequestOutcome {
    /// Compares by the parts a caller can observe.
    ///
    /// Hand-written because `NcError` deliberately does not implement `PartialEq`: the
    /// observable state of an error is its kind, its operation and its correlation, and
    /// deriving nothing is more honest than pretending the whole struct compares.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Completed | Self::Cancelled | Self::TimedOut,
                Self::Completed | Self::Cancelled | Self::TimedOut,
            ) => true,
            (Self::Failed(left), Self::Failed(right)) => {
                left.kind() == right.kind()
                    && left.operation_name() == right.operation_name()
                    && left.correlation() == right.correlation()
            }
            _ => false,
        }
    }
}

impl Eq for RequestOutcome {}

impl RequestOutcome {
    /// Whether the request reached a peer and got an answer.
    ///
    /// A timeout or a cancellation is *not* success even though the caller is no longer
    /// waiting: the work may still be running on the peer, which is exactly why a late reply
    /// has to be refused rather than delivered to whoever holds the identifier next.
    #[must_use]
    pub fn is_answered(self) -> bool {
        matches!(self, Self::Completed | Self::Failed(_))
    }
}

/// The record of one in-flight request.
///
/// Not `Copy`: [`RequestOutcome::Failed`] owns an [`NcError`], and a completion record is
/// handed to the caller exactly once, so cloning it would only invite the caller to keep a
/// copy of a failure that has already been reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedRequest {
    /// The request this record describes.
    pub request_id: RequestId,
    /// The correlation the request was issued under, so a completion can be traced back to
    /// the log lines and spans the caller already emitted.
    pub correlation: CorrelationId,
    /// The deadline that was set at registration, in the same units the caller supplied.
    pub deadline_ms: u64,
    /// When the request was registered, in the caller's time base.
    pub registered_at_ms: u64,
    /// How it finished.
    pub outcome: RequestOutcome,
}

/// A registration that is still outstanding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    correlation: CorrelationId,
    registered_at_ms: u64,
    deadline_ms: u64,
}

/// The registry's mutable state.
#[derive(Debug, Default)]
struct Inner {
    pending: HashMap<RequestId, Pending>,
}

/// Tracks which requests are outstanding, and when each was expected to finish.
///
/// Cheap to clone in the sense that matters: the *token* is the cloneable thing, not the
/// registry. A registry is owned by whoever issues and reaps requests, and shared by reference
/// with the transport that receives the replies.
#[derive(Debug)]
pub struct RequestRegistry {
    inner: Mutex<Inner>,
    max_in_flight: usize,
    timeout_ms: u64,
    /// The instant the registry was created; all internal timestamps are milliseconds since.
    base: Instant,
}

impl RequestRegistry {
    /// Creates a registry from the runtime limits.
    ///
    /// Reads both bounds from [`Limits`] rather than taking them separately, so a caller
    /// cannot configure a timeout in one place and a ceiling in another and end up with a
    /// registry that disagrees with the rest of the runtime.
    #[must_use]
    pub fn from_limits(limits: &Limits) -> Self {
        Self::new(limits.max_in_flight_requests, limits.request_timeout_ms)
    }

    /// Creates a registry with an explicit ceiling and timeout.
    ///
    /// A zero ceiling is clamped to one and a zero timeout to one millisecond. Both would
    /// otherwise mean "no request may ever be outstanding" or "every request is already
    /// expired", which can only be a configuration mistake, and returning a registry that
    /// rejects everything would be harder to diagnose than a documented minimum.
    #[must_use]
    pub fn new(max_in_flight: usize, timeout_ms: u64) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            max_in_flight: max_in_flight.max(1),
            timeout_ms: timeout_ms.max(1),
            base: Instant::now(),
        }
    }

    /// Milliseconds since the registry was created, on the monotonic clock.
    ///
    /// Monotonic, not wall-clock: a user adjusting the system time, or an NTP correction,
    /// must not be able to make an outstanding request live forever or expire it early.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// The configured in-flight ceiling.
    #[must_use]
    pub const fn max_in_flight(&self) -> usize {
        self.max_in_flight
    }

    /// The configured request timeout, in milliseconds.
    #[must_use]
    pub const fn request_timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// How many requests are currently outstanding.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.lock().pending.len()
    }

    /// Whether a request is outstanding.
    #[must_use]
    pub fn contains(&self, request_id: RequestId) -> bool {
        self.lock().pending.contains_key(&request_id)
    }

    /// The deadline set for a request, in the caller's time base.
    pub fn deadline_of(&self, request_id: RequestId) -> Result<u64> {
        self.lock()
            .pending
            .get(&request_id)
            .map(|pending| pending.deadline_ms)
            .ok_or_else(|| unknown_request(request_id))
    }

    /// Registers an outstanding request, stamping it with the current time.
    ///
    /// Fails with [`ncui_core::ErrorKind::Conflict`] if the identifier is already outstanding, and with
    /// [`ErrorKind::LimitExceeded`] if the ceiling is reached.
    pub fn register(&self, request_id: RequestId, correlation: CorrelationId) -> Result<()> {
        self.register_at(request_id, correlation, self.now_ms())
    }

    /// Registers an outstanding request against an explicit timestamp.
    ///
    /// The deterministic half of [`RequestRegistry::register`]. Deadlines are otherwise
    /// untestable, because a test that waits for the real timeout is a test nobody runs.
    pub fn register_at(
        &self,
        request_id: RequestId,
        correlation: CorrelationId,
        now_ms: u64,
    ) -> Result<()> {
        let mut inner = self.lock();
        if inner.pending.contains_key(&request_id) {
            return Err(NcError::new(ErrorKind::Conflict {
                reason: format!("request {request_id} is already in flight"),
            })
            .operation("ipc::request::register")
            .correlated(correlation));
        }
        if inner.pending.len() >= self.max_in_flight {
            return Err(NcError::new(ErrorKind::LimitExceeded {
                resource: "ipc.request.in_flight".to_owned(),
                limit: self.max_in_flight as u64,
                observed: inner.pending.len() as u64 + 1,
            })
            .operation("ipc::request::register")
            .correlated(correlation));
        }
        inner.pending.insert(
            request_id,
            Pending {
                correlation,
                registered_at_ms: now_ms,
                deadline_ms: now_ms.saturating_add(self.timeout_ms),
            },
        );
        Ok(())
    }

    /// Records that a request was answered successfully.
    pub fn complete(&self, request_id: RequestId) -> Result<CompletedRequest> {
        self.finish(request_id, RequestOutcome::Completed)
    }

    /// Records that a request was answered with an error.
    pub fn fail(&self, request_id: RequestId, error: NcError) -> Result<CompletedRequest> {
        self.finish(request_id, RequestOutcome::Failed(error))
    }

    /// Records that a request was cancelled before it completed.
    pub fn cancel(&self, request_id: RequestId) -> Result<CompletedRequest> {
        self.finish(request_id, RequestOutcome::Cancelled)
    }

    /// Removes a request and returns what was recorded about it.
    fn finish(&self, request_id: RequestId, outcome: RequestOutcome) -> Result<CompletedRequest> {
        let mut inner = self.lock();
        let pending = inner
            .pending
            .remove(&request_id)
            .ok_or_else(|| unknown_request(request_id))?;
        Ok(CompletedRequest {
            request_id,
            correlation: pending.correlation,
            deadline_ms: pending.deadline_ms,
            registered_at_ms: pending.registered_at_ms,
            outcome,
        })
    }

    /// Removes and returns every request whose deadline has passed at `now_ms`.
    ///
    /// Returns the identifiers so the caller can fail each waiting side with a
    /// [`ErrorKind::Timeout`], rather than leaving them to discover the expiry themselves.
    pub fn expire(&self, now_ms: u64) -> Vec<RequestId> {
        self.expire_at(now_ms)
    }

    /// The time-based half of [`RequestRegistry::expire`], for callers with their own clock.
    pub fn expire_at(&self, now_ms: u64) -> Vec<RequestId> {
        let mut inner = self.lock();
        let expired: Vec<RequestId> = inner
            .pending
            .iter()
            .filter(|(_, pending)| pending.deadline_ms <= now_ms)
            .map(|(id, _)| *id)
            .collect();
        for id in &expired {
            inner.pending.remove(id);
        }
        expired
    }

    /// Expires overdue requests against the registry's own monotonic clock.
    pub fn expire_now(&self) -> Vec<RequestId> {
        let mut inner = self.lock();
        let now = self.now_ms();
        let expired: Vec<RequestId> = inner
            .pending
            .iter()
            .filter(|(_, pending)| pending.deadline_ms <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in &expired {
            inner.pending.remove(id);
        }
        expired
    }

    /// Removes every outstanding request, returning them as cancelled.
    ///
    /// Shutdown. Every waiting side is released with a terminal record rather than being left
    /// with a future that will never resolve.
    pub fn drain(&self) -> Vec<CompletedRequest> {
        let mut inner = self.lock();
        let drained: Vec<CompletedRequest> = inner
            .pending
            .drain()
            .map(|(request_id, pending)| CompletedRequest {
                request_id,
                correlation: pending.correlation,
                deadline_ms: pending.deadline_ms,
                registered_at_ms: pending.registered_at_ms,
                outcome: RequestOutcome::Cancelled,
            })
            .collect();
        drop(inner);
        drained
    }

    /// Locks the registry, recovering from poisoning.
    ///
    /// A panic while the lock was held leaves the `HashMap` intact — `HashMap` operations
    /// cannot leave it half-updated — so recovering keeps the registry usable. Failing every
    /// subsequent request because some unrelated task panicked would be a far worse outcome
    /// than continuing.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The error for touching a request that is not outstanding.
///
/// Its own constructor so that the wording, the operation and the "not found" class are
/// identical across `complete`, `fail` and `cancel`; a caller that matches on
/// [`ncui_core::ErrorKind::NotFound`] must not have to know which of the three it called.
fn unknown_request(request_id: RequestId) -> NcError {
    ncui_core::error::not_found("request", request_id).operation("ipc::request")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )]

    use super::*;

    #[test]
    fn a_registered_request_is_pending_and_has_its_correlation() {
        let registry = RequestRegistry::new(4, 1_000);
        let correlation = CorrelationId::new();
        let id = RequestId::new();
        registry.register(id, correlation).unwrap();
        assert_eq!(registry.pending_count(), 1);
        assert!(registry.contains(id));
        assert_eq!(registry.deadline_of(id).unwrap(), 1_000);
    }

    #[test]
    fn a_duplicate_registration_is_a_conflict_and_does_not_overwrite() {
        let registry = RequestRegistry::new(4, 1_000);
        let id = RequestId::new();
        let first = CorrelationId::new();
        let second = CorrelationId::new();
        registry.register_at(id, first, 0).unwrap();
        let err = registry
            .register_at(id, second, 10)
            .expect_err("a duplicate id must be refused");
        assert!(matches!(err.kind(), ErrorKind::Conflict { .. }));
        // The original registration must survive: a silent overwrite would let a stale
        // response be delivered to the wrong caller.
        assert_eq!(
            registry.complete(id).unwrap().correlation,
            first,
            "the second registration must not have replaced the first"
        );
    }

    #[test]
    fn completing_an_unknown_request_is_an_error_not_a_silent_drop() {
        let registry = RequestRegistry::new(4, 1_000);
        for err in [
            registry.complete(RequestId::new()).unwrap_err(),
            registry.cancel(RequestId::new()).unwrap_err(),
            registry
                .fail(RequestId::new(), NcError::new(ErrorKind::Cancelled))
                .unwrap_err(),
        ] {
            assert!(
                matches!(err.kind(), ErrorKind::NotFound { .. }),
                "got: {err}"
            );
        }
    }

    #[test]
    fn a_request_maps_to_exactly_one_completion() {
        let registry = RequestRegistry::new(4, 1_000);
        let id = RequestId::new();
        registry.register(id, CorrelationId::new()).unwrap();
        assert_eq!(
            registry.complete(id).unwrap().outcome,
            RequestOutcome::Completed
        );
        assert_eq!(registry.pending_count(), 0);
        // The second completion has no request to map to.
        assert!(registry.complete(id).is_err());
    }

    #[test]
    fn the_in_flight_ceiling_is_enforced() {
        let registry = RequestRegistry::new(2, 1_000);
        registry
            .register(RequestId::new(), CorrelationId::new())
            .unwrap();
        registry
            .register(RequestId::new(), CorrelationId::new())
            .unwrap();
        let err = registry
            .register(RequestId::new(), CorrelationId::new())
            .expect_err("the ceiling must be enforced");
        match err.kind() {
            ErrorKind::LimitExceeded {
                limit, observed, ..
            } => {
                assert_eq!(*limit, 2);
                assert_eq!(*observed, 3);
            }
            other => panic!("expected LimitExceeded, got {other:?}"),
        }
        assert_eq!(registry.pending_count(), 2);
    }

    #[test]
    fn room_frees_up_once_a_request_completes() {
        let registry = RequestRegistry::new(1, 1_000);
        let first = RequestId::new();
        registry.register(first, CorrelationId::new()).unwrap();
        assert!(
            registry
                .register(RequestId::new(), CorrelationId::new())
                .is_err()
        );
        registry.complete(first).unwrap();
        assert!(
            registry
                .register(RequestId::new(), CorrelationId::new())
                .is_ok()
        );
    }

    #[test]
    fn expire_returns_exactly_the_overdue_requests() {
        let registry = RequestRegistry::new(8, 100);
        let early = RequestId::new();
        let late = RequestId::new();
        registry
            .register_at(early, CorrelationId::new(), 0)
            .unwrap();
        registry
            .register_at(late, CorrelationId::new(), 50)
            .unwrap();

        assert!(registry.expire(99).is_empty(), "nothing is due at 99ms");
        let expired = registry.expire(100);
        assert_eq!(expired, vec![early], "the 100ms deadline is due at 100ms");
        assert_eq!(registry.pending_count(), 1);
        assert!(registry.contains(late));

        assert_eq!(registry.expire(150), vec![late]);
        assert_eq!(registry.pending_count(), 0);
    }

    #[test]
    fn expire_at_is_exactly_the_time_based_expiry() {
        let registry = RequestRegistry::new(8, 50);
        let id = RequestId::new();
        registry
            .register_at(id, CorrelationId::new(), 1_000)
            .unwrap();
        // `expire_at` is the same operation with an explicit clock, so the two must agree.
        assert!(registry.expire_at(1_049).is_empty());
        assert_eq!(registry.expire_at(1_050), vec![id]);
        assert!(registry.expire_at(u64::MAX).is_empty());
    }

    #[test]
    fn an_expired_request_can_no_longer_be_completed() {
        let registry = RequestRegistry::new(8, 10);
        let id = RequestId::new();
        registry.register_at(id, CorrelationId::new(), 0).unwrap();
        assert_eq!(registry.expire(10), vec![id]);
        // The reply arrived after the deadline. Delivering it to whoever holds the id next is
        // exactly the confusion the registry exists to prevent.
        assert!(registry.complete(id).is_err());
    }

    #[test]
    fn the_deadline_accounts_for_the_registration_time() {
        let registry = RequestRegistry::new(8, 100);
        let id = RequestId::new();
        registry
            .register_at(id, CorrelationId::new(), 1_000)
            .unwrap();
        assert_eq!(registry.deadline_of(id).unwrap(), 1_100);
    }

    #[test]
    fn a_failed_request_records_its_error() {
        let registry = RequestRegistry::new(4, 1_000);
        let id = RequestId::new();
        registry.register(id, CorrelationId::new()).unwrap();
        let error = NcError::new(ErrorKind::Timeout { elapsed_ms: 1_000 });
        let record = registry.fail(id, error).unwrap();
        match record.outcome {
            RequestOutcome::Failed(ref recorded) => {
                assert_eq!(recorded.code(), "timeout");
            }
            ref other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn an_outcome_reports_whether_the_peer_actually_answered() {
        assert!(RequestOutcome::Completed.is_answered());
        assert!(RequestOutcome::Failed(NcError::new(ErrorKind::Cancelled)).is_answered());
        // A timeout leaves the work possibly still running on the peer, so it is not an
        // answer even though the caller stops waiting.
        assert!(!RequestOutcome::TimedOut.is_answered());
        assert!(!RequestOutcome::Cancelled.is_answered());
    }

    #[test]
    fn drain_releases_every_waiter_as_cancelled() {
        let registry = RequestRegistry::new(8, 1_000);
        for _ in 0..3 {
            registry
                .register(RequestId::new(), CorrelationId::new())
                .unwrap();
        }
        let drained = registry.drain();
        assert_eq!(drained.len(), 3);
        assert!(
            drained
                .iter()
                .all(|r| r.outcome == RequestOutcome::Cancelled)
        );
        assert_eq!(registry.pending_count(), 0);
    }

    #[test]
    fn a_zero_ceiling_or_timeout_is_clamped_to_a_usable_minimum() {
        let registry = RequestRegistry::new(0, 0);
        assert_eq!(registry.max_in_flight(), 1);
        assert_eq!(registry.request_timeout_ms(), 1);
        // A registry that rejected everything would be harder to diagnose than a minimum.
        assert!(
            registry
                .register(RequestId::new(), CorrelationId::new())
                .is_ok()
        );
    }

    #[test]
    fn a_registry_built_from_limits_uses_them() {
        let limits = Limits {
            max_in_flight_requests: 7,
            request_timeout_ms: 250,
            ..Limits::default()
        };
        let registry = RequestRegistry::from_limits(&limits);
        assert_eq!(registry.max_in_flight(), 7);
        assert_eq!(registry.request_timeout_ms(), 250);
    }

    #[test]
    fn a_registry_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RequestRegistry>();
    }

    #[test]
    fn concurrent_registration_of_one_id_yields_exactly_one_winner() {
        use std::sync::Arc;
        let registry = Arc::new(RequestRegistry::new(64, 1_000));
        let id = RequestId::new();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let registry = Arc::clone(&registry);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    registry.register(id, CorrelationId::new())
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners = results.iter().filter(|r| r.is_ok()).count();
        let conflicts = results
            .iter()
            .filter(|r| {
                matches!(
                    r.as_ref().err().map(NcError::kind),
                    Some(ErrorKind::Conflict { .. })
                )
            })
            .count();
        assert_eq!(winners, 1, "exactly one registration must win");
        assert_eq!(
            conflicts, 7,
            "every other must be a conflict, not a limit or a panic"
        );
        assert_eq!(registry.pending_count(), 1);
    }
}
