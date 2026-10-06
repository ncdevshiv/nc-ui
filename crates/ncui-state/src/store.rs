//! The revisioned store: a keyed collection that remembers what it changed.
//!
//! [`Store`] is deliberately not "a map with a version stamp". The whole point of this
//! crate is that a mutation must not force a consumer to resynchronise a whole tree
//! (spec 15), so a store has to be able to answer *what changed*, not only *what is*.
//!
//! # The model
//!
//! Every entry the store holds is at some **revision**. Revision `0` is the empty store;
//! each committed mutation produces exactly one new revision, strictly greater than every
//! revision that came before it. Each committed mutation also records a [`Patch`] — the
//! delta that moved the store from revision `n` to revision `n + 1` — in a bounded ring.
//!
//! Three access paths exist, and they are not redundant:
//!
//! * [`Store::take_pending`] — the **producer's** hand-off buffer. Lossless, drained
//!   explicitly, coalesced once it grows past a soft limit. This is the cheap path for a
//!   renderer in the same task: it costs one map update per frame regardless of how many
//!   mutations happened in that frame.
//! * [`Store::patch_since`] — the **catch-up** path. Replays the retained ring from a
//!   revision the caller names, coalesced into one patch. This is how a subscriber that
//!   missed ten frames gets one patch instead of ten.
//! * [`Store::iter`] — the **full read**, used only when a consumer genuinely has to
//!   resynchronise from scratch. Nothing in the incremental path routes through it.
//!
//! The ring is bounded ([`Store::with_history_capacity`]) because a store that retained
//! every delta forever would be an unbounded memory growth path (spec 96). When the ring
//! has already discarded a revision a caller asks for, the store **refuses** with
//! [`ncui_core::ErrorKind::Conflict`] and names the missing range rather than returning a
//! silently incomplete answer — the gap is the one thing an incremental protocol must
//! never hide.
//!
//! # Invariants
//!
//! 1. `revision()` is strictly increasing across every state-changing operation, and
//!    never advances for an operation that did not change anything (`remove` of an absent
//!    key). It saturates at `u64::MAX` rather than wrapping, so it is monotonic for the
//!    lifetime of the store.
//! 2. A store's contents are a pure function of the patch sequence applied to it.
//!    Applying `coalesce(p)` and applying `p` are therefore indistinguishable.
//! 3. The pending buffer, once drained, is exactly the set of changes committed since
//!    the last drain, and its reported range `(from, to]` is non-empty whenever it is
//!    non-`None`.
//! 4. The ring holds a contiguous suffix of the revision sequence. `retained_range()`
//!    names it, and `dropped_since()` names exactly which revisions a too-slow caller has
//!    lost access to.

use std::collections::{BTreeMap, VecDeque};
use std::ops::RangeInclusive;

use ncui_core::error::validation;
use ncui_core::{ErrorKind, NcError, Result};

use crate::patch::{Change, Patch, PatchDelivery};
use crate::subscription::{Subscription, SubscriptionFilter};

/// Revisions retained for catch-up by default.
///
/// Sized for a consumer that polls roughly once per frame while the producer mutates in
/// the hundreds: a few seconds of history at a normal frame cadence. A consumer that
/// falls further behind than this is told to resynchronise rather than being handed a
/// gap, which is the correct trade — a bounded buffer is a hard requirement of spec 96,
/// and a gap is worse than a resync because it is silent.
pub const DEFAULT_HISTORY_CAPACITY: usize = 1024;

/// Pending-buffer coalescing threshold.
///
/// The pending buffer is coalesced once it holds more than this many changes. Coalescing
/// is lossy for *intermediate* values but never for the final state of a key, which is
/// exactly the contract a renderer needs: it will draw each key's newest value, not its
/// historical ones.
pub const DEFAULT_PENDING_LIMIT: usize = 256;

/// One committed revision and the delta that produced it.
#[derive(Debug, Clone)]
struct Recorded<T> {
    revision: u64,
    patch: Patch<T>,
}

