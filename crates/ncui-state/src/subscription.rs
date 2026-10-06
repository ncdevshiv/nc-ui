//! Filtered views over a store's patch stream, and the catch-up protocol on top of them.
//!
//! A [`Subscription`] is a *cursor*, not a participant. It holds a borrowed reference to
//! a [`Store`] and a [`SubscriptionFilter`], and it registers nothing anywhere. That is a
//! deliberate architectural choice: registration would mean a wake-up queue, a
//! per-subscriber lock and an unsubscribe path that leaks if it is forgotten — machinery
//! whose only purpose is to deliver a delta the subscriber can equally well pull when it
//! is ready. Polling is how the rest of the runtime already works (spec 15: a mutation
//! must not re-render the application), so a subscription is a pull.
//!
//! # The catch-up contract
//!
//! [`Subscription::next_patch`] takes the revision the subscriber was current as of and
//! returns `Some` with everything newer, coalesced into one patch. Three outcomes, and
//! they are distinguishable without a side channel:
//!
//! | outcome | meaning | client action |
//! |---|---|---|
//! | `None`, [`Subscription::resync_required`] is `false` | caught up, nothing new | do nothing |
//! | `Some(delivery)` with an empty patch | time passed, the filter matched nothing | advance cursor to `to_revision` |
//! | `None`, [`Subscription::resync_required`] is `true` | changes were evicted; there is a gap | resynchronise from a full read |
//!
//! The second row is the reason the range travels with the patch. Without it, "no changes
//! you care about" and "no changes at all" would be indistinguishable, and a filtered
//! subscriber would either re-poll forever or advance its cursor by guessing.
//!
//! # The gap invariant
//!
//! The one thing an incremental protocol must never do is hand a consumer a patch with a
//! silent hole in it. The consumer would render state that is neither the old version nor
//! the new one, and nothing anywhere would report an error. So when a subscriber's
//! cursor is older than the ring, [`Subscription::next_patch`] refuses, sets
//! [`Subscription::resync_required`], and records the exact lost range in
//! [`Subscription::dropped_range`]. The flag is sticky — it stays set until
//! [`Subscription::clear_resync`] — so a client that polls in a loop cannot accidentally
//! walk past the gap into a false "all clear".

use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use ncui_core::{ErrorKind, Result};

use crate::patch::{PatchDelivery, StateKey};
use crate::store::Store;

/// Which keys a [`Subscription`] wants to hear about.
///
/// The three shapes map onto the three ways a UI actually partitions its state: the whole
/// tree, one explicitly enumerated set of nodes, or everything under a namespace.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SubscriptionFilter {
    /// Every change, whatever its key.
    #[default]
    All,
    /// Only the listed keys. Order in the input is irrelevant — the set is sorted, so the
    /// filter has one canonical representation and can be compared and logged.
    Keys(BTreeSet<StateKey>),
    /// Every key starting with this string.
    ///
    /// This is a **raw string prefix**, not a path-segment prefix: `win` matches both
    /// `win/1` and `windowpane/1`. The alternative would mean inventing a segment model
    /// (what is the separator? what is an escaping rule? what happens at the root?) that
    /// this crate has no business deciding — it knows nothing about what keys mean. A
    /// caller who needs segment semantics composes the key space accordingly, usually by
    /// including the separator in the prefix (`"win/"`).
    ///
    /// An empty prefix therefore matches every key, exactly like [`Self::All`].
    Prefix(StateKey),
}

impl SubscriptionFilter {
    /// A filter that matches every key. Equivalent to [`SubscriptionFilter::All`].
    #[must_use]
    pub const fn all() -> Self {
        Self::All
    }

    /// A filter matching exactly the given keys. Duplicates collapse.
    #[must_use]
    pub fn keys<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<StateKey>,
    {
        Self::Keys(keys.into_iter().map(Into::into).collect())
    }

    /// A filter matching every key starting with `prefix`.
    #[must_use]
    pub fn prefix(prefix: impl Into<StateKey>) -> Self {
        Self::Prefix(prefix.into())
    }

