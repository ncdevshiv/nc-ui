//! Island geometry and the DOM/GPU composition boundary.
//!
//! ADR-002 calls this "the highest-risk area in the renderer: focus, scrolling, and IME must
//! behave identically whether content is DOM or GPU". That is a claim about *agreement*, so
//! the type that exists here is a single coordinate space that both sides speak.
//!
//! # The one invariant everything else rests on
//!
//! A point in window coordinates maps to at most one island, and an island's rectangle maps to
//! at most one window rectangle. If either could be true twice, a click would be delivered
//! twice or to the wrong island, and no amount of correctness elsewhere would help. Every
//! method here is written so that property is checkable, and [`Islands::hit_test`] is the
//! assertion that it holds.
//!
//! # High-DPI is a scale factor, not a second space
//!
//! Physical pixels and logical points are kept apart, and the conversion happens in exactly
//! three functions ([`Viewport::physical_size`], [`Viewport::to_physical`] and
//! [`Viewport::to_logical`]). Storing a device
//! pixel ratio per island would be the bug this design avoids: two islands scaled
//! differently cannot be composed into one frame, because the compositor has one surface.

use core::fmt;

/// A point in logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct LogicalPoint {
    /// Horizontal position, in logical pixels.
    pub x: f32,
    /// Vertical position, in logical pixels.
    pub y: f32,
}

impl LogicalPoint {
    /// A point.
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

impl fmt::Display for LogicalPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.x, self.y)
    }
}

/// A size in logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LogicalSize {
    /// Width, in logical pixels.
    pub width: f32,
    /// Height, in logical pixels.
    pub height: f32,
}

impl LogicalSize {
    /// A size.
    #[must_use]
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

/// An axis-aligned rectangle in logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
}

impl Rect {
    /// A rectangle.
    #[must_use]
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The right edge, exclusive.
    ///
    /// Half-open on the right and bottom, so two adjacent rectangles neither overlap nor leave
    /// a seam. A closed interval would make every pixel on a shared edge belong to two tiles,
    /// which is the same double-delivery the hit test forbids.
    #[must_use]
    pub const fn right(&self) -> f32 {
        self.x + self.width
    }

    /// The bottom edge, exclusive.
    #[must_use]
    pub const fn bottom(&self) -> f32 {
        self.y + self.height
    }

    /// The centre point.
    #[must_use]
    pub const fn center(&self) -> LogicalPoint {
        LogicalPoint::new(self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    /// Whether this rectangle has a usable area.
    ///
    /// A zero or negative extent cannot contain a point, and one that is merely small can, so
    /// the test is on sign rather than on a threshold: a 0.5px island is a real island that a
    /// user can click.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.width > 0.0 && self.height > 0.0 && self.x.is_finite() && self.y.is_finite()
    }

    /// Whether `point` is inside, right and bottom edges excluded.
    #[must_use]
    pub fn contains(&self, point: LogicalPoint) -> bool {
        point.x >= self.x && point.x < self.right() && point.y >= self.y && point.y < self.bottom()
    }

    /// Whether this and `other` share any area.
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    /// The point in this rectangle's own coordinates.
    #[must_use]
    pub const fn to_local(&self, point: LogicalPoint) -> LogicalPoint {
        LogicalPoint::new(point.x - self.x, point.y - self.y)
    }

    /// The point in window coordinates.
    #[must_use]
    pub const fn to_window(&self, point: LogicalPoint) -> LogicalPoint {
        LogicalPoint::new(point.x + self.x, point.y + self.y)
    }

    /// Whether this rectangle entirely contains `other`.
    #[must_use]
    pub fn contains_rect(&self, other: &Self) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }
}

/// How a window's logical coordinates map to device pixels.
///
/// A device pixel ratio is a property of the *display*, not of an island, so it lives on the
/// viewport rather than on anything that renders. Islands in one window therefore always
/// agree, which is what makes a single surface compositable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// The window's size in logical pixels.
    pub size: LogicalSize,
    /// Device pixels per logical pixel.
    pub scale_factor: f32,
}

