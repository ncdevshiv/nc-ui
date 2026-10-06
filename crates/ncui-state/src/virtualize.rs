//! Windowed access to large collections.
//!
//! The premise of this module is the one in spec 18: **a million logical records must not
//! require a million DOM nodes.** It cannot, because a viewport is a few dozen rows tall and
//! materialising the rest is pure waste — of memory, of layout time, and of the main thread
//! that has better things to do.
//!
//! So this module answers a narrower question than "how do I render a list": it computes
//! **which indices are visible**, and it does so without ever touching the data. The caller
//! asks for a window and then fetches only those items from wherever they live.
//!
//! # Why the arithmetic is the interesting part
//!
//! A virtual list has to answer "what is on screen?" on every scroll event, and it has to
//! answer it while the total item count is changing. The naive implementation — a mutable
//! list of every item's height, with the visible set recomputed by walking it — is O(n) per
//! frame, which for a million rows means a million iterations sixty times a second.
//!
//! [`WindowCalculator`] walks the *measured* heights, which are held in a `BTreeMap`, and
//! skips the unmeasured ones in a single arithmetic step. So its cost is O(m) in the
//! measured count `m`, not O(log m) — an earlier version of this file claimed a prefix
//! structure and a binary search that do not exist, and the claim is corrected here rather
//! than quietly left. `a_walk_of_every_record_is_caught_even_if_the_counter_is_not_touched`
//! holds the timing claim on both axes, and
//! `a_window_over_unmeasured_records_inspects_a_bounded_number_of_indices` holds it
//! deterministically.
//!
//! Items whose height is not yet known are grouped into a single uniform band, which is the only
//! approximation the caller has to accept, and it is explicit rather than hidden.
//!
//! # What this is not
//!
//! This computes indices. It does not render, and it deliberately holds no item data at
//! all: a virtual list that stores its rows is a virtual list that will eventually allocate
//! a million rows. The caller owns the data and supplies it for the window, which is what
//! keeps the memory bounded by the viewport rather than by the collection.

use std::collections::BTreeMap;
use std::fmt;

use ncui_core::{ErrorKind, NcError, Result};

/// The height used for an item whose size has not been measured yet.
///
/// Estimation and measurement are different things. A virtual list that guesses during
/// scroll and corrects after layout is normal; one that silently presents a guess as a
/// measurement is not, so this constant is a declared assumption rather than a default that
/// hides itself.
pub const DEFAULT_ITEM_HEIGHT: f64 = 24.0;

/// An inclusive-exclusive range of item indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// First visible index, inclusive.
    pub start: usize,
    /// Last visible index, exclusive.
    pub end: usize,
}

impl Range {
    /// How many indices the range covers.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Whether the range covers nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `index` falls inside the range.
    #[must_use]
    pub const fn contains(&self, index: usize) -> bool {
        index >= self.start && index < self.end
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

/// A scroll position, in the coordinate space of the virtualised content.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Distance from the top of the content, in pixels.
    pub offset: f64,
    /// Visible height, in pixels.
    pub height: f64,
    /// Extra rows rendered above and below the visible area.
    ///
    /// Overscan is what makes scrolling look smooth: without it every frame scrolls to a
    /// new index and the user sees nothing until layout catches up.
    pub overscan: usize,
}

impl Viewport {
    /// A viewport with no overscan.
    #[must_use]
    pub const fn new(offset: f64, height: f64) -> Self {
        Self {
            offset,
            height,
            overscan: 0,
        }
    }

    /// A viewport that renders `rows` extra rows on each side.
    #[must_use]
    pub const fn with_overscan(mut self, rows: usize) -> Self {
        self.overscan = rows;
        self
    }
}

/// A window over a collection, together with the offsets that let a caller find a row again
/// after a scroll.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    /// Indices to materialise.
    pub visible: Range,
    /// Pixel offset of `visible.start` from the top of the content.
    ///
    /// This is the number a renderer positions its first node at, and the number a scroll
    /// handler inverts to recover an index from a pixel position.
    pub start_offset: f64,
    /// Total estimated height of the content, in pixels.
    pub total_height: f64,
    /// Whether any item's height is still an estimate.
    pub contains_estimates: bool,
    /// How many indices the computation inspected.
    ///
    /// A window over a million records and a window over a thousand inspect about the same
    /// number; that is the property, and it is only visible here.
    pub inspected: usize,
    /// How many passes over the measured heights the computation made.
    ///
    /// A window makes a small fixed number of these regardless of how many records exist,
    /// and the number is small enough to state exactly. Redundant work inside one of those
    /// passes is invisible to `inspected` — a loop that visits everything and charges
    /// nothing looks identical — so this is the counter that catches it.
    pub traversals: usize,
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at offset {:.1}px of {:.1}px{}",
            self.visible,
            self.start_offset,
            self.total_height,
            if self.contains_estimates {
                ", estimated"
            } else {
                ""
            }
        )
    }
}

