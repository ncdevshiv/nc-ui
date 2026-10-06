//! Incremental application state: revisioned stores, change patches, and filtered
//! subscriptions.
//!
//! # The problem this crate exists to solve
//!
//! A GUI application has state that changes constantly and a display that can change only
//! a little at a time. Two failure modes sit at either end of the scale, and both are
//! unacceptable:
//!
//! * **Whole-tree resynchronisation.** Keeping a mirror of every record as a live DOM
//!   node means a million logical records demand a million nodes, most of them invisible
//!   (spec 18). The fix is to hold state in a data structure that has no relationship to
//!   the display at all, and only materialise what is on screen.
//! * **Whole-application re-render.** Diffing the entire tree after every mutation means
//!   one keystroke costs a traversal of everything (spec 15). The fix is to propagate the
//!   *delta*, so the cost of a mutation is proportional to what actually changed.
//!
//! [`virtualize::WindowCalculator`] closes the first loop: it computes which indices a
//! viewport shows without ever materialising the ones it does not, so a million rows cost
//! a five-row window.
//!
//! [`Store`], [`Patch`] and [`Subscription`] are the three pieces that make both fixes
//! possible at once. State is plain data. Change travels as a diff. Consumers pull diffs
//! by revision, so a consumer that misses a hundred updates receives one patch rather
//! than a hundred, and never more than what changed.
//!
//! # The model, in one paragraph
//!
//! A [`Store`] holds records keyed by [`StateKey`] and carries a monotonically increasing
//! `revision`. Every committed mutation advances the revision by exactly one and records
//! the [`Patch`] that it applied. Consumers never ask "what does the store contain?" —
//! they ask "what changed since the revision I last saw?", and get a
//! [`PatchDelivery`]: a coalesced patch plus the revision range it covers, so their cursor
//! advances by a fact rather than a guess. The recorded history is a bounded ring, so a
//! consumer that falls further behind than the ring is deep is told to resynchronise
//! explicitly instead of being handed a patch with a silent hole in it.
//!
//! # Invariants
//!
//! These hold across the whole crate and are each covered by a named test.
//!
//! 1. **Revisions are strictly monotonic.** Every state-changing operation advances the
//!    revision by exactly one; operations that change nothing advance it by zero.
//! 2. **A patch is atomic.** Applying an *n*-change patch advances a store by one
//!    revision, so a burst of mutations is indistinguishable from one mutation to an
//!    observer that only ever sees deliveries.
//! 3. **Coalescing is order-stable and outcome-preserving.** Collapsing a patch to one
//!    change per key keeps each key's first position and last value, is idempotent, and
//!    produces a store identical to the one the uncoalesced patch would have produced.
//! 4. **Removal is total, never an error.** Applying a removal for an absent key succeeds
//!    and changes nothing, which is what makes replay and retry converge.
//! 5. **A gap is always announced.** A consumer that asks for a revision the ring no
//!    longer holds receives an error, never a partial patch; the subscription records the
//!    exact lost range and refuses further deltas until the resync flag is cleared.
//!
//! # Example
//!
//! ```
//! use ncui_state::{Store, SubscriptionFilter};
//!
//! // A store is homogeneous in its value type; a real application uses one record type
//! // per subtree and one store per subtree, which is what makes the prefix filter
//! // meaningful.
//! let mut store = Store::new();
//! assert!(store.insert("win/1/title", "Untitled".to_owned()).is_none());
//! store.insert("win/1/zoom", "1.0".to_owned());
//! store.insert("log/1/lines", String::new());
//!
//! // A subscriber interested only in the window subtree polls by revision.
//! let mut subscription = store.subscribe(SubscriptionFilter::prefix("win/"));
//! let delivery = subscription.next_patch(0).expect("three revisions pending");
//! assert_eq!(delivery.from_revision(), 0);
//! assert_eq!(delivery.to_revision(), 3);
//! assert_eq!(delivery.patch().len(), 2, "`log/1/lines` was filtered out");
//!
//! // The consumer applies the delta to a mirror and advances its cursor to the end of
//! // the range — not by one.
//! let mut mirror = Store::new();
//! delivery.into_patch().apply_to(&mut mirror);
//! assert_eq!(mirror.get("win/1/zoom").map(String::as_str), Some("1.0"));
//! assert_eq!(mirror.len(), 2);
//! ```

pub mod patch;
pub mod store;
pub mod subscription;
pub mod virtualize;

pub use patch::{Change, Patch, PatchDelivery, StateKey};
pub use store::{DEFAULT_HISTORY_CAPACITY, DEFAULT_PENDING_LIMIT, Store};
pub use subscription::{Subscription, SubscriptionFilter};