    /// Whether this filter selects `key`.
    ///
    /// Public because a caller often wants to ask the same question about a key it is
    /// about to insert into a [`Patch`](crate::Patch) itself, and having two
    /// implementations of that predicate would be a defect waiting to happen.
    #[must_use]
    pub fn matches(&self, key: &str) -> bool {
        match self {
            Self::All => true,
            Self::Keys(keys) => keys.contains(key),
            Self::Prefix(prefix) => key.starts_with(prefix.as_str()),
        }
    }
}

/// A filtered, resumable view over a [`Store`]'s patch stream.
///
/// Borrowing the store rather than owning a copy of it is what makes a subscription free
/// to create and impossible to forget to tear down; the cost is that the store cannot be
/// mutated while a subscription is alive, which is the right constraint — a live
/// subscription *is* an assertion that the consumer is reading.
#[derive(Debug)]
pub struct Subscription<'a, T> {
    store: &'a Store<T>,
    filter: SubscriptionFilter,
    resync_required: bool,
    dropped: Option<RangeInclusive<u64>>,
    cursor_hint: u64,
}

impl<'a, T> Subscription<'a, T> {
    /// Opens a subscription over `store`.
    ///
    /// Equivalent to [`Store::subscribe`]; provided so a caller holding a borrowed store
    /// can be explicit about the borrow.
    #[must_use]
    pub fn new(store: &'a Store<T>, filter: SubscriptionFilter) -> Self {
        Self {
            store,
            filter,
            resync_required: false,
            dropped: None,
            cursor_hint: store.revision(),
        }
    }

    /// The filter this subscription was opened with.
    #[must_use]
    pub const fn filter(&self) -> &SubscriptionFilter {
        &self.filter
    }

    /// The store's revision at the moment the subscription was opened.
    ///
    /// A convenience starting point for a subscriber that has just attached: it can call
    /// [`Subscription::next_patch`] with this value to learn about everything that
    /// happens from now on, and take the initial state separately.
    #[must_use]
    pub const fn cursor_hint(&self) -> u64 {
        self.cursor_hint
    }

    /// The store's current revision, whether or not it has advanced since the last poll.
    #[must_use]
    pub fn store_revision(&self) -> u64 {
        self.store.revision()
    }

    /// Whether this subscriber has fallen too far behind and must resynchronise.
    ///
    /// Sticky: once set it stays set until [`Subscription::clear_resync`]. While it is
    /// set, [`Subscription::next_patch`] returns `None` and delivers nothing, so a
    /// client that polls in a loop cannot step over the gap.
    #[must_use]
    pub const fn resync_required(&self) -> bool {
        self.resync_required
    }

    /// The revisions this subscriber lost and can no longer catch up from, if any.
    ///
    /// Only meaningful once [`Subscription::resync_required`] is `true`. A range is
    /// reported rather than a bare flag so a client can log exactly how far behind it
    /// fell, which is the difference between "occasionally resyncs" and "resyncs on every
    /// frame because the workload is too fast for the ring".
    #[must_use]
    pub fn dropped_range(&self) -> Option<RangeInclusive<u64>> {
        self.dropped.clone()
    }

    /// Clears the resync requirement and any recorded dropped range.
    ///
    /// Call this only after a genuine full resynchronisation, from which the caller
    /// restarts at [`Store::revision`]. Clearing it without having resynchronised
    /// re-opens a gap that the ring cannot close, and the client will silently render
    /// stale state.
    pub fn clear_resync(&mut self) {
        self.resync_required = false;
        self.dropped = None;
    }