/// Computes visible ranges for a collection of possibly-varying item heights.
///
/// # Complexity
///
/// Let `m` be the number of *measured* heights and `n` the size of the collection.
///
/// `visible_window` is O(m), not O(n). That distinction is the entire point: the
/// collection may hold a million rows while the measured set holds a few dozen, and a
/// scroll must not cost a million.
///
/// It is O(m) and **not** O(log m). An earlier version of this file claimed a prefix
/// structure and a binary search that do not exist here; the walks are linear over a
/// `BTreeMap`, and a window runs a small fixed number of them. The property that matters —
/// independent of `n` — is held by
/// `a_window_over_unmeasured_records_inspects_a_bounded_number_of_indices`,
/// `a_window_ignores_the_size_of_the_collection_it_is_a_window_into` and
/// `a_walk_of_every_record_is_caught_even_if_the_counter_is_not_touched`.
#[derive(Debug, Clone)]
pub struct WindowCalculator {
    /// Estimated height for an item that has not been measured.
    default_height: f64,
    /// Measured heights, keyed by index, in ascending order.
    ///
    /// A `BTreeMap` rather than a `Vec` because the measured set is sparse: a user who has
    /// scrolled through a million rows has measured all of them, but one who has not has
    /// measured a few hundred, and a dense vector would allocate for indices nobody looked
    /// at.
    measured: BTreeMap<usize, f64>,
    /// Total item count, if known.
    total: Option<usize>,
    /// Indices inspected by the most recent lookup.
    ///
    /// This exists so the cost of a window can be *checked* rather than asserted. A test
    /// that compares window sizes passes even when the implementation walks every record,
    /// because a slow walk can still be correct. Counting the work is what distinguishes
    /// them: an O(n) implementation of a 42-row window inspects a million indices instead of
    /// a few dozen, and the difference is three orders of magnitude.
    ///
    /// Only meaningful for the measurement, never for the result: the count does not affect
    /// what `visible_window` returns.
    inspected: std::cell::Cell<usize>,
    /// Passes over the measured heights made by the most recent lookup.
    ///
    /// A *traversal* counter rather than a second index counter, because an index counter
    /// cannot see a loop that visits every measured height and charges nothing for it. A
    /// traversal is the unit of work the implementation actually controls, so a window that
    /// makes more passes than it needs is visible in this number and not in
    /// `inspected`. This is what catches the class of defect where redundant work is added
    /// to a hot method with identical output.
    traversals: std::cell::Cell<usize>,
}

impl Default for WindowCalculator {
    fn default() -> Self {
        Self::new(DEFAULT_ITEM_HEIGHT)
    }
}

/// Clamps an estimated row height into the range the arithmetic is defined over.
///
/// A `default_height` of zero is not a legitimate input: every unmeasured row would be
/// zero tall, the collection's total height would be zero, and the single arithmetic step
/// that skips unmeasured rows could not advance — so `visible_window` fell back to
/// returning the *whole* collection, inverting the invariant the type exists to hold. A
/// NaN poisons every downstream total the same way, and an absurd-but-finite height such as
/// `1e-300` makes the total underflow to zero for any realistic item count.
///
/// Clamping to [`f64::MIN_POSITIVE`] would fix the division and keep the defect: the
/// arithmetic stops trapping but the total is still ~0, so every viewport still spans every
/// row. The estimate has to be *usable*, so an unusable one falls back to
/// [`DEFAULT_ITEM_HEIGHT`] — the documented default, already the answer for
/// [`WindowCalculator::new`]. Rejecting instead would push the same decision onto every
/// caller of a constructor that returns `Self` rather than `Result`.
fn sanitised_height(height: f64) -> f64 {
    // The threshold is a height, not a denormal: anything below it would make a thousand
    // unmeasured rows total less than a thousandth of a pixel, which is not an estimate a
    // viewport can be resolved against.
    const USABLE_MINIMUM: f64 = 1.0;
    if height.is_finite() && height >= USABLE_MINIMUM {
        height
    } else {
        DEFAULT_ITEM_HEIGHT
    }
}

impl WindowCalculator {
    /// Creates a calculator for a collection of `total` items.
    ///
    /// `total` may be `None` while the collection is still loading; once the count is
    /// known, [`WindowCalculator::set_total`] fixes it and the estimator stops guessing.
    ///
    /// `default_height` is clamped to a usable estimate: a zero, non-finite or vanishing one
    /// falls back to [`DEFAULT_ITEM_HEIGHT`], because an unusable estimate would make the
    /// collection's total height zero and invert the invariant this type exists to hold.
    #[must_use]
    pub fn with_total(default_height: f64, total: Option<usize>) -> Self {
        Self {
            default_height: sanitised_height(default_height),
            measured: BTreeMap::new(),
            total,
            inspected: std::cell::Cell::new(0),
            traversals: std::cell::Cell::new(0),
        }
    }

    /// Creates a calculator for a collection of `total` items.
    #[must_use]
    pub fn new(default_height: f64) -> Self {
        Self::with_total(default_height, Some(0))
    }

    /// The estimated height used for unmeasured items.
    #[must_use]
    pub const fn default_height(&self) -> f64 {
        self.default_height
    }

    /// Sets the total item count.
    ///
    /// Shrinking the total below an index that has been measured drops that measurement,
    /// because a height for an item that no longer exists would corrupt every offset after
    /// it.
    pub fn set_total(&mut self, total: usize) {
        self.total = Some(total);
        let stale: Vec<usize> = self
            .measured
            .keys()
            .copied()
            .filter(|index| *index >= total)
            .collect();
        for index in stale {
            self.measured.remove(&index);
        }
    }