impl Viewport {
    /// A viewport at a given scale.
    #[must_use]
    pub fn new(size: LogicalSize, scale_factor: f32) -> Self {
        // A scale of zero or a NaN would make every conversion either collapse to zero or
        // produce garbage, and both look like a working renderer until something is drawn.
        // The floor keeps the conversion defined; a caller wanting a real ratio sets one.
        let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        Self {
            size: LogicalSize::new(
                if size.width.is_finite() {
                    size.width
                } else {
                    0.0
                },
                if size.height.is_finite() {
                    size.height
                } else {
                    0.0
                },
            ),
            scale_factor,
        }
    }

    /// The window's size in device pixels.
    #[must_use]
    pub fn physical_size(&self) -> LogicalSize {
        LogicalSize::new(
            self.size.width * self.scale_factor,
            self.size.height * self.scale_factor,
        )
    }

    /// Converts a logical point to device pixels.
    #[must_use]
    pub fn to_physical(&self, point: LogicalPoint) -> LogicalPoint {
        LogicalPoint::new(point.x * self.scale_factor, point.y * self.scale_factor)
    }

    /// Converts a device point to logical pixels.
    #[must_use]
    pub fn to_logical(&self, point: LogicalPoint) -> LogicalPoint {
        LogicalPoint::new(point.x / self.scale_factor, point.y / self.scale_factor)
    }

    /// The window's whole area as a rectangle, in logical coordinates.
    #[must_use]
    pub fn bounds(&self) -> Rect {
        Rect::new(0.0, 0.0, self.size.width, self.size.height)
    }

    /// Whether `rect` lies entirely within the window.
    #[must_use]
    pub fn contains_rect(&self, rect: &Rect) -> bool {
        self.bounds().contains_rect(rect)
    }
}

impl Default for Viewport {
    fn default() -> Self {
        Self::new(LogicalSize::new(1280.0, 720.0), 1.0)
    }
}

/// What renders a region: the DOM, the GPU, or native widgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RenderMode {
    /// The embedded document renders it.
    Dom,
    /// The GPU engine renders it.
    Gpu,
    /// Native platform widgets render it.
    Native,
}

impl RenderMode {
    /// The stable wire name, which is what `render_mode` observability reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dom => "dom",
            Self::Gpu => "gpu",
            Self::Native => "native",
        }
    }
}

impl fmt::Display for RenderMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One GPU island: a rectangle of the page the GPU engine renders.
#[derive(Debug, Clone, PartialEq)]
pub struct Island {
    /// The island's stable identity, used in metrics, in accessibility, and in recovery.
    pub id: String,
    /// Where it sits, in window coordinates.
    pub bounds: Rect,
    /// The scroll position within the island's own content.
    pub scroll: LogicalPoint,
    /// How much content the island holds beyond its viewport.
    pub content_size: LogicalSize,
    /// What actually renders it, always [`RenderMode::Gpu`] for a GPU island.
    pub mode: RenderMode,
}

impl Island {
    /// An island with a rectangle and no scroll.
    #[must_use]
    pub fn new(id: &str, bounds: Rect) -> Self {
        Self {
            id: id.to_owned(),
            bounds,
            scroll: LogicalPoint::default(),
            content_size: LogicalSize::new(bounds.width, bounds.height),
            mode: RenderMode::Gpu,
        }
    }

    /// Sets the island's content size, which is what scrolling is bounded by.
    #[must_use]
    pub fn with_content(mut self, size: LogicalSize) -> Self {
        self.content_size = size;
        self
    }

    /// The furthest the island can scroll in each direction.
    ///
    /// Zero on an axis whose content fits, which is what makes a scroll gesture on a
    /// non-scrollable axis a no-op rather than a jitter.
    #[must_use]
    pub const fn max_scroll(&self) -> LogicalPoint {
        LogicalPoint::new(
            (self.content_size.width - self.bounds.width).max(0.0),
            (self.content_size.height - self.bounds.height).max(0.0),
        )
    }

    /// Whether the island is scrolled anywhere but its origin.
    #[must_use]
    pub const fn is_scrolled(&self) -> bool {
        self.scroll.x > 0.0 || self.scroll.y > 0.0
    }