/// A revisioned collection of records keyed by [`StateKey`](crate::StateKey).
///
/// Iteration is over a [`BTreeMap`], so [`Store::iter`] yields keys in sorted order and
/// is stable across runs. That determinism is worth the `O(log n)` lookup: render order,
/// snapshot comparison and test fixtures all need "the same order every time", and a
/// hash-ordered map would make every one of them a source of flaky, order-dependent
/// failures.
#[derive(Debug, Clone)]
pub struct Store<T> {
    entries: BTreeMap<String, T>,
    revision: u64,
    history: VecDeque<Recorded<T>>,
    history_capacity: usize,
    pending: Patch<T>,
    pending_from: u64,
    pending_limit: usize,
}

impl<T> Store<T> {
    /// Creates an empty store at revision `0` with the default history capacity and
    /// pending limit.
    ///
    /// Revision `0` is the empty store by definition: it is the cursor a consumer may
    /// start from, and it is what `patch_since(0)` replays from.
    #[must_use]
    pub fn new() -> Self {
        Self::with_history_capacity(DEFAULT_HISTORY_CAPACITY)
    }

    /// Creates an empty store retaining `capacity` revisions for catch-up.
    ///
    /// A capacity of `0` disables catch-up entirely: nothing is retained, so every
    /// [`Store::patch_since`] call for a revision older than the store's current one
    /// fails with [`ErrorKind::Conflict`] and reports the whole gap as dropped. That is
    /// the right configuration only for a store nothing is subscribed to — a consumer
    /// that needs deltas must be given a capacity, or it will be told to resynchronise
    /// on its first poll.
    #[must_use]
    pub fn with_history_capacity(capacity: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            revision: 0,
            history: VecDeque::new(),
            history_capacity: capacity,
            pending: Patch::empty(),
            pending_from: 0,
            pending_limit: DEFAULT_PENDING_LIMIT,
        }
    }

    /// Overrides the pending-buffer coalescing threshold.
    ///
    /// Returns the store so it can be chained onto
    /// [`Store::with_history_capacity`]. Lowering this trades intermediate fidelity for
    /// a smaller producer-side buffer; it never affects what the ring retains, so it can
    /// never cause a subscriber to miss a revision.
    #[must_use]
    pub fn with_pending_limit(mut self, limit: usize) -> Self {
        self.pending_limit = limit;
        self
    }

    /// The store's current revision.
    ///
    /// The number itself is opaque; what matters is that it strictly increases whenever
    /// the contents change, and that a consumer holding an older value can name it here
    /// to ask what it missed.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// How many records the store holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store holds no records at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Looks up the value stored at `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&T> {
        self.entries.get(key)
    }

    /// Whether a record exists at `key`.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Iterates every record in sorted key order.
    ///
    /// This is the full-read path. It exists for snapshots, tests and resynchronisation;
    /// the incremental paths ([`Store::take_pending`], [`Store::patch_since`]) never go
    /// through it, which is what keeps a single mutation from costing a full traversal.
    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &T)> + DoubleEndedIterator + '_ {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }

    /// Drains the producer hand-off buffer, returning everything committed since the
    /// previous drain together with the revision range it covers.
    ///
    /// Returns `None` when nothing has been committed since the last drain. The
    /// delivered range is `(from_revision, to_revision]`, so a consumer that applies it
    /// may set its cursor to [`PatchDelivery::to_revision`] and skip nothing.
    ///
    /// The buffer is coalesced once it exceeds the pending limit, so the returned patch
    /// may be shorter than the number of mutations since the last drain. That is safe by
    /// construction — see [`Patch::coalesce`] — and it is what stops a producer that
    /// never drains from growing without bound.
    #[must_use]
    pub fn take_pending(&mut self) -> Option<PatchDelivery<T>> {
        if self.pending.is_empty() {
            return None;
        }
        let patch = std::mem::take(&mut self.pending);
        Some(PatchDelivery::new(self.pending_from, self.revision, patch))
    }

    /// The undrained changes, without consuming them.
    #[must_use]
    pub fn pending_patch(&self) -> &Patch<T> {
        &self.pending
    }

    /// The revision range the pending buffer currently covers, without draining it.
    ///
    /// `None` when the buffer is empty. Otherwise `(from, to]` with `from < to`.
    #[must_use]
    pub fn pending_range(&self) -> Option<RangeInclusive<u64>> {
        if self.pending.is_empty() {
            None
        } else {
            Some(self.pending_from..=self.revision)
        }
    }

    /// How many revisions the ring currently retains.
    #[must_use]
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// The ring's configured capacity, which is an upper bound on
    /// [`Store::history_len`].
    #[must_use]
    pub const fn history_capacity(&self) -> usize {
        self.history_capacity
    }

    /// The revision range the ring still retains, `oldest..=current`.
    ///
    /// `None` only when the store has never been mutated. A store with a capacity of `0`
    /// that *has* been mutated returns `None` as well — it retains nothing, so there is
    /// no range to report. Use [`Store::dropped_since`] to tell those two cases apart.
    #[must_use]
    pub fn retained_range(&self) -> Option<RangeInclusive<u64>> {
        self.history
            .front()
            .map(|oldest| oldest.revision..=self.revision)
    }

    /// Exactly which revisions a caller asking for `since_revision` can no longer see.
    ///
    /// `None` means the request is answerable from the ring (or nothing has happened
    /// since). `Some(range)` means those revisions were evicted before the caller asked,
    /// and the caller has a genuine gap that no amount of catch-up can close.
    #[must_use]
    pub fn dropped_since(&self, since_revision: u64) -> Option<RangeInclusive<u64>> {
        if since_revision >= self.revision {
            // Either the caller is caught up, or it claims to be ahead of the store; the
            // latter is a caller error reported separately, not a dropped range.
            return None;
        }
        match self.history.front() {
            None => Some((since_revision + 1)..=self.revision),
            Some(oldest) => {
                let first_available = oldest.revision;
                if since_revision + 1 < first_available {
                    Some((since_revision + 1)..=(first_available - 1))
                } else {
                    None
                }
            }
        }
    }

    /// Opens a filtered view over this store's patch stream.
    ///
    /// The returned [`Subscription`] borrows the store, which is what makes the common
    /// case free: there is no registration, no callback, no wake-up queue, and nothing to
    /// unregister. A subscription is a cursor, not a participant.
    #[must_use]
    pub fn subscribe(&self, filter: SubscriptionFilter) -> Subscription<'_, T> {
        Subscription::new(self, filter)
    }

    /// Applies one change to the backing map without advancing the revision.
    ///
    /// Called only from [`Patch::apply_to`], which commits the whole batch as a single
    /// revision once every change has been applied.
    pub(crate) fn apply_change_in_place(&mut self, change: &Change<T>)
    where
        T: Clone,
    {
        match change {
            Change::Upsert { key, value } => {
                self.entries.insert(key.clone(), value.clone());
            }
            Change::Remove { key } => {
                self.entries.remove(key);
            }
        }
    }
}