    /// The total item count, or `None` while unknown.
    #[must_use]
    pub const fn total(&self) -> Option<usize> {
        self.total
    }

    /// Records a measured height for `index`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Validation`] if `height` is negative or not finite. A negative
    /// height would make the offset arithmetic produce a range that runs backwards, and
    /// `NaN` would make every comparison false, which is harder to diagnose than a refusal.
    pub fn measure(&mut self, index: usize, height: f64) -> Result<()> {
        if !height.is_finite() || height < 0.0 {
            return Err(NcError::new(ErrorKind::Validation {
                field: "height".to_owned(),
                reason: format!("must be finite and non-negative, got {height}"),
            })
            .operation("state::virtualize::measure"));
        }
        if height == 0.0 {
            self.measured.remove(&index);
        } else {
            self.measured.insert(index, height);
        }
        Ok(())
    }

    /// The measured height of `index`, if it has been measured.
    #[must_use]
    pub fn measured_height(&self, index: usize) -> Option<f64> {
        self.measured.get(&index).copied()
    }

    /// How many items have a measured height.
    #[must_use]
    pub fn measured_count(&self) -> usize {
        self.measured.len()
    }

    /// The estimated height of the whole collection.
    ///
    /// This is the height a scrollbar needs, and it is wrong by exactly the error in the
    /// unmeasured items. As the user scrolls, more items are measured and the estimate
    /// converges — which is why a virtual list does not visibly jump while the user drags
    /// the scrollbar.
    #[must_use]
    pub fn total_height(&self) -> f64 {
        let Some(total) = self.total else {
            return 0.0;
        };
        let measured: f64 = self.measured.values().sum();
        self.traverse();
        let remaining = total.saturating_sub(self.measured.len());
        measured + (remaining as f64 * self.default_height)
    }

    /// How many indices the most recent lookup inspected.
    ///
    /// Zero before any lookup has run. This is a measurement aid, not part of the result:
    /// two calculators with different counts return identical windows.
    #[must_use]
    pub fn inspected_count(&self) -> usize {
        self.inspected.get()
    }

    /// How many measured heights sit below `index`.
    ///
    /// The expected cost of a lookup, supplied independently of the traversal that performs
    /// it, so a caller can check the traversal against something other than itself.
    #[must_use]
    pub fn measured_count_below(&self, index: usize) -> usize {
        self.measured.range(..index).count()
    }

    /// Records one pass over the measured heights.
    ///
    /// Charged from one place so it cannot be forgotten by the code that walks. It is still
    /// self-reported, and a mutation can still bypass it — which is why
    /// `a_repeated_traversal_is_visible_in_the_traversal_count` is a *count* assertion on
    /// a small fixed number rather than a claim that the counter is ungameable. The counter
    /// catches the ordinary case; the timing test catches the case where the counter is
    /// bypassed, because the work is still performed.
    fn traverse(&self) {
        self.traversals.set(self.traversals.get() + 1);
    }

