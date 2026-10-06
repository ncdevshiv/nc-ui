//! Change patches: the wire and in-process unit of incremental state transfer.
//!
//! A [`Patch`] is an ordered list of [`Change`]s against a [`crate::Store`]. It is the
//! only way state ever moves between a producer and a consumer in this crate, and it
//! exists so that neither side ever has to resynchronise a whole tree (spec 15, 18).
//!
//! The three types here form a deliberate ladder:
//!
//! * [`Change`] — one key's worth of intent: put this value at this key, or take it out.
//! * [`Patch`] — an ordered, coalescable run of changes covering a revision range.
//! * [`PatchDelivery`] — a [`Patch`] plus the revision range it is known to cover, so a
//!   consumer can advance its cursor without guessing.
//!
//! # Invariants
//!
//! 1. **Order is significant, position is not.** Two patches that mention the same set of
//!    keys and end in the same per-key outcome produce the same store contents, whatever
//!    their internal order — as long as both are coalesced. [`Patch::coalesce`] is what
//!    makes this true, and it is order-*stable*: the surviving change for a key keeps the
//!    position of that key's first appearance and the *value* of its last appearance.
//! 2. **Removal is not an error.** Applying [`Change::Remove`] for an absent key is a
//!    no-op. This is what makes replay convergent: a consumer that fell behind and is
//!    handed a catch-up patch must reach the producer's state regardless of which
//!    versions of which keys it happened to already hold.
//! 3. **A patch is the unit of revision advancement.** Applying an *n*-change patch
//!    advances a store by exactly one revision, not *n*. Batch construction is therefore
//!    atomic from the outside, which is what lets a renderer treat a burst of mutations
//!    as a single frame of work.
//! 4. **A delivery is honest about its range.** [`PatchDelivery::from_revision`] is
//!    exclusive and [`PatchDelivery::to_revision`] is inclusive, so `from < to` whenever
//!    the delivery is non-empty, and a consumer may always set its cursor to `to`.

use serde::{Deserialize, Serialize};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use crate::store::Store;

/// The type used to identify one record inside a [`Store`].
///
/// Keys are UTF-8 strings rather than one of the `ncui-core` identifier newtypes. The
/// deciding factor is the prefix filter in [`crate::SubscriptionFilter`]: a subscription
/// that must be able to say "everything under `win/`" needs keys with a textual
/// hierarchy, and the runtime's id newtypes are opaque 128-bit hex strings with no
/// relation to each other. A UI state store is a tree of panels, windows and bindings,
/// and a stable, human-readable, hierarchically related key is what a tree-shaped thing
/// actually needs.
///
/// The cost is that key equality is a string comparison rather than a 128-bit compare.
/// That is irrelevant next to the `O(log n)` map lookup it accompanies, and it buys the
/// prefix relation for free.
pub type StateKey = String;

/// A single key-scoped change against a [`Store`].
///
/// Serialised in serde's internally-tagged form, so on the wire a change is one JSON
/// object rather than a nested enum — `{"op":"upsert","key":"win/1","value":42}`. The
/// tag keeps the encoding readable and forward-compatible: a consumer that does not know
/// an `op` can branch on the string, and adding a variant does not change the shape of
/// the existing ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Change<T> {
    /// Insert or replace the value stored at `key`.
    Upsert {
        /// The record key being written.
        key: StateKey,
        /// The new value. Replaces any previous value wholesale; this crate does not
        /// merge nested structures, because partial merge semantics belong to the
        /// domain layer that knows what a value means.
        value: T,
    },
    /// Delete the value stored at `key`, if any.
    Remove {
        /// The record key being deleted.
        key: StateKey,
    },
}

impl<T> Change<T> {
    /// Constructs an [`Change::Upsert`].
    ///
    /// Exists so callers do not have to spell the variant in full at every call site,
    /// and so the `key` conversion happens in one place.
    #[must_use]
    pub fn upsert(key: impl Into<StateKey>, value: T) -> Self {
        Self::Upsert {
            key: key.into(),
            value,
        }
    }

    /// Constructs a [`Change::Remove`].
    #[must_use]
    pub fn remove(key: impl Into<StateKey>) -> Self {
        Self::Remove { key: key.into() }
    }