impl<T> Store<T>
where
    T: Clone,
{
    /// Removes the record at `key`, returning it if it was present.
    ///
    /// Removing an absent key changes nothing and therefore **does not** advance the
    /// revision or emit a change. That is not an optimisation: a revision that advanced
    /// without a corresponding change would force every subscriber to wake up and find
    /// an empty diff, and it would make "revision *n*" no longer a faithful name for a
    /// state.
    ///
    /// Recording the removal in the ring and the pending buffer copies the patch, so this
    /// requires `T: Clone` even though the removal itself carries no value. The bound is
    /// the price of the history ring, and every store in this runtime clones its records
    /// anyway.
    pub fn remove(&mut self, key: &str) -> Option<T> {
        let removed = self.entries.remove(key)?;
        self.commit(Patch::single(Change::remove(key)));
        Some(removed)
    }

    /// Commits a mutation: advances the revision and records the delta.
    ///
    /// Revision advancement saturates instead of wrapping, so the sequence is monotonic
    /// even in the (unreachable in practice) case of `u64::MAX` mutations.
    pub(crate) fn commit(&mut self, patch: Patch<T>) {
        if self.pending.is_empty() {
            self.pending_from = self.revision;
        }
        self.revision = self.revision.saturating_add(1);
        if self.history_capacity > 0 {
            while self.history.len() >= self.history_capacity {
                self.history.pop_front();
            }
            self.history.push_back(Recorded {
                revision: self.revision,
                patch: patch.clone(),
            });
        }
        self.pending.append(patch);
        if self.pending.len() > self.pending_limit {
            self.pending.coalesce();
        }
    }

    /// Inserts or replaces the record at `key`, returning the value it displaced.
    ///
    /// Always advances the revision and always emits an [`Change::Upsert`], even when the
    /// new value equals the old one. A caller that writes an unchanged value has made a
    /// statement about that key; suppressing the change would make the revision sequence
    /// stop being a faithful description of what the producer did. Callers that want to
    /// skip no-op writes must compare first.
    pub fn insert(&mut self, key: impl Into<String>, value: T) -> Option<T> {
        let key = key.into();
        let previous = self.entries.insert(key.clone(), value.clone());
        self.commit(Patch::single(Change::Upsert { key, value }));
        previous
    }

    /// Replays every retained revision after `since_revision` as one coalesced patch.
    ///
    /// This is the catch-up primitive. The returned [`PatchDelivery`] carries the range
    /// `(since_revision, store.revision()]`, so a caller can hand it straight to a
    /// consumer and then advance its cursor to [`PatchDelivery::to_revision`].
    ///
    /// # Errors
    ///
    /// * [`ErrorKind::Validation`] if `since_revision` is greater than the store's
    ///   current revision — the caller believes it has seen changes that do not exist,
    ///   which is a caller bug and must not be papered over.
    /// * [`ErrorKind::Conflict`] if the ring has already evicted revisions the caller
    ///   needs. The reason string names the exact lost range; recover by resynchronising
    ///   from [`Store::iter`] rather than by retrying.
    pub fn patch_since(&self, since_revision: u64) -> Result<PatchDelivery<T>> {
        if since_revision > self.revision {
            return Err(validation(
                "since_revision",
                format!(
                    "requested revision {since_revision} is ahead of store revision {}",
                    self.revision
                ),
            )
            .operation("state::store::patch_since"));
        }
        if let Some(dropped) = self.dropped_since(since_revision) {
            return Err(NcError::new(ErrorKind::Conflict {
                reason: format!(
                    "revision range {}-{} was evicted before revision {since_revision} could \
                     be replayed; resynchronise from a full read",
                    dropped.start(),
                    dropped.end()
                ),
            })
            .operation("state::store::patch_since"));
        }
        let mut patch = Patch::empty();
        for recorded in &self.history {
            if recorded.revision > since_revision {
                patch.append(recorded.patch.clone());
            }
        }
        patch.coalesce();
        Ok(PatchDelivery::new(since_revision, self.revision, patch))
    }
}