    /// Applies a scroll delta, clamped to the content.
    ///
    /// Clamping rather than refusing is what a user expects: a fling past the end should
    /// stop at the end, and stopping at the end is observable, whereas refusing to scroll
    /// reads as a dropped frame.
    #[must_use]
    pub fn scrolled_by(mut self, delta: LogicalPoint) -> Self {
        let max = self.max_scroll();
        self.scroll.x = (self.scroll.x + delta.x).clamp(0.0, max.x);
        self.scroll.y = (self.scroll.y + delta.y).clamp(0.0, max.y);
        self
    }

    /// Maps a window point into content coordinates, if it is inside this island.
    #[must_use]
    pub fn content_point(&self, point: LogicalPoint) -> Option<LogicalPoint> {
        self.bounds
            .contains(point)
            .then(|| self.bounds.to_local(point) + self.scroll)
    }
}

impl core::ops::Add<LogicalPoint> for LogicalPoint {
    type Output = LogicalPoint;
    fn add(self, other: LogicalPoint) -> LogicalPoint {
        LogicalPoint::new(self.x + other.x, self.y + other.y)
    }
}

impl core::ops::Sub<LogicalPoint> for LogicalPoint {
    type Output = LogicalPoint;
    fn sub(self, other: LogicalPoint) -> LogicalPoint {
        LogicalPoint::new(self.x - other.x, self.y - other.y)
    }
}

/// Every island on a page, in composition order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Islands {
    islands: Vec<Island>,
    viewport: Viewport,
}

impl Islands {
    /// A page with no islands.
    #[must_use]
    pub fn new(viewport: Viewport) -> Self {
        Self {
            islands: Vec::new(),
            viewport,
        }
    }

    /// The viewport these islands compose into.
    #[must_use]
    pub const fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    /// The islands, in the order they were added.
    #[must_use]
    pub fn all(&self) -> &[Island] {
        &self.islands
    }

    /// Returns every island to the top of its content, in place.
    ///
    /// Narrower than a mutable accessor on purpose. A `&mut [Island]` would let a caller
    /// rewrite `id`, `bounds` or `mode` with no check — a strictly wider hole than
    /// [`Self::replace`], in a type whose purpose is maintaining an invariant. Scrolling is the
    /// only in-place edit this collection supports, so it is the only one offered, and it needs
    /// no `Result` because it cannot change identity, bounds or order.
    pub fn scroll_all_to_origin(&mut self) {
        for island in &mut self.islands {
            island.scroll = LogicalPoint::default();
        }
    }