    /// The key this change applies to, whether it writes or deletes.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Upsert { key, .. } | Self::Remove { key } => key,
        }
    }

    /// Whether this change deletes rather than writes.
    ///
    /// Callers that render deletions separately from writes (a virtualised list that
    /// removes a row, say) need this without destructuring the value they do not have.
    #[must_use]
    pub const fn is_remove(&self) -> bool {
        matches!(self, Self::Remove { .. })
    }

    /// The value being written, or `None` for a removal.
    #[must_use]
    pub const fn value(&self) -> Option<&T> {
        match self {
            Self::Upsert { value, .. } => Some(value),
            Self::Remove { .. } => None,
        }
    }

    /// Consumes the change, yielding the value being written or `None` for a removal.
    #[must_use]
    pub fn into_value(self) -> Option<T> {
        match self {
            Self::Upsert { value, .. } => Some(value),
            Self::Remove { .. } => None,
        }
    }
}

/// An ordered, coalescable run of [`Change`]s against a [`Store`].
///
/// The order is the order the changes were produced in; [`Patch::coalesce`] collapses it
/// without disturbing the relative order of the keys that survive. Serialises as a plain
/// JSON array, so a patch is a single value on the wire with no envelope of its own —
/// the revision range travels beside it in a [`PatchDelivery`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Patch<T> {
    changes: Vec<Change<T>>,
}

/// Implemented by hand rather than derived: `#[derive(Default)]` would demand `T: Default`,
/// and an empty patch is meaningful for any `T` — including a record type that has no
/// sensible zero value, which is most of them.
impl<T> Default for Patch<T> {
    fn default() -> Self {
        Self::empty()
    }
}

impl<T> Patch<T> {
    /// The empty patch. Serialises as `[]` and applies as a no-op.
    ///
    /// Preferred over constructing a `Patch` through a collection literal because the
    /// field is private: `Patch::empty()` is the only way to name the zero case without
    /// a type annotation, and the zero case shows up in every replay loop.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            changes: Vec::new(),
        }
    }

    /// A patch containing exactly one change.
    #[must_use]
    pub fn single(change: Change<T>) -> Self {
        Self {
            changes: vec![change],
        }
    }

    /// How many changes the patch holds, counting duplicates.
    ///
    /// This is the *uncoalesced* length. After [`Patch::coalesce`] the number of distinct
    /// keys touched is what matters, and [`Patch::distinct_keys`] reports it without
    /// consuming the patch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Whether the patch holds no changes at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Iterates the changes in order.
    pub fn iter(&self) -> std::slice::Iter<'_, Change<T>> {
        self.changes.iter()
    }

    /// Appends one change to the end of the patch.
    pub fn push(&mut self, change: Change<T>) {
        self.changes.push(change);
    }

    /// Appends every change of `other`, preserving both orders.
    ///
    /// Used to build a catch-up patch out of many retained single-change patches. No
    /// coalescing happens here: concatenation preserves the production order, and the
    /// caller decides when the accumulated run is worth collapsing.
    pub fn append(&mut self, other: Self) {
        self.changes.extend(other.changes);
    }

    /// Keeps only the changes for which `keep` returns `true`, preserving order.
    ///
    /// This is the primitive a [`crate::Subscription`] filters with. It is public because
    /// a renderer that has already pulled a patch wants to drop the subtrees it does not
    /// render without round-tripping through the store.
    pub fn retain(&mut self, mut keep: impl FnMut(&Change<T>) -> bool) {
        self.changes.retain(|change| keep(change));
    }

    /// Collapses repeated changes to the same key: **last write wins**.
    ///
    /// This is the operation that makes incremental update cheap. A producer that
    /// touches `win/1` fifty times between two frames delivers one change, not fifty, and
    /// the consumer's cost is proportional to the number of *distinct keys* that changed
    /// rather than to the number of mutations.
    ///
    /// It is **order-stable**: each surviving key occupies the slot of its first
    /// appearance, and carries the outcome of its last appearance. Two consequences
    /// follow, and both are tested:
    ///
    /// * Re-running [`Patch::coalesce`] is a no-op, so a patch may be coalesced at every
    ///   hop without the result drifting.
    /// * Coalescing commutes with applying: `coalesce` then `apply_to` and `apply_to`
    ///   then reading the store produce identical contents for any patch, because the
    ///   only thing that survives coalescing is each key's final state.
    ///
    /// Note the interaction with removals: an upsert followed by a removal of the same
    /// key collapses to a single removal. That is correct — the consumer ends without the
    /// key — and it is why a removal never needs the value it is undoing to be retained.
    pub fn coalesce(&mut self) {
        if self.changes.len() < 2 {
            return;
        }
        // A hash map keyed by the (cloned) key string, pointing at the slot in `kept`.
        // Cloning the key once per change is cheaper than any of the alternatives and is
        // bounded by the patch's own size; the values themselves are never copied.
        let mut position: HashMap<StateKey, usize> = HashMap::with_capacity(self.changes.len());
        let mut kept: Vec<Change<T>> = Vec::with_capacity(self.changes.len());
        for change in std::mem::take(&mut self.changes) {
            match position.entry(change.key().to_owned()) {
                Entry::Occupied(slot) => {
                    let index = *slot.get();
                    kept[index] = change;
                }
                Entry::Vacant(slot) => {
                    slot.insert(kept.len());
                    kept.push(change);
                }
            }
        }
        self.changes = kept;
    }

    /// Returns a coalesced copy, leaving `self` untouched.
    ///
    /// Convenience for the many call sites that own a patch by value and simply want the
    /// collapsed form; implemented by cloning rather than by consuming so callers can
    /// still log or forward the original. Requires `T: Clone` because it duplicates the
    /// patch; [`Patch::coalesce`] itself has no such bound, so a caller that already owns
    /// its patch never needs to pay for the copy.
    #[must_use]
    pub fn coalesced(&self) -> Self
    where
        T: Clone,
    {
        let mut copy = self.clone();
        copy.coalesce();
        copy
    }

    /// How many distinct keys the patch touches after coalescing.
    ///
    /// The honest cost metric for a consumer: this, not [`Patch::len`], is how much work
    /// the patch actually asks for.
    #[must_use]
    pub fn distinct_keys(&self) -> usize {
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.changes.len());
        for change in &self.changes {
            seen.insert(change.key());
        }
        seen.len()
    }

    /// Applies every change in order and returns the store's new revision.
    ///
    /// The whole patch advances the store by exactly **one** revision, whatever its
    /// length, so an observer sees the batch land atomically. This is why a consumer may
    /// safely coalesce patches without also having to reason about intermediate revisions
    /// it will never be shown.
    ///
    /// Applying a patch to a store that already reflects some or all of it is a no-op for
    /// the overlapping keys, which is what makes retry and replay safe.
    pub fn apply_to(&self, store: &mut Store<T>) -> u64
    where
        T: Clone,
    {
        if self.is_empty() {
            return store.revision();
        }
        for change in &self.changes {
            store.apply_change_in_place(change);
        }
        store.commit(self.clone());
        store.revision()
    }
}