impl<T> Default for Store<T> {
    fn default() -> Self {
        Self::new()
    }
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
    use crate::patch::StateKey;

    fn contents(store: &Store<i32>) -> Vec<(String, i32)> {
        store.iter().map(|(k, v)| (k.to_owned(), *v)).collect()
    }

    #[test]
    fn an_empty_store_starts_at_revision_zero() {
        let store = Store::<i32>::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert_eq!(store.revision(), 0);
        assert_eq!(store.history_len(), 0);
        assert_eq!(store.retained_range(), None);
        assert_eq!(store.dropped_since(0), None);
        assert!(store.pending_patch().is_empty());
        assert_eq!(store.pending_range(), None);
        assert_eq!(store.iter().count(), 0);
        let default = Store::<i32>::default();
        assert_eq!(default.revision(), 0);
        assert!(default.is_empty());
        assert_eq!(default.history_capacity(), DEFAULT_HISTORY_CAPACITY);
    }

    #[test]
    fn every_insert_advances_the_revision_by_exactly_one() {
        let mut store = Store::new();
        let mut previous = store.revision();
        for index in 0..100 {
            store.insert(format!("k{index}"), index);
            assert_eq!(
                store.revision(),
                previous + 1,
                "revision must advance exactly once per mutation"
            );
            assert!(store.revision() > previous, "revision must be monotonic");
            previous = store.revision();
        }
    }

    #[test]
    fn removing_an_absent_key_does_not_advance_the_revision() {
        let mut store = Store::new();
        store.insert("a", 1);
        let before = store.revision();
        assert_eq!(store.remove("missing"), None);
        assert_eq!(store.revision(), before);
        assert_eq!(store.history_len(), 1, "no change, no history entry");
    }