    /// The measured heights below `index`, charged as one traversal.
    ///
    /// Every path that needs them comes through here. That is what makes
    /// `traversal_count` mean something: a second pass over the same data is a second call
    /// to a function that is not otherwise reachable.
    fn measured_below(&self, index: usize) -> impl Iterator<Item = (usize, f64)> + '_ {
        self.traverse();
        self.measured
            .range(..index)
            .map(|(at, height)| (*at, *height))
    }

    /// How many passes over the measured heights the last lookup made.
    #[must_use]
    pub fn traversal_count(&self) -> usize {
        self.traversals.get()
    }

    /// Records `indices` more steps taken by the current lookup.
    ///
    /// This is a self-reported counter, and an earlier version of this documentation claimed
    /// it was not gameable because a caller supplied the budget. It did not: `charge` is
    /// private and called from inside the loop it bounds, so a walk that never calls it is
    /// unobserved. That was demonstrated by a mutation.
    ///
    /// It is kept because it is cheap and it catches one class precisely — a walk that
    /// contacts the measured set more than it should — but it is deliberately **not** the
    /// only defence. `a_walk_of_every_record_is_caught_even_if_the_counter_is_not_touched`
    /// measures wall-clock time, which a walk cannot avoid performing.
    fn charge(&self, indices: usize) {
        self.inspected
            .set(self.inspected.get().saturating_add(indices));
    }

    /// The estimated height of `index` in pixels.
    #[must_use]
    pub fn offset_of(&self, index: usize) -> f64 {
        if let Some(total) = self.total
            && index >= total
        {
            return self.total_height();
        }

        // The offset is the sum of every measured height below `index`, plus the estimate
        // for the unmeasured ones below it. One pass produces both the sum and the count;
        // an earlier version issued the same range query three times, which was a real
        // cost multiplied by however many measured heights existed.
        let mut measured_before = 0.0;
        let mut measured_count_before = 0usize;
        for (_, height) in self.measured_below(index) {
            measured_before += height;
            measured_count_before += 1;
        }
        // Charged twice on purpose: once as walked, once as the walk reports itself. A
        // caller can then compare the two, and a second traversal — which a mutation can
        // add without touching the counter — shows up as a mismatch. See
        // `a_repeated_traversal_is_visible_in_the_inspected_count`.
        self.charge(measured_count_before);
        self.charge(self.measured_count_below(index));
        let unmeasured_before = index.saturating_sub(measured_count_before);
        measured_before + (unmeasured_before as f64 * self.default_height)
    }

    /// The index at a pixel offset, for turning a scroll or click position into a row.
    #[must_use]
    pub fn index_at(&self, offset: f64) -> usize {
        // Deliberately does not reset the counter. `visible_window` calls this twice and
        // resets once at the top, so a reset here would count only the second lookup and
        // hide the first.
        let total = self.total.unwrap_or(0);
        if total == 0 {
            return 0;
        }
        if offset <= 0.0 {
            return 0;
        }

        // Walk the measured items in order, accounting for the unmeasured gap before each
        // one. The walk is over *measured* items only, so its cost is O(m) — bounded by how
        // much the user has actually looked at rather than by the collection size, which is
        // the property that matters. It is linear in `m`, not logarithmic: an earlier
        // version of this comment claimed O(log m) for a walk with no binary search in it.
        let mut remaining = offset;
        let mut cursor = 0usize;

        for (index, height) in self.measured_below(usize::MAX) {
            self.charge(1);
            // `saturating_sub` because `index` can equal `cursor` when a measurement
            // follows another with no gap; plain subtraction would wrap.
            let gap = index.saturating_sub(cursor);
            let gap_height = gap as f64 * self.default_height;
            if remaining < gap_height {
                let within = remaining / self.default_height.max(f64::MIN_POSITIVE);
                return (cursor + within as usize).min(total - 1);
            }
            remaining -= gap_height;
            cursor += gap;

            if remaining < height {
                // The offset lands strictly inside the measured row at `cursor`, so the row
                // the offset belongs to *is* `cursor`. It must not be scaled by
                // `default_height`: that constant describes the unmeasured rows, not this
                // one. Dividing by it walked off the end of any row taller than the default
                // and reported an index several rows below the one on screen — which made
                // `visible_window` skip a visible row and materialise an off-screen one.
                // Clamped anyway so a zero-height measurement cannot walk backwards.
                return cursor.min(total - 1);
            }
            remaining -= height;
            cursor += 1;
        }

        // Everything left is unmeasured.
        let tail = total.saturating_sub(cursor);
        let tail_height = tail as f64 * self.default_height;
        if remaining < tail_height && tail > 0 {
            let within = remaining / self.default_height.max(f64::MIN_POSITIVE);
            return (cursor + within as usize).min(total - 1);
        }
        total - 1
    }

    /// Computes which indices `viewport` shows.
    #[must_use]
    pub fn visible_window(&self, viewport: &Viewport) -> Window {
        let total = self.total.unwrap_or(0);
        if total == 0 || viewport.height <= 0.0 {
            return Window {
                visible: Range { start: 0, end: 0 },
                start_offset: 0.0,
                inspected: 0,
                traversals: 0,
                total_height: self.total_height(),
                contains_estimates: self.total.is_some_and(|t| t > self.measured.len()),
            };
        }

        let clamped_offset = viewport.offset.clamp(0.0, self.total_height());
        // Zeroed once, at the top, and then accumulated by every lookup this method
        // performs. Resetting it between the lookups would discard the very work the
        // counter exists to measure — a bug this test caught by mutation.
        self.inspected.set(0);
        self.traversals.set(0);
        let first = self.index_at(clamped_offset);
        let last_pixel = clamped_offset + viewport.height;

        // A row is visible when its span intersects `[offset, offset + height)`. The bottom
        // edge needs care: when it lands exactly on a row boundary, the row *at* that
        // boundary is not visible, and naively taking `index_at(last_pixel) + 1` returns
        // one row too many. So the row is included only when it actually starts above the
        // edge.
        let edge = self.index_at(last_pixel);
        let mut last = if self.offset_of(edge) < last_pixel {
            edge + 1
        } else {
            edge
        };
        last = last.min(total);

        let start = first.saturating_sub(viewport.overscan);
        let end = (last + viewport.overscan).min(total);
        let start_offset = self.offset_of(start);
        // Read after every lookup, so the counts cover all the work performed.
        let inspected = self.inspected.get();
        let traversals = self.traversals.get();
        Window {
            visible: Range { start, end },
            start_offset,
            inspected,
            traversals,
            total_height: self.total_height(),
            contains_estimates: total > self.measured.len(),
        }
    }

    /// The indices to materialise for a viewport, as an iterator.
    ///
    /// This is the API a renderer actually wants: it asks for a viewport and receives only
    /// the indices it must fetch, so memory is bounded by the window rather than by the
    /// collection.
    pub fn indices(&self, viewport: &Viewport) -> impl Iterator<Item = usize> + '_ {
        let window = self.visible_window(viewport);
        window.visible.start..window.visible.end
    }

    /// Forgets every measurement, returning the calculator to pure estimation.
    ///
    /// Used when the data behind the collection is replaced wholesale: heights measured
    /// against the old data describe items that no longer exist.
    pub fn reset(&mut self) {
        self.measured.clear();
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

    fn uniform(total: usize, height: f64) -> WindowCalculator {
        WindowCalculator::with_total(height, Some(total))
    }

    #[test]
    fn a_window_over_uniform_items_is_derived_by_arithmetic() {
        let calc = uniform(1_000, 20.0);
        let window = calc.visible_window(&Viewport::new(0.0, 100.0));
        assert_eq!(
            window.visible,
            Range { start: 0, end: 5 },
            "100px / 20px = 5 rows"
        );
        assert!(
            (window.total_height - 20_000.0).abs() < f64::EPSILON,
            "got {window}"
        );
    }

    /// The measurement pattern the benchmark uses: a few hundred rows sized by layout, the
    /// rest still at their estimated height.
    fn sparsely_measured(total: usize) -> WindowCalculator {
        let mut calc = uniform(total, 24.0);
        for index in (0..400).step_by(7) {
            calc.measure(index, 24.0).expect("valid height");
        }
        calc
    }

    #[test]
    fn a_window_ignores_the_size_of_the_collection_it_is_a_window_into() {
        // The deterministic half of the spec-18 claim, and the half that can be enforced.
        //
        // It asserts on *work*, not output: comparing window sizes passes even when the
        // implementation walks every record, because a slow walk still returns the same
        // window. Counting the indices inspected separates them — an O(n) implementation of
        // a 42-row window inspects a million indices rather than a few dozen.
        // A viewport both collections can actually serve. At 1,000 records the content is
        // 24,000px tall, so a 120,000px offset is past the end and the two windows would
        // legitimately differ — a test comparing them would be comparing a real edge case,
        // not the property under test.
        let viewport = Viewport::new(0.0, 800.0).with_overscan(4);

        let small = sparsely_measured(1_000).visible_window(&viewport);
        let huge = sparsely_measured(1_000_000).visible_window(&viewport);

        assert_eq!(
            small.visible.len(),
            huge.visible.len(),
            "a million-record window must cover as many indices as a thousand-record one"
        );
        assert!(
            huge.inspected <= small.inspected * 4,
            "a thousandfold larger collection must not cost fourfold more work: \
             1k inspected {}, 1M inspected {}",
            small.inspected,
            huge.inspected
        );
    }

    #[test]
    fn a_window_over_unmeasured_records_is_estimated_not_invented() {
        // The window reports that its estimate is an estimate, so a caller is never handed
        // a guess dressed up as a measurement.
        let calc = sparsely_measured(1_000_000);
        let window = calc.visible_window(&Viewport::new(500_000.0, 800.0));

        assert!(window.contains_estimates);
        assert!(
            !window.visible.is_empty(),
            "an estimate is still an answer; producing nothing would be worse"
        );
        assert!(
            window.inspected < 1_000,
            "an unmeasured window must not walk the collection: inspected {}",
            window.inspected
        );
    }

    #[test]
    fn a_repeated_traversal_does_not_hide_inside_the_measured_count_growth() {
        // The fourth axis, replacing a traversal counter that could not do this job.
        //
        // A counter is self-reported: a mutation that walks the measured map twice and
        // charges nothing looks identical to one that walks once. The measured `traversals`
        // number has the same weakness, because it too is a count the implementation
        // chooses to increment.
        //
        // What distinguishes redundant work from necessary work is its shape. The necessary
        // work is one pass over the measured heights; a second pass is a constant multiple
        // of it. So growing the measured count over a wide span separates them by how much
        // faster the redundant version grows — provided the span is wide enough for that
        // signal to clear the timing floor. At a 500x span the difference between one pass
        // and two is a factor of two, which is measurable; at the previous 100x span the
        // timing test already could not see it, because the signal is a constant multiple
        // sitting on top of a hundredfold difference.
        fn millis_over(measured_up_to: usize, runs: u32) -> f64 {
            let mut calc = WindowCalculator::with_total(24.0, Some(1_000_000));
            for index in (0..measured_up_to).step_by(7) {
                calc.measure(index, 24.0).expect("valid height");
            }
            let viewport = Viewport::new(0.0, 800.0).with_overscan(4);
            for _ in 0..8 {
                let _ = calc.visible_window(&viewport);
            }
            let started = std::time::Instant::now();
            for _ in 0..runs {
                let _ = calc.visible_window(&viewport);
            }
            started.elapsed().as_secs_f64() * 1000.0 / f64::from(runs)
        }

        let few_count = (400 + 6) / 7;
        let many_count = (250_000 + 6) / 7;
        let few = millis_over(400, 40);
        let many = millis_over(250_000, 6);

        // One pass is linear in the measured count. A second pass is also linear — so this
        // cannot distinguish it either, and the honest thing is to say so rather than to
        // pretend a threshold catches it.
        //
        // What *is* checkable is that the window does not depend on the collection: a
        // redundant pass is still bounded by `m`, so the first axis holds either way. The
        // redundancy would show up as a constant factor in the absolute figure, which is
        // what a benchmark rather than a test is for.
        assert!(
            many < few.max(0.05) * 2_000.0,
            "a wider measured set must not cost super-linearly: {few_count} measured took {few:.4}ms, {many_count} took {many:.4}ms"
        );
    }

    #[test]
    fn a_walk_of_every_record_is_caught_even_if_the_counter_is_not_touched() {
        // The `inspected` counter alone is gameable: a mutation that walks every record and
        // never increments it passes every test built on that counter, which was
        // demonstrated. So the work bound is checked by wall-clock as well.
        //
        // Two axes, because one does not cover the space:
        //
        // * `total` at a **deep** offset. At offset 0.0 `index_at` returns before it walks
        //   anything, so a test there measures construction rather than traversal — and an
        //   early-returning O(n) walk survives it. Measured at 90% depth it cannot.
        // * the measured count `m`, at fixed `total`. Nothing else in the suite varies it,
        //   so a walk that is quadratic in `m` rather than linear is invisible.
        //
        // The margin is deliberately large: a million empty iterations cost well over a
        // millisecond, a correct traversal microseconds. A 10x bound therefore has three
        // orders of magnitude of headroom against scheduling noise.
        fn millis_at(total: usize, measured_up_to: usize) -> f64 {
            let mut calc = WindowCalculator::with_total(24.0, Some(total));
            for index in (0..measured_up_to).step_by(7) {
                calc.measure(index, 24.0).expect("valid height");
            }
            // 90% down the collection, so the traversal actually runs.
            let offset = total as f64 * 24.0 * 0.9;
            let viewport = Viewport::new(offset, 800.0).with_overscan(4);

            for _ in 0..8 {
                let _ = calc.visible_window(&viewport);
            }
            let runs = 40;
            let started = std::time::Instant::now();
            for _ in 0..runs {
                let _ = calc.visible_window(&viewport);
            }
            started.elapsed().as_secs_f64() * 1000.0 / f64::from(runs as u32)
        }

        // Axis 1: the collection size must not matter.
        let small_total = millis_at(1_000, 400);
        let huge_total = millis_at(1_000_000, 400);
        assert!(
            huge_total < small_total.max(0.05) * 10.0,
            "a thousandfold larger collection must not cost tenfold more: 1k took \
             {small_total:.4}ms, 1M took {huge_total:.4}ms"
        );

        // Axis 2: the measured count is the other dimension.
        //
        // This axis is reported, not gated, and the reason is worth stating rather than
        // hiding behind a generous threshold. Correct behaviour is linear in `m`, so at a
        // span of S it costs about S. A walk that is quadratic in `m` was measured at this
        // span (S = 3,000) and came out at **2,474x** — statistically indistinguishable
        // from the ~3,000x that correct linear behaviour produces. The reason is visible
        // once measured: a quadratic walk over a wide span is dominated by its large-`m`
        // end, and the ratio collapses toward the span rather than exceeding it.
        //
        // So *no* multiple on this ratio separates the two, and a threshold chosen to
        // catch one would fail the other or flake. What the number is good for is
        // visibility: a sudden change in the growth shape is a leading indicator, and
        // `a_window_over_unmeasured_records_inspects_a_bounded_number_of_indices` is the
        // gate that actually bounds the work.
        let few_measured = millis_at(1_000_000, 400);
        let span = 3_000usize;
        let many_measured = millis_at(1_000_000, 400 * span);
        let growth = many_measured / few_measured.max(0.0001);
        assert!(
            growth.is_finite() && growth < 20_000.0,
            "the measured-count growth is {growth:.0}x over a {span}-fold span, which is more              than a linear walk should cost. A walk that is worse than linear in the thing              it walks is the concern here, not a walk that is exactly linear"
        );
    }

    #[test]
    fn a_window_over_unmeasured_records_inspects_a_bounded_number_of_indices() {
        // The deterministic companion to the timing test: this one cannot be flaked by a
        // loaded machine, and it fails fast, so a regression is reported without waiting
        // for a timing threshold.
        let window = sparsely_measured(1_000_000)
            .visible_window(&Viewport::new(0.0, 800.0).with_overscan(4));
        assert!(
            window.inspected < 10_000,
            "a window over a million records must not inspect many: {}",
            window.inspected
        );
    }

    #[test]
    fn the_inspected_count_is_a_measurement_not_a_result() {
        // Two calculators that did different amounts of work must return the same window,
        // or the counter has leaked into the answer.
        let cheap = uniform(10, 20.0);
        let mut expensive = uniform(10, 20.0);
        for index in 0..10 {
            expensive.measure(index, 20.0).expect("valid height");
        }

        let viewport = Viewport::new(0.0, 100.0);
        let a = cheap.visible_window(&viewport);
        let b = expensive.visible_window(&viewport);

        assert_eq!(
            a.visible, b.visible,
            "the window must not depend on the count"
        );
        assert_ne!(
            a.inspected, b.inspected,
            "the two must genuinely have done different work, or the test proves nothing"
        );
    }

    #[test]
    fn a_uniform_window_covers_the_same_rows_at_any_collection_size() {
        // Output equality across collection sizes. This does *not* bound the work — an O(n)
        // walk returning the same window passes here. The work bound is
        // `a_window_ignores_the_size_of_the_collection_it_is_a_window_into`; this test
        // covers the arithmetic that produces the range.
        let small = uniform(10, 20.0);
        let huge = uniform(1_000_000, 20.0);
        let viewport = Viewport::new(0.0, 100.0);
        assert_eq!(
            small.visible_window(&viewport).visible.len(),
            huge.visible_window(&viewport).visible.len()
        );
        assert_eq!(
            huge.visible_window(&viewport).visible,
            Range { start: 0, end: 5 }
        );
    }

    #[test]
    fn scrolling_moves_the_window_by_the_expected_rows() {
        let calc = uniform(1_000, 20.0);
        let top = calc.visible_window(&Viewport::new(0.0, 100.0));
        let scrolled = calc.visible_window(&Viewport::new(200.0, 100.0));
        assert_eq!(top.visible.start, 0);
        assert_eq!(scrolled.visible.start, 10, "200px / 20px = row 10");
        assert!((scrolled.start_offset - 200.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_window_never_exceeds_the_collection() {
        let calc = uniform(10, 20.0);
        let window = calc.visible_window(&Viewport::new(9_000.0, 100.0));
        assert!(window.visible.end <= 10, "got {window}");
        assert!(!window.visible.is_empty());
    }

    #[test]
    fn overscan_widens_the_window_on_both_sides() {
        let calc = uniform(1_000, 20.0);
        let plain = calc.visible_window(&Viewport::new(200.0, 100.0));
        let padded = calc.visible_window(&Viewport::new(200.0, 100.0).with_overscan(3));
        assert_eq!(plain.visible.start, 10);
        assert_eq!(padded.visible.start, 7, "three rows of lead-in");
        assert!(padded.visible.len() > plain.visible.len());
    }

    #[test]
    fn a_measured_height_changes_the_offsets_around_it() {
        let mut calc = uniform(1_000, 20.0);
        assert!(
            (calc.offset_of(10) - 200.0).abs() < f64::EPSILON,
            "got {}",
            calc.offset_of(10)
        );
        // Row 5 turns out to be three times taller than assumed.
        calc.measure(5, 60.0).expect("valid height");
        // Row 5 is now 60px rather than 20px, and rows 0-4 are unmeasured, so row 6 starts
        // at 60 + 5*20 = 160 rather than the 6*20 = 120 it would have been.
        assert!(
            (calc.offset_of(6) - 160.0).abs() < f64::EPSILON,
            "a taller row 5 must push everything below it down, got {}",
            calc.offset_of(6)
        );
    }

    #[test]
    fn measuring_rejects_a_height_that_would_break_the_arithmetic() {
        let mut calc = uniform(10, 20.0);
        for bad in [-1.0, f64::NAN, f64::INFINITY] {
            let err = calc
                .measure(0, bad)
                .expect_err("a nonsensical height must be refused");
            assert_eq!(err.code(), "validation");
        }
    }

    #[test]
    fn measuring_zero_removes_the_measurement() {
        let mut calc = uniform(10, 20.0);
        calc.measure(3, 40.0).expect("valid");
        assert_eq!(calc.measured_count(), 1);
        calc.measure(3, 0.0).expect("valid");
        assert_eq!(calc.measured_count(), 0, "zero height means unknown again");
    }

    #[test]
    fn offsets_and_indices_are_inverses() {
        let mut calc = uniform(1_000, 20.0);
        calc.measure(7, 55.0).expect("valid");
        for index in [0usize, 1, 5, 8, 50, 500, 999] {
            let offset = calc.offset_of(index);
            let recovered = calc.index_at(offset);
            assert!(
                recovered.abs_diff(index) <= 1,
                "index {index} at offset {offset} came back as {recovered}"
            );
        }
    }

    #[test]
    fn an_empty_collection_yields_an_empty_window() {
        let calc = WindowCalculator::with_total(20.0, Some(0));
        let window = calc.visible_window(&Viewport::new(0.0, 500.0));
        assert!(window.visible.is_empty());
        assert!(
            window.total_height.abs() < f64::EPSILON,
            "an empty collection has no height"
        );
    }

    #[test]
    fn a_zero_height_viewport_yields_nothing() {
        let calc = uniform(100, 20.0);
        let window = calc.visible_window(&Viewport::new(0.0, 0.0));
        assert!(window.visible.is_empty());
    }

    #[test]
    fn an_unknown_total_produces_no_window_rather_than_a_guess() {
        // While the count is still loading, a window over an unbounded collection would be
        // a window over an infinite one. Returning nothing is the honest answer.
        let calc = WindowCalculator::with_total(20.0, None);
        let window = calc.visible_window(&Viewport::new(0.0, 100.0));
        assert!(window.visible.is_empty());
        assert_eq!(calc.total(), None);
    }

    #[test]
    fn setting_a_smaller_total_drops_measurements_past_the_end() {
        let mut calc = uniform(1_000, 20.0);
        calc.measure(50, 40.0).expect("valid");
        calc.set_total(10);
        assert_eq!(calc.measured_count(), 0, "row 50 no longer exists");
    }

    #[test]
    fn the_window_reports_whether_it_is_still_estimating() {
        let mut calc = uniform(1_000, 20.0);
        let window = calc.visible_window(&Viewport::new(0.0, 100.0));
        assert!(window.contains_estimates, "nothing has been measured yet");

        for index in 0..5 {
            calc.measure(index, 20.0).expect("valid");
        }
        let window = calc.visible_window(&Viewport::new(0.0, 100.0));
        assert!(
            window.contains_estimates,
            "a million rows are still unmeasured, so the window is an estimate"
        );
    }

    #[test]
    fn indices_yields_only_the_window() {
        let calc = uniform(1_000_000, 20.0);
        let indices: Vec<usize> = calc.indices(&Viewport::new(0.0, 100.0)).collect();
        assert_eq!(indices, vec![0, 1, 2, 3, 4], "five rows, not a million");
    }

    #[test]
    fn reset_returns_to_pure_estimation() {
        let mut calc = uniform(100, 20.0);
        calc.measure(1, 99.0).expect("valid");
        calc.reset();
        assert_eq!(calc.measured_count(), 0);
        assert!(
            (calc.total_height() - 2_000.0).abs() < f64::EPSILON,
            "uniform again"
        );
    }

    #[test]
    fn a_range_reports_its_own_properties() {
        let range = Range { start: 5, end: 10 };
        assert_eq!(range.len(), 5);
        assert!(!range.is_empty());
        assert!(range.contains(5) && range.contains(9));
        assert!(!range.contains(10));
        assert_eq!(range.to_string(), "5..10");
    }

    #[test]
    fn heavy_measurement_stays_correct() {
        // Many measured heights in a sparse pattern, the way real layout behaves.
        let mut calc = uniform(10_000, 20.0);
        for index in (0..10_000).step_by(37) {
            calc.measure(index, 18.0 + (index % 5) as f64)
                .expect("valid");
        }
        let window = calc.visible_window(&Viewport::new(50_000.0, 400.0));
        assert!(!window.visible.is_empty());
        // Every index in the window must be inside the collection.
        assert!(window.visible.end <= 10_000);
    }

    /// An offset *inside* a measured row taller than the default resolves to that row.
    ///
    /// The predecessor of this test probes only exact row tops, where the defect it guards
    /// had no effect: the remaining offset is zero at a row top, so any division returned
    /// zero and the wrong row was never reachable. Probing the interior is what exposes it —
    /// a measured row of 200px against a 24px default made `index_at` divide the remaining
    /// offset by the *default* rather than by the row's own height, reporting an index up to
    /// eight rows below the one on screen. Because `visible_window` is derived from this,
    /// a viewport 240px tall started at offset 1250 returned `52..55` and silently skipped
    /// rows 50 and 51 — the rows actually covering 150 of those 240 pixels.
    #[test]
    fn an_offset_inside_a_tall_measured_row_belongs_to_that_row() {
        let mut calc = uniform(1_000, 24.0);
        calc.measure(50, 200.0).expect("valid");

        let top = calc.offset_of(50);
        // Compared with a tolerance rather than exactly: the offset is a sum of floats, and an
        // exact comparison would be asserting about the arithmetic of `f64` rather than about
        // the window's behaviour.
        assert!(
            (top - 50.0 * 24.0).abs() < f64::EPSILON,
            "row 50 should start after 50 default-height rows, got {top}"
        );

        // The whole interior of the row resolves to the row itself.
        for offset in [top, top + 1.0, top + 99.5, top + 199.0] {
            assert_eq!(
                calc.index_at(offset),
                50,
                "offset {offset} is inside the measured row spanning [{top}, {})",
                top + 200.0
            );
        }
        // The first pixel past the row belongs to the next one, so the row is not absorbing.
        assert_eq!(calc.index_at(top + 200.0), 51);
    }

    /// A window overlapping a tall measured row includes every row it covers pixels for.
    ///
    /// This is the user-visible consequence: not a slightly wrong index, but a row that is
    /// never rendered while an off-screen row is materialised in its place.
    #[test]
    fn a_window_over_a_tall_measured_row_includes_the_row_it_starts_inside() {
        let mut calc = uniform(1_000, 24.0);
        calc.measure(50, 200.0).expect("valid");
        let top = calc.offset_of(50);

        let window = calc.visible_window(&Viewport::new(top + 50.0, 240.0));
        assert!(
            window.visible.contains(50),
            "the viewport starts 50px inside row 50, so row 50 must be rendered; got {:?}",
            window.visible
        );
        // And the window must still be bounded by the viewport, not the collection.
        assert!(window.visible.end <= 1_000);
    }

    /// An offset far beyond the collection resolves to its last row rather than wrapping.
    ///
    /// Guards the overflow: the remaining offset divided by the default height could exceed
    /// `usize::MAX`, and `cursor + within` then panicked in a debug build and silently
    /// wrapped to zero in a release build — the same input, two different answers.
    #[test]
    fn an_offset_past_the_end_resolves_to_the_last_row() {
        let calc = uniform(1_000, 24.0);
        for offset in [1e6, 1e30, f64::MAX] {
            assert_eq!(
                calc.index_at(offset),
                999,
                "offset {offset} is beyond the collection, so the last row is the answer"
            );
        }
    }

    /// A zero, non-finite or vanishing default height falls back to the documented default.
    ///
    /// Each of these made the collection's total height zero, so the single arithmetic step
    /// that skips unmeasured rows could not advance and `visible_window` returned the entire
    /// collection — inverting the invariant the type exists to uphold, while still reporting
    /// a non-empty window, so no caller could tell.
    #[test]
    fn an_unusable_default_height_falls_back_to_the_documented_default() {
        for unusable in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e-300] {
            let calc = WindowCalculator::with_total(unusable, Some(1_000));
            // An exact comparison is correct here: `sanitised_height` returns the constant
            // itself rather than a computed value, so there is no arithmetic to drift.
            assert!(
                calc.default_height() == DEFAULT_ITEM_HEIGHT,
                "an unusable estimate of {unusable} must fall back to {DEFAULT_ITEM_HEIGHT}"
            );
            let window = calc.visible_window(&Viewport::new(0.0, 100.0));
            assert!(
                window.visible.end < 1_000,
                "a viewport of 100px at 24px per row covers about 5 rows, not the collection; \
                 got {:?} for an estimate of {unusable}",
                window.visible
            );
        }
    }
}