    /// Returns everything newer than `since_revision`, or `None` when caught up.
    ///
    /// See the module documentation for the full outcome table. In short: `None` means
    /// either "caught up" or "resynchronise, see [`Subscription::resync_required`]",
    /// and those two are told apart by that flag — never by the return value alone.
    ///
    /// Any underlying failure to produce a delta is treated as "cannot continue
    /// incrementally" and raises the resync flag, because the alternative — returning a
    /// patch that might have a hole in it — is the one outcome an incremental protocol
    /// must never produce. Use [`Subscription::try_next_patch`] to learn *why*.
    pub fn next_patch(&mut self, since_revision: u64) -> Option<PatchDelivery<T>>
    where
        T: Clone,
    {
        match self.try_next_patch(since_revision) {
            Ok(delivery) => delivery,
            Err(_) => {
                self.resync_required = true;
                None
            }
        }
    }

    /// The fallible form of [`Subscription::next_patch`].
    ///
    /// Returns `Ok(None)` when the subscriber is caught up, or when
    /// [`Subscription::resync_required`] is already set. Returns `Err` with
    /// [`ErrorKind::Conflict`] when revisions were evicted — in which case the dropped
    /// range has been recorded — or with [`ErrorKind::Validation`] when `since_revision`
    /// is ahead of the store, which is a caller bug and leaves the resync flag alone.
    pub fn try_next_patch(&mut self, since_revision: u64) -> Result<Option<PatchDelivery<T>>>
    where
        T: Clone,
    {
        if self.resync_required {
            return Ok(None);
        }
        match self.store.patch_since(since_revision) {
            Ok(delivery) => {
                let from_revision = delivery.from_revision();
                let to_revision = delivery.to_revision();
                if from_revision == to_revision {
                    return Ok(None);
                }
                let mut patch = delivery.into_patch();
                patch.retain(|change| self.filter.matches(change.key()));
                Ok(Some(PatchDelivery::new(from_revision, to_revision, patch)))
            }
            Err(error) => {
                if matches!(error.kind(), ErrorKind::Conflict { .. }) {
                    self.dropped = self.store.dropped_since(since_revision);
                    self.resync_required = true;
                }
                Err(error)
            }
        }
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
    use crate::patch::{Change, Patch};
    use ncui_core::error::validation;

    fn keys_of(delivery: &PatchDelivery<i32>) -> Vec<String> {
        delivery
            .patch()
            .iter()
            .map(|change| change.key().to_owned())
            .collect()
    }

    fn store_with(revisions: &[(&str, i32)]) -> Store<i32> {
        let mut store = Store::new();
        for (key, value) in revisions {
            store.insert(*key, *value);
        }
        store
    }

    #[test]
    fn the_all_filter_matches_every_key() {
        let filter = SubscriptionFilter::all();
        assert!(filter.matches(""));
        assert!(filter.matches("win/1"));
        assert!(filter.matches("anything at all"));
        assert_eq!(filter, SubscriptionFilter::default());
    }

    #[test]
    fn a_key_set_filter_matches_only_its_members() {
        let filter = SubscriptionFilter::keys(["win/1", "log/2"]);
        assert!(filter.matches("win/1"));
        assert!(filter.matches("log/2"));
        assert!(!filter.matches("win/2"));
        assert!(!filter.matches("log/1"));
        assert!(!filter.matches(""));
        assert_eq!(
            filter,
            SubscriptionFilter::Keys(BTreeSet::from(["log/2".to_owned(), "win/1".to_owned()]))
        );
    }

    #[test]
    fn a_key_set_filter_deduplicates_its_input() {
        let filter = SubscriptionFilter::keys(["a", "a", "b"]);
        assert_eq!(filter, SubscriptionFilter::keys(["a", "b"]));
    }

    #[test]
    fn a_prefix_filter_matches_everything_under_the_prefix() {
        let filter = SubscriptionFilter::prefix("win/");
        assert!(filter.matches("win/1"));
        assert!(filter.matches("win/1/panel"));
        assert!(filter.matches("win/"));
        assert!(!filter.matches("window/1"));
        assert!(!filter.matches("log/1"));
        assert!(!filter.matches("awin/1"));
    }

    #[test]
    fn a_prefix_filter_is_a_raw_string_prefix_not_a_path_segment_prefix() {
        let filter = SubscriptionFilter::prefix("win");
        assert!(
            filter.matches("windowpane/1"),
            "the prefix relation is documented as raw, not segment-aware"
        );
        assert!(filter.matches("win/1"));
    }

    #[test]
    fn an_empty_prefix_matches_every_key_just_like_all() {
        let filter = SubscriptionFilter::prefix("");
        for key in ["", "a", "win/1"] {
            assert!(filter.matches(key));
        }
    }

    #[test]
    fn a_caught_up_subscription_returns_nothing() {
        let store = store_with(&[("a", 1)]);
        let mut subscription = store.subscribe(SubscriptionFilter::all());
        assert!(subscription.next_patch(1).is_none());
        assert!(!subscription.resync_required());
        assert_eq!(subscription.store_revision(), 1);
    }

    #[test]
    fn a_subscription_against_an_untouched_store_returns_nothing() {
        let store = Store::<i32>::new();
        let mut subscription = store.subscribe(SubscriptionFilter::all());
        assert!(subscription.next_patch(0).is_none());
        assert!(!subscription.resync_required());
    }

    #[test]
    fn next_patch_delivers_only_changes_newer_than_the_cursor() {
        let store = store_with(&[("a", 1), ("b", 2), ("c", 3)]);
        let mut subscription = store.subscribe(SubscriptionFilter::all());

        let delivery = subscription
            .next_patch(1)
            .expect("changes 2 and 3 are pending");
        assert_eq!(delivery.from_revision(), 1);
        assert_eq!(delivery.to_revision(), 3);
        assert_eq!(keys_of(&delivery), vec!["b".to_owned(), "c".to_owned()]);
    }

    #[test]
    fn next_patch_catches_a_subscriber_up_from_an_older_revision_in_one_patch() {
        let store = store_with(&[("a", 1), ("b", 2), ("a", 3), ("c", 4), ("b", 5)]);
        let mut subscription = store.subscribe(SubscriptionFilter::all());

        let delivery = subscription.next_patch(0).expect("five revisions pending");
        assert_eq!(delivery.from_revision(), 0);
        assert_eq!(delivery.to_revision(), 5);
        assert_eq!(
            delivery.patch(),
            &Patch::from(vec![
                Change::upsert("a", 3),
                Change::upsert("b", 5),
                Change::upsert("c", 4)
            ]),
            "catch-up is coalesced to one change per key"
        );
    }

    #[test]
    fn a_prefix_subscription_delivers_only_its_own_subtree() {
        let store = store_with(&[
            ("win/1", 1),
            ("log/1", 2),
            ("win/2", 3),
            ("log/2", 4),
            ("win/1", 5),
        ]);
        let mut subscription = store.subscribe(SubscriptionFilter::prefix("win/"));

        let delivery = subscription.next_patch(0).expect("pending");
        assert_eq!(
            keys_of(&delivery),
            vec!["win/1".to_owned(), "win/2".to_owned()]
        );
        assert_eq!(
            delivery.patch().iter().next(),
            Some(&Change::upsert("win/1", 5)),
            "the surviving value is the latest, not the first"
        );
        assert_eq!(delivery.to_revision(), 5, "the cursor still advances fully");
    }

    #[test]
    fn a_key_set_subscription_delivers_only_its_keys() {
        let store = store_with(&[("a", 1), ("b", 2), ("c", 3)]);
        let mut subscription = store.subscribe(SubscriptionFilter::keys(["a", "c"]));

        let delivery = subscription.next_patch(0).expect("pending");
        assert_eq!(keys_of(&delivery), vec!["a".to_owned(), "c".to_owned()]);
    }

    #[test]
    fn a_filter_that_excludes_everything_yields_an_empty_patch_over_a_real_range() {
        let store = store_with(&[("log/1", 1), ("log/2", 2)]);
        let mut subscription = store.subscribe(SubscriptionFilter::prefix("win/"));

        let delivery = subscription
            .next_patch(0)
            .expect("time passed even though nothing matched");
        assert!(delivery.is_empty());
        assert_eq!(delivery.from_revision(), 0);
        assert_eq!(delivery.to_revision(), 2);
    }

    #[test]
    fn filtering_and_coalescing_commute_because_both_are_per_key() {
        let store = store_with(&[("win/1", 1), ("log/1", 2), ("win/1", 3), ("win/2", 4)]);
        let filter = SubscriptionFilter::prefix("win/");

        let all = store.patch_since(0).expect("nothing dropped").into_patch();
        let mut filter_then_coalesce = all.clone();
        filter_then_coalesce.retain(|change| filter.matches(change.key()));
        filter_then_coalesce.coalesce();

        let mut coalesce_then_filter = all;
        coalesce_then_filter.coalesce();
        coalesce_then_filter.retain(|change| filter.matches(change.key()));

        assert_eq!(filter_then_coalesce, coalesce_then_filter);
    }

    #[test]
    fn a_slow_subscriber_that_falls_off_the_ring_is_told_to_resync() {
        let mut store = Store::with_history_capacity(2);
        for index in 0..5 {
            store.insert(format!("k{index}"), index);
        }

        let mut subscription = store.subscribe(SubscriptionFilter::all());
        assert!(!subscription.resync_required());

        let delivery = subscription.next_patch(0);
        assert!(delivery.is_none());
        assert!(
            subscription.resync_required(),
            "a subscriber behind the ring must be told, not silently shorted"
        );
    }

    #[test]
    fn the_dropped_range_names_exactly_the_lost_revisions() {
        let mut store = Store::with_history_capacity(2);
        for index in 0..5 {
            store.insert(format!("k{index}"), index);
        }
        let mut subscription = store.subscribe(SubscriptionFilter::all());
        subscription.next_patch(0);
        assert_eq!(subscription.dropped_range(), Some(1..=3));
    }

    #[test]
    fn a_subscriber_that_fell_behind_but_is_still_within_the_ring_catches_up_normally() {
        let mut store = Store::with_history_capacity(4);
        for index in 0..4 {
            store.insert(format!("k{index}"), index);
        }
        let mut subscription = store.subscribe(SubscriptionFilter::all());
        let delivery = subscription
            .next_patch(2)
            .expect("revisions 3 and 4 are retained");
        assert_eq!(keys_of(&delivery), vec!["k2".to_owned(), "k3".to_owned()]);
        assert!(!subscription.resync_required());
    }

    #[test]
    fn the_resync_flag_is_sticky_until_cleared_and_blocks_further_deltas() {
        let mut store = Store::with_history_capacity(2);
        for index in 0..5 {
            store.insert(format!("k{index}"), index);
        }
        let mut subscription = store.subscribe(SubscriptionFilter::all());

        assert!(subscription.next_patch(0).is_none());
        assert!(subscription.resync_required());

        // Even after the store has moved on, a subscriber flagged for resync gets nothing:
        // delivering now would hand it a patch that starts in the middle of a gap.
        assert!(subscription.next_patch(4).is_none());
        assert!(subscription.next_patch(store.revision()).is_none());
        assert!(subscription.resync_required());

        subscription.clear_resync();
        assert!(!subscription.resync_required());
        assert_eq!(subscription.dropped_range(), None);

        let delivery = subscription.next_patch(3).expect("now answerable");
        assert_eq!(keys_of(&delivery), vec!["k3".to_owned(), "k4".to_owned()]);
        assert_eq!(delivery.from_revision(), 3);
        assert_eq!(delivery.to_revision(), 5);
    }

    #[test]
    fn try_next_patch_surfaces_the_conflict_and_records_the_dropped_range() {
        let mut store = Store::with_history_capacity(2);
        for index in 0..5 {
            store.insert(format!("k{index}"), index);
        }
        let mut subscription = store.subscribe(SubscriptionFilter::all());

        let error = subscription
            .try_next_patch(0)
            .expect_err("evicted revisions must be reported");
        assert!(
            matches!(error.kind(), ErrorKind::Conflict { .. }),
            "expected Conflict, got {:?}",
            error.kind()
        );
        assert!(
            error.to_string().contains("1-3"),
            "reason names the gap: {error}"
        );
        assert!(subscription.resync_required());
        assert_eq!(subscription.dropped_range(), Some(1..=3));
    }

    #[test]
    fn try_next_patch_surfaces_a_validation_error_without_raising_the_resync_flag() {
        let store = store_with(&[("a", 1), ("b", 2)]);
        let mut subscription = store.subscribe(SubscriptionFilter::all());

        let error = subscription
            .try_next_patch(99)
            .expect_err("a cursor ahead of the store is a caller bug");
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "expected Validation, got {:?}",
            error.kind()
        );
        assert_eq!(error.operation_name(), "state::store::patch_since");
        assert!(
            !subscription.resync_required(),
            "a bad cursor is not a gap; the caller can fix it without a resync"
        );

        // And the same subscription still works once the cursor is corrected.
        let delivery = subscription.next_patch(1).expect("revision 2 is pending");
        assert_eq!(keys_of(&delivery), vec!["b".to_owned()]);
        assert!(subscription.next_patch(2).is_none(), "now caught up");
    }