    #[test]
    fn insert_returns_the_displaced_value_and_remove_returns_the_present_one() {
        let mut store = Store::new();
        assert_eq!(store.insert("a", 1), None);
        assert_eq!(store.insert("a", 2), Some(1));
        assert_eq!(store.get("a"), Some(&2));
        assert!(store.contains_key("a"));
        assert_eq!(store.remove("a"), Some(2));
        assert_eq!(store.get("a"), None);
        assert!(!store.contains_key("a"));
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn iteration_is_in_sorted_key_order() {
        let mut store = Store::new();
        for key in ["c", "a", "b"] {
            store.insert(key, 1);
        }
        let keys: Vec<&str> = store.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn pending_batches_drain_and_report_their_revision_range() {
        let mut store = Store::new();
        assert_eq!(store.take_pending(), None, "nothing committed yet");

        store.insert("a", 1);
        store.insert("b", 2);
        store.remove("a");

        let range = store.pending_range().expect("pending after mutations");
        assert_eq!(
            range.start(),
            &0,
            "buffer starts at the last drained revision"
        );
        assert_eq!(range.end(), &store.revision());

        let delivery = store.take_pending().expect("pending after mutations");
        assert_eq!(delivery.from_revision(), 0);
        assert_eq!(delivery.to_revision(), 3);
        assert_eq!(delivery.len(), 3);
        assert_eq!(store.pending_range(), None);
        assert_eq!(store.take_pending(), None, "draining twice yields nothing");
        // Draining does not roll back the store or forget the ring.
        assert_eq!(contents(&store), vec![("b".to_owned(), 2)]);
        assert_eq!(store.history_len(), 3);
    }

    #[test]
    fn pending_range_restarts_from_the_revision_at_drain_time() {
        let mut store = Store::new();
        store.insert("a", 1);
        store.insert("b", 2);
        let _ = store.take_pending();

        store.insert("c", 3);
        let range = store.pending_range().expect("pending after second batch");
        assert_eq!(range.start(), &2);
        assert_eq!(range.end(), &3);
    }

    #[test]
    fn pending_coalesces_once_it_exceeds_the_limit_but_keeps_final_values() {
        let mut store = Store::with_history_capacity(1024).with_pending_limit(2);
        store.insert("a", 1);
        store.insert("a", 2);
        store.insert("a", 3);
        store.insert("a", 4);

        let pending = store.pending_patch();
        assert!(
            pending.len() <= 2,
            "pending must be coalesced past the limit, got {}",
            pending.len()
        );
        assert!(
            pending.len() < 4,
            "four writes to one key must not leave four pending changes"
        );
        assert_eq!(pending.distinct_keys(), 1);
        assert_eq!(
            pending.iter().last(),
            Some(&Change::upsert("a", 4)),
            "the last pending change carries the final value"
        );
        // Coalescing the producer buffer must not disturb the ring's fidelity.
        assert_eq!(store.history_len(), 4);
        let replayed = store.patch_since(0).expect("nothing dropped").into_patch();
        assert_eq!(replayed, Patch::single(Change::upsert("a", 4)));
    }

    #[test]
    fn history_capacity_is_respected_and_the_ring_keeps_a_suffix() {
        let mut store = Store::with_history_capacity(2);
        for index in 0..5 {
            store.insert(format!("k{index}"), index);
        }
        assert_eq!(store.history_len(), 2);
        assert_eq!(store.history_capacity(), 2);
        assert_eq!(store.retained_range(), Some(4..=5));
    }

    #[test]
    fn zero_history_capacity_retains_nothing_and_reports_the_whole_gap() {
        let mut store = Store::with_history_capacity(0);
        store.insert("a", 1);
        store.insert("b", 2);
        assert_eq!(store.history_len(), 0);
        assert_eq!(store.retained_range(), None);
        assert_eq!(store.dropped_since(0), Some(1..=2));

        let err = store
            .patch_since(0)
            .expect_err("no history means no catch-up");
        assert!(
            matches!(err.kind(), ErrorKind::Conflict { .. }),
            "expected Conflict, got {:?}",
            err.kind()
        );
        assert_eq!(err.operation_name(), "state::store::patch_since");
    }

    #[test]
    fn dropped_since_names_exactly_the_evicted_revisions() {
        let mut store = Store::with_history_capacity(2);
        for index in 0..5 {
            store.insert(format!("k{index}"), index);
        }
        // The ring holds revisions 4 and 5. A caller at revision 3 is directly behind
        // them and catches up fine; anything older has lost 1..=3 and cannot.
        assert_eq!(store.dropped_since(0), Some(1..=3));
        assert_eq!(store.dropped_since(1), Some(2..=3));
        assert_eq!(store.dropped_since(2), Some(3..=3));
        assert_eq!(store.dropped_since(3), None, "3 is directly behind 4");
        assert_eq!(store.dropped_since(4), None);
        assert_eq!(store.dropped_since(5), None, "caught up");
    }

    #[test]
    fn patch_since_rejects_a_revision_from_the_future() {
        let mut store = Store::new();
        store.insert("a", 1);
        let err = store
            .patch_since(99)
            .expect_err("future revision is a caller bug");
        assert!(
            matches!(err.kind(), ErrorKind::Validation { .. }),
            "expected Validation, got {:?}",
            err.kind()
        );
    }

    #[test]
    fn patch_since_replays_every_retained_revision_as_one_coalesced_patch() {
        let mut store = Store::new();
        store.insert("a", 1);
        store.insert("b", 2);
        store.insert("a", 3);
        store.remove("b");

        let delivery = store.patch_since(0).expect("nothing dropped");
        assert_eq!(delivery.from_revision(), 0);
        assert_eq!(delivery.to_revision(), 4);
        assert_eq!(
            delivery.patch(),
            &Patch::from(vec![Change::upsert("a", 3), Change::remove("b")])
        );
    }

    #[test]
    fn patch_since_answers_empty_when_the_caller_is_current() {
        let mut store = Store::new();
        store.insert("a", 1);
        let delivery = store.patch_since(1).expect("nothing dropped");
        assert!(delivery.is_empty());
        assert_eq!(delivery.from_revision(), 1);
        assert_eq!(delivery.to_revision(), 1);
    }

    #[test]
    fn patch_since_of_an_empty_store_is_an_empty_patch_at_revision_zero() {
        let store = Store::<i32>::new();
        let delivery = store.patch_since(0).expect("nothing dropped");
        assert!(delivery.is_empty());
        assert_eq!(delivery.to_revision(), 0);
    }

    #[test]
    fn apply_to_applies_upserts_and_removes_and_advances_one_revision() {
        let mut store = Store::new();
        store.insert("a", 1);
        store.insert("b", 2);
        let before = store.revision();

        let patch = Patch::from(vec![
            Change::upsert("a", 10),
            Change::upsert("c", 30),
            Change::remove("b"),
        ]);
        let after = patch.apply_to(&mut store);

        assert_eq!(
            after,
            before + 1,
            "a patch is one revision, not one per change"
        );
        assert_eq!(store.revision(), after);
        assert_eq!(
            contents(&store),
            vec![("a".to_owned(), 10), ("c".to_owned(), 30)]
        );
    }

    #[test]
    fn apply_to_is_total_even_when_removing_an_absent_key() {
        let mut store = Store::<i32>::new();
        let before = store.revision();
        let patch = Patch::single(Change::remove("never-existed"));
        let after = patch.apply_to(&mut store);
        assert!(store.is_empty());
        assert_eq!(
            after,
            before + 1,
            "replay records the patch it applied even when the patch was a no-op"
        );

        // An empty patch, by contrast, advances nothing at all.
        assert_eq!(
            Patch::<i32>::empty().apply_to(&mut store),
            after,
            "an empty patch advances nothing"
        );
    }

    #[test]
    fn apply_to_is_idempotent_when_replayed() {
        let mut store = Store::new();
        let patch = Patch::from(vec![Change::upsert("a", 1), Change::upsert("b", 2)]);
        patch.apply_to(&mut store);
        let first = contents(&store);
        patch.apply_to(&mut store);
        assert_eq!(contents(&store), first, "replaying must converge");
    }

    #[test]
    fn a_patch_round_trips_through_serde_and_still_rebuilds_the_store() {
        let mut source = Store::new();
        for index in 0..5 {
            source.insert(format!("k{index}"), index * 10);
        }
        source.remove("k1");
        let patch = source.patch_since(0).expect("nothing dropped").into_patch();

        let json = serde_json::to_string(&patch).expect("serializable");
        let decoded: Patch<i32> = serde_json::from_str(&json).expect("valid patch");

        let mut rebuilt = Store::new();
        decoded.apply_to(&mut rebuilt);
        assert_eq!(contents(&rebuilt), contents(&source));
    }

    #[test]
    fn subscribing_borrows_the_store_and_sees_its_revisions() {
        let store = Store::<i32>::new();
        let subscription = store.subscribe(SubscriptionFilter::all());
        assert!(!subscription.resync_required());
        assert_eq!(subscription.cursor_hint(), 0);
    }

    #[test]
    fn keys_are_plain_strings_so_prefixes_are_meaningful() {
        let mut store = Store::new();
        let key: StateKey = "win/1".to_owned();
        store.insert(key.clone(), 1);
        assert!(key.starts_with("win/"));
        assert_eq!(store.get(&key), Some(&1));
    }

    /// A deterministic `SplitMix64`-style source so the property test below explores a
    /// different region of the input space on every run of the suite without pulling in a
    /// property-testing dependency for one test.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    /// The property: coalescing is invisible at the storage layer.
    ///
    /// `apply(patch)` and `apply(coalesce(patch))` must leave a store with identical
    /// contents, for every patch. This is the load-bearing claim behind every coalescing
    /// call site in the crate — subscriptions coalesce before delivering, the pending
    /// buffer coalesces under pressure, and neither is allowed to change what the state
    /// actually is. A bug here would be invisible in a unit test of `coalesce` alone,
    /// because that test can only check the patch, not its effect.
    #[test]
    fn coalescing_a_patch_does_not_change_the_resulting_store() {
        const KEYS: [&str; 5] = ["a", "b", "c", "win/1", "win/2"];

        for seed in 1..=400_u64 {
            let mut rng = Lcg(seed);

            // A non-empty starting state, so removals of absent keys and removals of
            // present keys are both exercised.
            let mut start = Store::new();
            for key in KEYS {
                if rng.below(3) == 0 {
                    start.insert(key, i32::try_from(rng.below(1000)).unwrap_or(0));
                }
            }

            let changes: Vec<Change<i32>> = (0..=rng.below(24))
                .map(|_| {
                    let key = KEYS[rng.below(KEYS.len() as u64) as usize];
                    if rng.below(4) == 0 {
                        Change::remove(key)
                    } else {
                        Change::upsert(key, i32::try_from(rng.below(1000)).unwrap_or(0))
                    }
                })
                .collect();
            let patch: Patch<i32> = changes.into_iter().collect();

            let mut applied_raw = start.clone();
            patch.apply_to(&mut applied_raw);

            let mut applied_coalesced = start;
            patch.coalesced().apply_to(&mut applied_coalesced);

            assert_eq!(
                contents(&applied_raw),
                contents(&applied_coalesced),
                "seed {seed}: coalescing changed the resulting store for {patch:?}"
            );
        }
    }

    /// Companion property: coalescing is idempotent on random patches, which is what lets
    /// a patch be coalesced at every hop without the result drifting.
    #[test]
    fn coalescing_a_patch_twice_equals_coalescing_it_once_on_random_input() {
        const KEYS: [&str; 4] = ["a", "b", "win/1", "win/2"];

        for seed in 1..=400_u64 {
            let mut rng = Lcg(seed.wrapping_mul(0x5DEE_CE66));
            let changes: Vec<Change<i32>> = (0..rng.below(30))
                .map(|_| {
                    let key = KEYS[rng.below(KEYS.len() as u64) as usize];
                    if rng.below(3) == 0 {
                        Change::remove(key)
                    } else {
                        Change::upsert(key, i32::try_from(rng.below(100)).unwrap_or(0))
                    }
                })
                .collect();
            let mut once = Patch::from(changes);
            once.coalesce();
            let mut twice = once.clone();
            twice.coalesce();
            assert_eq!(once, twice, "seed {seed}: coalescing is not idempotent");
            assert_eq!(
                once.len(),
                once.distinct_keys(),
                "seed {seed}: a coalesced patch holds exactly one change per distinct key"
            );
        }
    }
}