impl<T> Default for PatchDelivery<T> {
    fn default() -> Self {
        Self::new(0, 0, Patch::empty())
    }
}

impl<T> From<Vec<Change<T>>> for Patch<T> {
    fn from(changes: Vec<Change<T>>) -> Self {
        Self { changes }
    }
}

impl<T> FromIterator<Change<T>> for Patch<T> {
    fn from_iter<I: IntoIterator<Item = Change<T>>>(iter: I) -> Self {
        Self {
            changes: Vec::from_iter(iter),
        }
    }
}

impl<T> Extend<Change<T>> for Patch<T> {
    fn extend<I: IntoIterator<Item = Change<T>>>(&mut self, iter: I) {
        self.changes.extend(iter);
    }
}

impl<'a, T> IntoIterator for &'a Patch<T> {
    type Item = &'a Change<T>;
    type IntoIter = std::slice::Iter<'a, Change<T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.changes.iter()
    }
}

impl<T> IntoIterator for Patch<T> {
    type Item = Change<T>;
    type IntoIter = std::vec::IntoIter<Change<T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.changes.into_iter()
    }
}

/// A [`Patch`] together with the revision range it is known to cover.
///
/// The range is what lets a consumer resume without a full resynchronisation: having
/// applied a delivery, it may set its cursor to [`PatchDelivery::to_revision`] and ask
/// only for what happened afterwards. Without the range, a coalesced patch would be
/// indistinguishable from a patch covering one revision, and the consumer could never
/// know whether it had caught up.
///
/// An **empty** patch with a non-empty range is a meaningful delivery: it says "time
/// passed, and the filter excluded everything that changed". See
/// [`crate::Subscription::next_patch`] for the contract that produces it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchDelivery<T> {
    from_revision: u64,
    to_revision: u64,
    patch: Patch<T>,
}