    /// How many islands the page has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.islands.len()
    }

    /// Whether the page has no islands.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.islands.is_empty()
    }

    /// Adds an island.
    ///
    /// # Errors
    ///
    /// Returns a reason when the island cannot be composed: a duplicate id, a degenerate
    /// rectangle, a rectangle outside the window, or one that overlaps another. Overlap is
    /// refused rather than resolved by z-order because "the top one wins" is a rule the DOM
    /// and the GPU would then have to agree on, and disagreement is exactly the defect ADR-002
    /// warns about.
    pub fn add(&mut self, island: Island) -> Result<(), String> {
        if island.id.trim().is_empty() {
            return Err("an island must have an id".to_owned());
        }
        if self.islands.iter().any(|other| other.id == island.id) {
            return Err(format!("island `{}` already exists", island.id));
        }
        // Everything else is `validate_candidate`, so `add` and `replace` cannot drift apart: a
        // future invariant added there applies to both without anyone remembering to copy it.
        self.validate_candidate(&island, None)?;
        self.islands.push(island);
        Ok(())
    }

    /// The island containing `point`, if any.
    ///
    /// The whole composition boundary is this function: a point in window coordinates is
    /// delivered to exactly one island or to the DOM.
    #[must_use]
    pub fn hit_test(&self, point: LogicalPoint) -> Option<&Island> {
        self.islands
            .iter()
            .find(|island| island.bounds.contains(point))
    }

    /// The islands overlapping `rect`, in composition order.
    #[must_use]
    pub fn intersecting(&self, rect: Rect) -> Vec<&Island> {
        self.islands
            .iter()
            .filter(|island| island.bounds.overlaps(&rect))
            .collect()
    }

    /// Replaces one island, keeping its position in composition order.
    ///
    /// The replacement faces the same checks [`Self::add`] applies that are meaningful here —
    /// render mode, usable bounds, containment, and overlap against the islands it will sit
    /// beside. Without that, `replace` was a way to smuggle in an island `add` would have
    /// refused — an overlapping one, an off-window one, a zero-area one, one in
    /// [`RenderMode::Dom`] — which made [`Self::hit_test`]'s "delivered to exactly one island"
    /// promise false for anything that edited a collection this way.
    ///
    /// # Errors
    ///
    /// Returns a reason when there is no island with that id, or when the replacement breaks an
    /// invariant the collection maintains.
    pub fn replace(&mut self, island: Island) -> Result<(), String> {
        let at = self
            .islands
            .iter()
            .position(|other| other.id == island.id)
            .ok_or_else(|| format!("island `{}` does not exist", island.id))?;
        self.validate_candidate(&island, Some(at))
            .map_err(|e| format!("replacing `{}`: {e}", island.id))?;
        self.islands[at] = island;
        Ok(())
    }

    /// The checks [`Self::add`] applies to a new island, against the current set.
    ///
    /// `skip` is the index being replaced, excluded from the neighbour scan so an island is not
    /// checked against itself. [`Self::add`] delegates here too, after its own two id checks, so
    /// the two entry points cannot drift: render mode, usable bounds, containment, and
    /// non-self overlap — four checks, shared.
    ///
    /// The two id checks stay in `add` rather than here because they cannot fire for `replace`:
    /// it locates the slot by matching that very id against a set `add` already validated. A
    /// check that cannot fire reads as coverage and is not.
    fn validate_candidate(&self, island: &Island, skip: Option<usize>) -> Result<(), String> {
        if island.mode != RenderMode::Gpu {
            return Err(format!(
                "island `{}` declares render mode {}; only Gpu belongs in this collection",
                island.id, island.mode
            ));
        }
        if !island.bounds.is_valid() {
            return Err(format!(
                "island `{}` has bounds {:?}, which enclose no area",
                island.id, island.bounds
            ));
        }
        if !self.viewport.contains_rect(&island.bounds) {
            return Err(format!(
                "island `{}` at {:?} extends outside the window {:?}",
                island.id,
                island.bounds,
                self.viewport.bounds()
            ));
        }
        if let Some(other) = self
            .islands
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != skip)
            .find(|(_, other)| other.bounds.overlaps(&island.bounds))
        {
            return Err(format!(
                "island `{}` overlaps `{}`; overlap is refused rather than resolved by \
                 z-order, because DOM and GPU would then have to agree on which wins",
                island.id, other.1.id
            ));
        }
        Ok(())
    }

    /// The ids of islands that overlap each other.
    ///
    /// The invariant [`Islands::add`] maintains, checked over the whole set rather than
    /// assumed from the insertion path — a page assembled from several sources can reach an
    /// `Islands` by other means, and a renderer that silently drops a click is worse than one
    /// that reports a bad layout.
    #[must_use]
    pub fn overlapping_pairs(&self) -> Vec<(&str, &str)> {
        let mut pairs = Vec::new();
        for (i, first) in self.islands.iter().enumerate() {
            for second in &self.islands[i + 1..] {
                if first.bounds.overlaps(&second.bounds) {
                    pairs.push((first.id.as_str(), second.id.as_str()));
                }
            }
        }
        pairs
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    // Geometry is asserted on exact values here on purpose: a clamp boundary that is off by
    // one ulp is a real off-by-one, not float noise, so an epsilon would hide the defect.
    clippy::float_cmp,
    reason = "test assertions must abort loudly on failure, and geometry is exact"
)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_refuses_an_island_that_would_break_an_invariant_add_maintains() {
        // `replace` used to assign without re-running `add`'s checks, so anything that edited a
        // collection this way could smuggle in an island `add` would have refused — which made
        // `hit_test`'s "delivered to exactly one island" promise false.
        let mut page = Islands::new(viewport());
        page.add(Island::new("a", Rect::new(0.0, 0.0, 100.0, 100.0)))
            .expect("a");
        page.add(Island::new("b", Rect::new(200.0, 0.0, 100.0, 100.0)))
            .expect("b");

        // Overlapping its neighbour: refused, and the collection is untouched.
        let overlapping = Island::new("a", Rect::new(0.0, 0.0, 300.0, 100.0));
        assert!(
            page.replace(overlapping).is_err(),
            "an overlap must be refused"
        );
        assert!(
            page.overlapping_pairs().is_empty(),
            "a refused replace must not have been applied: {:?}",
            page.overlapping_pairs()
        );

        // Outside the window: refused.
        let outside = Island::new("a", Rect::new(5000.0, 5000.0, 100.0, 100.0));
        assert!(
            page.replace(outside).is_err(),
            "an off-window island must be refused"
        );

        // A DOM island in a GPU collection: refused.
        let mut dom = Island::new("a", Rect::new(0.0, 0.0, 100.0, 100.0));
        dom.mode = RenderMode::Dom;
        assert!(page.replace(dom).is_err(), "a Dom island must be refused");

        // Degenerate bounds: refused. `add` refuses these too, and `replace` used to accept
        // them, which put a zero-area island into a collection whose hit-testing assumes every
        // island is clickable.
        let zero_area = Island::new("a", Rect::new(0.0, 0.0, 0.0, 100.0));
        assert!(
            page.replace(zero_area).is_err(),
            "a zero-area island must be refused"
        );
        let negative = Island::new("a", Rect::new(0.0, 0.0, 100.0, -10.0));
        assert!(
            page.replace(negative).is_err(),
            "negative height must be refused"
        );

        // Unknown id: refused, as before.
        let unknown = Island::new("z", Rect::new(400.0, 0.0, 50.0, 50.0));
        assert!(
            page.replace(unknown).is_err(),
            "an unknown id must be refused"
        );

        // And a legitimate replacement still works — scrolling is the production caller.
        let mut scrolled = Island::new("a", Rect::new(0.0, 0.0, 100.0, 100.0));
        scrolled.scroll = LogicalPoint::new(0.0, 40.0);
        page.replace(scrolled)
            .expect("a scroll is a legal replacement");
        assert_eq!(page.all()[0].scroll.y, 40.0);
        assert_eq!(page.len(), 2, "the collection must be unchanged in size");
    }

    #[test]
    fn scroll_all_to_origin_leaves_identity_bounds_and_order_alone() {
        let mut page = Islands::new(viewport());
        for (id, x) in [("a", 0.0), ("b", 200.0), ("c", 400.0)] {
            let mut island = Island::new(id, Rect::new(x, 0.0, 100.0, 100.0));
            island.scroll = LogicalPoint::new(0.0, 25.0);
            page.add(island).expect("added");
        }
        let before: Vec<(String, Rect)> = page
            .all()
            .iter()
            .map(|i| (i.id.clone(), i.bounds))
            .collect();

        page.scroll_all_to_origin();

        assert!(
            page.all()
                .iter()
                .all(|i| i.scroll.y == 0.0 && i.scroll.x == 0.0)
        );
        let after: Vec<(String, Rect)> = page
            .all()
            .iter()
            .map(|i| (i.id.clone(), i.bounds))
            .collect();
        assert_eq!(
            before, after,
            "identity, bounds and composition order must be untouched"
        );
    }

    fn viewport() -> Viewport {
        Viewport::new(LogicalSize::new(1000.0, 800.0), 1.0)
    }

    fn island(id: &str, x: f32, y: f32, w: f32, h: f32) -> Island {
        Island::new(id, Rect::new(x, y, w, h))
    }

    #[test]
    fn a_point_belongs_to_exactly_one_island_or_the_dom() {
        // The composition boundary's whole invariant. A double hit means a click is
        // delivered twice; a miss means content that is visible cannot be clicked.
        let mut page = Islands::new(viewport());
        page.add(island("left", 0.0, 0.0, 500.0, 800.0))
            .expect("added");
        page.add(island("right", 500.0, 0.0, 500.0, 800.0))
            .expect("added");

        for (x, y) in [
            (10.0, 10.0),   // left
            (990.0, 790.0), // right
            (250.0, 400.0), // left, middle
            (750.0, 400.0), // right, middle
        ] {
            let hits: Vec<_> = page
                .all()
                .iter()
                .filter(|i| i.bounds.contains(LogicalPoint::new(x, y)))
                .collect();
            assert_eq!(hits.len(), 1, "({x}, {y}) landed in {} islands", hits.len());
        }

        // A point between and above the islands belongs to the DOM.
        let mut with_gap = Islands::new(viewport());
        with_gap
            .add(island("low", 0.0, 400.0, 1000.0, 400.0))
            .expect("added");
        assert!(
            with_gap.hit_test(LogicalPoint::new(500.0, 100.0)).is_none(),
            "a point above every island is the DOM's"
        );
    }

    #[test]
    fn adjacent_islands_share_no_pixel() {
        // Half-open edges: a pixel on the boundary must not belong to both, which is the
        // double-delivery case above in its subtlest form.
        let mut page = Islands::new(viewport());
        page.add(island("a", 0.0, 0.0, 100.0, 100.0))
            .expect("added");
        page.add(island("b", 100.0, 0.0, 100.0, 100.0))
            .expect("added");
        assert!(page.overlapping_pairs().is_empty(), "no overlap");
        let boundary = LogicalPoint::new(100.0, 50.0);
        assert!(
            !page.all()[0].bounds.contains(boundary),
            "a is half-open at the right"
        );
        assert!(
            page.all()[1].bounds.contains(boundary),
            "b owns the boundary pixel"
        );
    }

    #[test]
    fn an_overlapping_island_is_refused_rather_than_z_ordered() {
        let mut page = Islands::new(viewport());
        page.add(island("first", 0.0, 0.0, 200.0, 200.0))
            .expect("added");
        let error = page
            .add(island("second", 100.0, 100.0, 200.0, 200.0))
            .expect_err("overlap must be refused");
        assert!(error.contains("overlaps"), "{error}");
    }

    #[test]
    fn an_island_outside_the_window_is_refused() {
        let mut page = Islands::new(viewport());
        assert!(
            page.add(island("outside", 900.0, 700.0, 500.0, 500.0))
                .is_err()
        );
        // Exactly filling the window is fine.
        assert!(page.add(island("full", 0.0, 0.0, 1000.0, 800.0)).is_ok());
    }

    #[test]
    fn a_degenerate_or_duplicate_island_is_refused() {
        let mut page = Islands::new(viewport());
        assert!(page.add(island("zero", 0.0, 0.0, 0.0, 100.0)).is_err());
        assert!(page.add(island("negative", 0.0, 0.0, 100.0, -5.0)).is_err());
        assert!(
            page.add(island("infinite", 0.0, 0.0, f32::INFINITY, 100.0))
                .is_err()
        );
        assert!(
            page.add(island("nan", f32::NAN, 0.0, 100.0, 100.0))
                .is_err()
        );
        assert!(page.add(island("", 0.0, 0.0, 10.0, 10.0)).is_err());

        page.add(island("dup", 0.0, 0.0, 10.0, 10.0))
            .expect("added");
        assert!(page.add(island("dup", 20.0, 0.0, 10.0, 10.0)).is_err());
    }

    #[test]
    fn an_island_in_the_dom_collection_is_refused() {
        let mut page = Islands::new(viewport());
        let mut wrong = island("dom", 0.0, 0.0, 10.0, 10.0);
        wrong.mode = RenderMode::Dom;
        let error = page
            .add(wrong)
            .expect_err("a Dom region is not a GPU island");
        assert!(error.contains("Gpu"), "{error}");
    }

    #[test]
    fn scrolling_is_clamped_to_the_content() {
        let island = island("scrollable", 0.0, 0.0, 200.0, 200.0)
            .with_content(LogicalSize::new(200.0, 1000.0));
        let max = island.max_scroll();
        assert_eq!(max.x, 0.0, "content is not wider than the viewport");
        assert_eq!(max.y, 800.0);

        // A fling past the end stops at the end rather than being refused.
        let overscrolled = island.scrolled_by(LogicalPoint::new(0.0, 5000.0));
        assert_eq!(overscrolled.scroll.y, 800.0, "clamped to the end");
        assert!(overscrolled.is_scrolled());

        // And back past the start stops at the start.
        let backwards = overscrolled.scrolled_by(LogicalPoint::new(0.0, -5000.0));
        assert_eq!(backwards.scroll.y, 0.0, "clamped to the start");
        assert!(!backwards.is_scrolled());
    }

    #[test]
    fn an_axis_that_does_not_scroll_does_not_jitter() {
        let island =
            island("wide", 0.0, 0.0, 400.0, 200.0).with_content(LogicalSize::new(400.0, 200.0));
        let scrolled = island.scrolled_by(LogicalPoint::new(50.0, 50.0));
        assert_eq!(scrolled.scroll, LogicalPoint::new(0.0, 0.0));
    }

    #[test]
    fn a_window_point_maps_into_content_coordinates() {
        let island = island("s", 100.0, 50.0, 200.0, 200.0)
            .with_content(LogicalSize::new(200.0, 900.0))
            .scrolled_by(LogicalPoint::new(0.0, 100.0));

        // Inside: the window offset plus the scroll.
        let content = island
            .content_point(LogicalPoint::new(150.0, 100.0))
            .expect("inside");
        assert_eq!(content, LogicalPoint::new(50.0, 150.0));

        // Outside: no mapping, rather than a clamped one that would deliver a click to
        // content the user did not point at.
        assert!(
            island
                .content_point(LogicalPoint::new(10.0, 10.0))
                .is_none()
        );
        assert!(
            island
                .content_point(LogicalPoint::new(1000.0, 10.0))
                .is_none()
        );
    }

    #[test]
    fn high_dpi_conversion_round_trips() {
        for scale in [1.0, 1.25, 1.5, 2.0, 3.0] {
            let viewport = Viewport::new(LogicalSize::new(800.0, 600.0), scale);
            let point = LogicalPoint::new(123.5, 456.25);
            let round = viewport.to_logical(viewport.to_physical(point));
            assert!(
                (round.x - point.x).abs() < 0.01 && (round.y - point.y).abs() < 0.01,
                "scale {scale} did not round trip: {point} -> {round}"
            );
        }
    }

    #[test]
    fn the_scale_factor_lives_on_the_viewport_not_the_island() {
        // Two islands scaled differently cannot share a surface, so the ratio is a display
        // property. This asserts it is reachable from exactly one place.
        let viewport = Viewport::new(LogicalSize::new(1920.0, 1080.0), 2.0);
        assert_eq!(viewport.physical_size(), LogicalSize::new(3840.0, 2160.0));

        let island = Island::new("only", Rect::new(0.0, 0.0, 100.0, 100.0));
        let scale_fields = format!("{island:?}");
        assert!(
            !scale_fields.contains("scale"),
            "an island must not carry its own scale factor: {scale_fields}"
        );
    }

    #[test]
    fn a_useless_scale_factor_falls_back_to_one() {
        // A zero or NaN ratio would collapse every conversion; a renderer that silently
        // draws nothing is worse than one that assumes a 1:1 display.
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(
                Viewport::new(LogicalSize::new(100.0, 100.0), bad).scale_factor,
                1.0
            );
        }
    }

    #[test]
    fn islands_can_be_replaced_in_place() {
        // Scrolling updates an island without changing composition order, which is what keeps
        // a scroll from re-ordering the page.
        let mut page = Islands::new(viewport());
        page.add(island("a", 0.0, 0.0, 100.0, 100.0))
            .expect("added");
        page.add(island("b", 200.0, 0.0, 100.0, 100.0))
            .expect("added");

        let scrolled = page.all()[0]
            .clone()
            .with_content(LogicalSize::new(100.0, 900.0))
            .scrolled_by(LogicalPoint::new(0.0, 100.0));
        page.replace(scrolled).expect("replaced");

        assert_eq!(page.all()[0].id, "a", "order is unchanged");
        assert_eq!(page.all()[0].scroll.y, 100.0, "the scroll was applied");
        assert!(page.replace(island("absent", 0.0, 0.0, 1.0, 1.0)).is_err());
    }

    #[test]
    fn render_mode_names_are_stable() {
        // These appear in metrics and in the DOM/GPU boundary, so renaming one is a breaking
        // change to an observable.
        assert_eq!(RenderMode::Gpu.to_string(), "gpu");
        assert_eq!(RenderMode::Dom.as_str(), "dom");
        assert_eq!(RenderMode::Native.as_str(), "native");
    }
}