    #[test]
    fn next_patch_recovers_from_a_bad_cursor_by_requiring_a_resync() {
        let store = store_with(&[("a", 1)]);
        let mut subscription = store.subscribe(SubscriptionFilter::all());
        assert!(subscription.next_patch(99).is_none());
        assert!(
            subscription.resync_required(),
            "the infallible wrapper must not let an unanswerable cursor through"
        );
    }

    #[test]
    fn a_subscription_reports_the_filter_it_was_opened_with() {
        let store = Store::<i32>::new();
        let filter = SubscriptionFilter::prefix("win/");
        let subscription = store.subscribe(filter.clone());
        assert_eq!(subscription.filter(), &filter);
        assert_eq!(subscription.cursor_hint(), 0);
    }

    #[test]
    fn cursor_hint_is_the_store_revision_at_subscription_time() {
        let store = store_with(&[("a", 1), ("b", 2)]);
        let subscription = store.subscribe(SubscriptionFilter::all());
        assert_eq!(subscription.cursor_hint(), 2);
    }

    #[test]
    fn a_removal_is_delivered_to_a_subscriber_that_filtered_in_the_key() {
        let store = store_with(&[("win/1", 1), ("log/1", 2)]);
        let mut removed = store.clone();
        removed.remove("win/1");

        let mut subscription = removed.subscribe(SubscriptionFilter::prefix("win/"));
        let delivery = subscription.next_patch(2).expect("the removal is pending");
        assert_eq!(delivery.patch(), &Patch::single(Change::remove("win/1")));
    }

    #[test]
    fn a_filtered_patch_applies_to_a_mirror_store_and_reaches_the_same_contents() {
        let source = store_with(&[("win/1", 1), ("log/1", 2), ("win/2", 3), ("win/1", 4)]);
        let mut subscription = source.subscribe(SubscriptionFilter::prefix("win/"));
        let delivery = subscription.next_patch(0).expect("pending");

        let mut mirror = Store::new();
        delivery.into_patch().apply_to(&mut mirror);
        assert_eq!(
            mirror.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            vec!["win/1", "win/2"]
        );
        assert_eq!(mirror.get("win/1"), Some(&4));
    }

    #[test]
    fn an_error_without_an_operation_string_is_still_classifiable() {
        // Guards the classification contract the subscription relies on: it branches on
        // `ErrorKind`, so the two failure modes it distinguishes must stay distinct.
        let conflict = ncui_core::NcError::new(ErrorKind::Conflict {
            reason: "gap".into(),
        });
        let bad = validation("since_revision", "ahead");
        assert!(matches!(conflict.kind(), ErrorKind::Conflict { .. }));
        assert!(matches!(bad.kind(), ErrorKind::Validation { .. }));
        assert!(!matches!(bad.kind(), ErrorKind::Conflict { .. }));
    }
}