impl<T> PatchDelivery<T> {
    /// Builds a delivery covering `(from_revision, to_revision]`.
    ///
    /// Public because a store, a relay, or a test fixture may need to hand-craft one;
    /// `from_revision` is exclusive and `to_revision` inclusive.
    #[must_use]
    pub const fn new(from_revision: u64, to_revision: u64, patch: Patch<T>) -> Self {
        Self {
            from_revision,
            to_revision,
            patch,
        }
    }

    /// The exclusive lower bound: the revision the consumer was current as of.
    #[must_use]
    pub const fn from_revision(&self) -> u64 {
        self.from_revision
    }

    /// The inclusive upper bound: the revision the consumer may now advance to.
    #[must_use]
    pub const fn to_revision(&self) -> u64 {
        self.to_revision
    }

    /// The changes themselves.
    #[must_use]
    pub const fn patch(&self) -> &Patch<T> {
        &self.patch
    }

    /// How many changes the delivery carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.patch.len()
    }

    /// Whether the delivery carries no changes, which is distinct from covering no
    /// revisions: check [`PatchDelivery::from_revision`] against
    /// [`PatchDelivery::to_revision`] for that.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.patch.is_empty()
    }

    /// Consumes the delivery, yielding the bare patch and discarding the range.
    ///
    /// For callers that apply changes but track their cursor out of band.
    #[must_use]
    pub fn into_patch(self) -> Patch<T> {
        self.patch
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

    fn upsert(key: &str, value: i32) -> Change<i32> {
        Change::upsert(key, value)
    }

    #[test]
    fn empty_patch_has_no_changes_and_is_the_default() {
        let patch = Patch::<i32>::empty();
        assert!(patch.is_empty());
        assert_eq!(patch.len(), 0);
        assert_eq!(patch.iter().count(), 0);
        assert_eq!(patch, Patch::default());
        assert_eq!(serde_json::to_string(&patch).expect("serializable"), "[]");
    }

    #[test]
    fn single_and_from_iter_agree() {
        let built = Patch::single(upsert("a", 1));
        let collected: Patch<i32> = [upsert("a", 1)].into_iter().collect();
        assert_eq!(built, collected);
    }

    #[test]
    fn change_accessors_expose_key_and_value() {
        let write = upsert("win/1", 7);
        assert_eq!(write.key(), "win/1");
        assert!(!write.is_remove());
        assert_eq!(write.value(), Some(&7));
        assert_eq!(write.clone().into_value(), Some(7));

        let delete = Change::<i32>::remove("win/1");
        assert_eq!(delete.key(), "win/1");
        assert!(delete.is_remove());
        assert_eq!(delete.value(), None);
        assert_eq!(delete.into_value(), None);
    }

    #[test]
    fn coalesce_keeps_the_last_write_per_key() {
        let mut patch: Patch<i32> = [upsert("a", 1), upsert("a", 2), upsert("a", 3)]
            .into_iter()
            .collect();
        patch.coalesce();
        assert_eq!(patch.len(), 1);
        assert_eq!(patch.iter().next(), Some(&upsert("a", 3)));
    }

    #[test]
    fn coalesce_is_order_stable_by_first_appearance() {
        let mut patch: Patch<i32> = [
            upsert("a", 1),
            upsert("b", 1),
            upsert("a", 2),
            upsert("c", 1),
            upsert("b", 2),
        ]
        .into_iter()
        .collect();
        patch.coalesce();
        let keys: Vec<&str> = patch.iter().map(Change::key).collect();
        // First-appearance order, not last-appearance order: a, b, c.
        assert_eq!(keys, vec!["a", "b", "c"]);
        let values: Vec<i32> = patch
            .iter()
            .map(|c| c.value().copied().unwrap_or(-1))
            .collect();
        assert_eq!(values, vec![2, 2, 1]);
    }

    #[test]
    fn coalesce_upsert_then_remove_collapses_to_remove() {
        let mut patch: Patch<i32> = [upsert("a", 1), Change::remove("a")].into_iter().collect();
        patch.coalesce();
        assert_eq!(patch.len(), 1);
        assert!(patch.iter().next().unwrap().is_remove());
    }

    #[test]
    fn coalesce_remove_then_upsert_collapses_to_upsert() {
        let mut patch: Patch<i32> = [Change::remove("a"), upsert("a", 9)].into_iter().collect();
        patch.coalesce();
        assert_eq!(patch.len(), 1);
        assert_eq!(patch.iter().next(), Some(&upsert("a", 9)));
    }

    #[test]
    fn coalesce_is_idempotent() {
        let mut patch: Patch<i32> = [upsert("b", 1), upsert("a", 1), upsert("b", 2)]
            .into_iter()
            .collect();
        patch.coalesce();
        let once = patch.clone();
        patch.coalesce();
        assert_eq!(patch, once, "coalescing twice must equal coalescing once");
    }

    #[test]
    fn coalesce_leaves_short_and_empty_patches_alone() {
        let empty: Patch<i32> = Patch::empty();
        let mut empty = empty;
        empty.coalesce();
        assert!(empty.is_empty());

        let mut single = Patch::single(upsert("a", 1));
        single.coalesce();
        assert_eq!(single, Patch::single(upsert("a", 1)));
    }

    #[test]
    fn coalesced_is_non_mutating_and_matches_in_place_coalesce() {
        let patch: Patch<i32> = [upsert("a", 1), upsert("a", 2)].into_iter().collect();
        let mut in_place = patch.clone();
        in_place.coalesce();
        assert_eq!(patch.len(), 2, "coalesced() must not consume the original");
        assert_eq!(patch.coalesced(), in_place);
    }

    #[test]
    fn distinct_keys_counts_keys_not_changes() {
        let patch: Patch<i32> = [upsert("a", 1), upsert("b", 1), upsert("a", 2)]
            .into_iter()
            .collect();
        assert_eq!(patch.distinct_keys(), 2);
        assert_eq!(Patch::<i32>::empty().distinct_keys(), 0);
    }

    #[test]
    fn append_concatenates_in_order() {
        let mut first: Patch<i32> = [upsert("a", 1)].into_iter().collect();
        let second: Patch<i32> = [upsert("b", 2), upsert("c", 3)].into_iter().collect();
        first.append(second);
        let keys: Vec<&str> = first.iter().map(Change::key).collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn retain_preserves_order_of_survivors() {
        let mut patch: Patch<i32> = [upsert("win/1", 1), upsert("log/1", 2), upsert("win/2", 3)]
            .into_iter()
            .collect();
        patch.retain(|change| change.key().starts_with("win/"));
        let keys: Vec<&str> = patch.iter().map(Change::key).collect();
        assert_eq!(keys, vec!["win/1", "win/2"]);
    }

    #[test]
    fn patches_round_trip_through_json() {
        let patch: Patch<i32> = [upsert("a", 1), Change::remove("b")].into_iter().collect();
        let json = serde_json::to_string(&patch).expect("serializable");
        let back: Patch<i32> = serde_json::from_str(&json).expect("valid patch");
        assert_eq!(back, patch);
    }

    #[test]
    fn changes_serialise_as_tagged_objects() {
        let json = serde_json::to_string(&upsert("a", 5)).expect("serializable");
        assert_eq!(json, r#"{"op":"upsert","key":"a","value":5}"#);
        let json = serde_json::to_string(&Change::<i32>::remove("a")).expect("serializable");
        assert_eq!(json, r#"{"op":"remove","key":"a"}"#);
    }

    #[test]
    fn deliveries_round_trip_through_json_and_expose_their_range() {
        let delivery = PatchDelivery::new(3, 7, Patch::single(upsert("a", 1)));
        assert_eq!(delivery.from_revision(), 3);
        assert_eq!(delivery.to_revision(), 7);
        assert_eq!(delivery.len(), 1);
        assert!(!delivery.is_empty());

        let json = serde_json::to_string(&delivery).expect("serializable");
        let back: PatchDelivery<i32> = serde_json::from_str(&json).expect("valid delivery");
        assert_eq!(back, delivery);
        assert_eq!(back.into_patch(), Patch::single(upsert("a", 1)));
    }

    #[test]
    fn a_delivery_can_carry_an_empty_patch_over_a_non_empty_range() {
        let delivery = PatchDelivery::new(1, 5, Patch::<i32>::empty());
        assert!(delivery.is_empty());
        assert_ne!(delivery.from_revision(), delivery.to_revision());
    }
}
