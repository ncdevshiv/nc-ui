//! Turning a glyph's outline into the coverage a renderer can draw.
//!
//! A font describes a letter as contours of straight lines and curves. A screen has pixels. The
//! gap between them is a rasteriser, and this is it: it flattens the curves, works out which
//! pixels the filled shape covers, and produces one byte of coverage per pixel.
//!
//! # Why coverage and not a bitmap
//!
//! A glyph is drawn in whatever colour the caller wants, so what is rasterised is *how much* of
//! each pixel the letter covers, not the letter's colour. One coverage map can then be drawn in
//! black, in white, or over a background, without rasterising again.
//!
//! # The fill rule
//!
//! TrueType outlines are filled by the **non-zero winding rule**: a point is inside when a ray
//! from it crosses more contour edges one way than the other. That rule is what makes the hole of
//! an `o` a hole — the inner contour is wound the opposite way from the outer one — and it is why
//! this module counts crossings with a direction rather than simply asking whether a point is
//! enclosed. An even-odd fill would draw the `o` solid.
//!
//! # Anti-aliasing
//!
//! Each pixel is sampled on a grid of `SAMPLES` by `SAMPLES` points and the coverage is the
//! fraction of those points inside the shape. Sixteen samples a pixel is not analytic coverage,
//! but it is honest coverage: an edge that crosses a pixel is drawn at the fraction of the pixel
//! it really occupies, and the result is smooth to the eye without a second code path for the
//! pixels that fall exactly on an edge.
//!
//! # Coordinates
//!
//! The outline arrives in the font's own space, which has its origin at the glyph's baseline and
//! its y axis pointing **up**. The coverage map is produced top row first, because that is the
//! order every image format and every pixel buffer in this workspace uses. The map records the
//! outline coordinate of its top-left corner, so a caller can put it back where the glyph was.

use core::fmt;

/// How many samples are taken along each axis of a pixel.
///
/// Four by four: sixteen coverage levels, which is enough that a diagonal edge reads as a line
/// rather than as a staircase, and cheap enough that a screen of text is a few milliseconds.
const SAMPLES: usize = 4;

/// The largest coverage map this will produce, in pixels.
///
/// A bound because the outline is font data: a glyph whose outline claims to be a million pixels
/// wide is either a font this renderer should not be drawing or a file that is lying, and either
/// way the answer is a refusal rather than an allocation.
pub const MAX_COVERAGE_PIXELS: u32 = 4_194_304;

/// One point of a glyph's outline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutlinePoint {
    /// The x coordinate, in the outline's own units.
    pub x: f32,
    /// The y coordinate, in the outline's own units.
    pub y: f32,
    /// Whether the point is on the curve, or is a control point of a curve that passes near it.
    pub on_curve: bool,
}

impl OutlinePoint {
    /// A point on the curve.
    #[must_use]
    pub const fn on_curve(x: f32, y: f32) -> Self {
        Self {
            x,
            y,
            on_curve: true,
        }
    }

    /// A control point, which the curve approaches but does not pass through.
    #[must_use]
    pub const fn control(x: f32, y: f32) -> Self {
        Self {
            x,
            y,
            on_curve: false,
        }
    }
}

/// One closed contour of a glyph's outline.
///
/// A letter can have several — an `o` has two, an `i` has two — and which is a hole and which is
/// the letter is decided by the direction each one is wound in, not by the order they arrive in.
#[derive(Debug, Clone, PartialEq)]
pub struct Contour {
    /// The contour's points, in the order the font draws them. The first is where the pen went
    /// down; the contour closes back to it.
    pub points: Vec<OutlinePoint>,
}

impl Contour {
    /// A contour from its points.
    #[must_use]
    pub fn new(points: Vec<OutlinePoint>) -> Self {
        Self { points }
    }
}

/// A glyph's coverage: how much of each pixel the glyph fills.
#[derive(Debug, Clone, PartialEq)]
pub struct Coverage {
    /// The width of the map, in pixels.
    pub width: u32,
    /// The height of the map, in pixels.
    pub height: u32,
    /// Where the map's top-left pixel is, in the outline's own coordinates.
    pub origin_x: f32,
    /// Where the map's top-left pixel is, in the outline's own coordinates.
    pub origin_y: f32,
    /// The coverage, one byte per pixel, from the top row down.
    ///
    /// Zero is "the glyph does not touch this pixel" and 255 is "the glyph fills it".
    pub alpha: Vec<u8>,
}

impl Coverage {
    /// The coverage of one pixel, or `None` when the pixel is outside the map.
    #[must_use]
    pub fn at(&self, x: u32, y: u32) -> Option<u8> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = usize::try_from(y).ok()? * usize::try_from(self.width).ok()?
            + usize::try_from(x).ok()?;
        self.alpha.get(index).copied()
    }

    /// Whether the map covers no pixel at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.alpha.iter().all(|value| *value == 0)
    }

    /// Whether the map is the size its dimensions say it is.
    #[must_use]
    pub fn is_whole(&self) -> bool {
        usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .map(|height| width * height)
            })
            .is_some_and(|pixels| pixels == self.alpha.len())
    }
}

/// Why an outline could not be turned into coverage.
#[derive(Debug, Clone, PartialEq)]
pub enum GlyphError {
    /// The outline holds no contour with an area.
    Empty,
    /// The outline is larger than [`MAX_COVERAGE_PIXELS`].
    TooLarge {
        /// The width the outline claims.
        width: f32,
        /// The height the outline claims.
        height: f32,
    },
}

impl fmt::Display for GlyphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the outline encloses no area, so it covers no pixel"),
            Self::TooLarge { width, height } => write!(
                f,
                "an outline of {width}x{height} pixels is larger than the {MAX_COVERAGE_PIXELS} a \
                 coverage map may hold"
            ),
        }
    }
}

impl std::error::Error for GlyphError {}

/// A point of a flattened polygon.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Point {
    x: f32,
    y: f32,
}

/// Rasterises an outline into coverage.
///
/// # Errors
///
/// Returns [`GlyphError::Empty`] when the outline encloses no area — a space, or a contour that
/// collapsed to a point — and [`GlyphError::TooLarge`] when the map would be larger than
/// [`MAX_COVERAGE_PIXELS`].
pub fn rasterise(contours: &[Contour]) -> Result<Coverage, GlyphError> {
    let polygons: Vec<Vec<Point>> = contours
        .iter()
        .map(flatten)
        .filter(|polygon| polygon.len() >= 3)
        .collect();
    let Some((min_x, min_y, max_x, max_y)) = bounds(&polygons) else {
        return Err(GlyphError::Empty);
    };
    let width = (max_x - min_x).ceil();
    let height = (max_y - min_y).ceil();
    if !(width.is_finite() && height.is_finite()) || width <= 0.0 || height <= 0.0 {
        return Err(GlyphError::Empty);
    }
    // The bound is checked before the allocation, not after: the dimensions came from a font file.
    let pixels = width * height;
    if pixels > MAX_COVERAGE_PIXELS as f32 {
        return Err(GlyphError::TooLarge { width, height });
    }
    let Ok(width_px) = u32::try_from(width as u64) else {
        return Err(GlyphError::TooLarge { width, height });
    };
    let Ok(height_px) = u32::try_from(height as u64) else {
        return Err(GlyphError::TooLarge { width, height });
    };
    let mut alpha = vec![0_u8; usize::try_from(pixels as u64).unwrap_or(0)];
    let mut row = vec![0_u16; usize::try_from(width_px).unwrap_or(0)];
    // The y axis of the outline points up and the map's rows run down, so the first row is the
    // topmost strip of the glyph: the one just below `max_y`.
    for pixel_row in 0..height_px {
        row.fill(0);
        for sub_row in 0..SAMPLES {
            let y = max_y - (pixel_row as f32) - (sub_row as f32 + 0.5) / SAMPLES as f32;
            let crossings = crossings(&polygons, y);
            if crossings.is_empty() {
                continue;
            }
            fill_row(&crossings, min_x, width_px, &mut row);
        }
        let start =
            usize::try_from(pixel_row).unwrap_or(0) * usize::try_from(width_px).unwrap_or(0);
        let total = (SAMPLES * SAMPLES) as u16;
        for (column, count) in row.iter().enumerate() {
            let covered = u32::from(*count) * 255 / u32::from(total);
            if let Some(slot) = alpha.get_mut(start + column) {
                *slot = u8::try_from(covered).unwrap_or(255);
            }
        }
    }
    Ok(Coverage {
        width: width_px,
        height: height_px,
        origin_x: min_x,
        origin_y: max_y,
        alpha,
    })
}

/// Counts one pixel row's covered samples.
///
/// The crossings are sorted by x, so the samples are walked left to right with the winding count
/// carried along: a sample is inside when the number of edges crossed one way exceeds the number
/// crossed the other. That is the non-zero winding rule, and it is what leaves an `o` hollow.
fn fill_row(crossings: &[(f32, i32)], min_x: f32, width: u32, row: &mut [u16]) {
    let mut winding = 0_i32;
    let mut index = 0_usize;
    for column in 0..width {
        for sub in 0..SAMPLES {
            let x = min_x + column as f32 + (sub as f32 + 0.5) / SAMPLES as f32;
            while index < crossings.len() && crossings[index].0 <= x {
                winding += crossings[index].1;
                index += 1;
            }
            if winding != 0
                && let Some(count) = row.get_mut(column as usize)
            {
                *count = count.saturating_add(1);
            }
        }
    }
}

/// The edges a horizontal line at `y` crosses, and which way each one goes.
fn crossings(polygons: &[Vec<Point>], y: f32) -> Vec<(f32, i32)> {
    let mut found = Vec::new();
    for polygon in polygons {
        for index in 0..polygon.len() {
            let from = polygon[index];
            let to = polygon[(index + 1) % polygon.len()];
            // Half-open: an edge counts when it starts on or below the line and ends above it, or
            // the other way round. Counting both ends would double-count a vertex.
            if (from.y <= y) == (to.y <= y) {
                continue;
            }
            let span = to.y - from.y;
            if span == 0.0 {
                continue;
            }
            let x = from.x + (y - from.y) * (to.x - from.x) / span;
            found.push((x, if to.y > from.y { 1 } else { -1 }));
        }
    }
    found.sort_by(|left, right| left.0.total_cmp(&right.0));
    found
}

/// The smallest rectangle that holds every polygon, as `(min_x, min_y, max_x, max_y)`.
fn bounds(polygons: &[Vec<Point>]) -> Option<(f32, f32, f32, f32)> {
    let mut bounds: Option<(f32, f32, f32, f32)> = None;
    for polygon in polygons {
        for point in polygon {
            bounds = Some(match bounds {
                None => (point.x, point.y, point.x, point.y),
                Some((min_x, min_y, max_x, max_y)) => (
                    min_x.min(point.x),
                    min_y.min(point.y),
                    max_x.max(point.x),
                    max_y.max(point.y),
                ),
            });
        }
    }
    bounds.filter(|(min_x, min_y, max_x, max_y)| {
        min_x.is_finite() && min_y.is_finite() && max_x.is_finite() && max_y.is_finite()
    })
}

/// Turns one contour into a polygon of straight segments.
///
/// The font describes curves with control points, and consecutive control points imply a point on
/// the curve halfway between them — a compression the format uses to store a smooth curve with
/// fewer bytes. A flattener that ignored the implied points would cut every such curve short.
fn flatten(contour: &Contour) -> Vec<Point> {
    let points = &contour.points;
    let Some(first) = points.first().copied() else {
        return Vec::new();
    };
    // A contour whose first point is a control point starts at the implied point between it and
    // the last point, which is where the closing curve meets the opening one.
    let (start, skip) = if first.on_curve {
        (first, 1)
    } else {
        let last = points.last().copied().unwrap_or(first);
        (midpoint(last, first), 0)
    };
    let mut polygon = vec![Point {
        x: start.x,
        y: start.y,
    }];
    let mut current = start;
    let mut index = skip;
    while index < points.len() {
        let point = points[index];
        if point.on_curve {
            polygon.push(Point {
                x: point.x,
                y: point.y,
            });
            current = point;
            index += 1;
            continue;
        }
        match points.get(index + 1).copied() {
            Some(next) if next.on_curve => {
                flatten_quad(current, point, next, &mut polygon);
                current = next;
                index += 2;
            }
            Some(next) => {
                // Two control points in a row: the curve passes through the point between them.
                let implied = midpoint(point, next);
                flatten_quad(current, point, implied, &mut polygon);
                current = implied;
                index += 1;
            }
            None => {
                // A control point as the last point closes the contour back to its start.
                flatten_quad(current, point, start, &mut polygon);
                current = start;
                index += 1;
            }
        }
    }
    // The contour is closed by definition; a polygon that does not end where it began is closed
    // here so the edge walk sees the closing segment.
    if let Some(last) = polygon.last()
        && ((last.x - start.x).abs() > f32::EPSILON || (last.y - start.y).abs() > f32::EPSILON)
    {
        polygon.push(Point {
            x: start.x,
            y: start.y,
        });
    }
    polygon
}

/// The point halfway between two points.
fn midpoint(left: OutlinePoint, right: OutlinePoint) -> OutlinePoint {
    OutlinePoint::on_curve(left.x.midpoint(right.x), left.y.midpoint(right.y))
}

/// Appends the segments of a quadratic curve, from `from` through `control` to `to`.
fn flatten_quad(from: OutlinePoint, control: OutlinePoint, to: OutlinePoint, out: &mut Vec<Point>) {
    let steps = curve_steps(from, control, to);
    for step in 1..=steps {
        let t = step as f32 / steps as f32;
        let inverse = 1.0 - t;
        let x = inverse * inverse * from.x + 2.0 * inverse * t * control.x + t * t * to.x;
        let y = inverse * inverse * from.y + 2.0 * inverse * t * control.y + t * t * to.y;
        out.push(Point { x, y });
    }
}

/// How many straight segments a curve is drawn with.
///
/// Proportional to the square root of the curve's length: a long curve needs more segments to stay
/// within a pixel, and a short one needs few. The result is bounded at both ends so a degenerate
/// curve cannot ask for a million segments and a real one never looks like a polygon.
fn curve_steps(from: OutlinePoint, control: OutlinePoint, to: OutlinePoint) -> usize {
    let length = distance(from, control) + distance(control, to);
    if !length.is_finite() {
        return 2;
    }
    let steps = (length * 2.0).sqrt().ceil();
    if !steps.is_finite() {
        return 2;
    }
    // A curve shorter than a pixel still needs one segment; a long one is capped.
    (steps as usize).clamp(2, 64)
}

/// The distance between two points.
fn distance(left: OutlinePoint, right: OutlinePoint) -> f32 {
    let dx = right.x - left.x;
    let dy = right.y - left.y;
    (dx * dx + dy * dy).sqrt()
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    fn square(min_x: f32, min_y: f32, size: f32, clockwise: bool) -> Contour {
        let points = if clockwise {
            vec![
                OutlinePoint::on_curve(min_x, min_y),
                OutlinePoint::on_curve(min_x, min_y + size),
                OutlinePoint::on_curve(min_x + size, min_y + size),
                OutlinePoint::on_curve(min_x + size, min_y),
            ]
        } else {
            vec![
                OutlinePoint::on_curve(min_x, min_y),
                OutlinePoint::on_curve(min_x + size, min_y),
                OutlinePoint::on_curve(min_x + size, min_y + size),
                OutlinePoint::on_curve(min_x, min_y + size),
            ]
        };
        Contour::new(points)
    }

    #[test]
    fn a_square_is_covered_and_nothing_around_it_is() {
        let coverage = rasterise(&[square(0.0, 0.0, 10.0, false)]).expect("a square rasterises");
        assert_eq!((coverage.width, coverage.height), (10, 10));
        assert!(coverage.is_whole());
        assert_eq!(coverage.alpha.len(), 100);
        assert!(
            coverage.alpha.iter().all(|value| *value == 255),
            "every pixel of the square is fully covered"
        );
        assert_eq!(coverage.at(0, 0), Some(255));
        assert_eq!(coverage.at(10, 10), None, "outside the map is not a pixel");
    }

    #[test]
    fn a_hole_is_left_hollow_by_the_winding_rule() {
        // The defect this exists for: an outline whose inner contour is wound the other way is a
        // letter with a hole, and an even-odd or per-contour fill would draw the hole solid.
        let outer = square(0.0, 0.0, 10.0, false);
        let inner = square(3.0, 3.0, 4.0, true);
        let coverage = rasterise(&[outer, inner]).expect("an outline with a hole rasterises");
        assert_eq!((coverage.width, coverage.height), (10, 10));
        assert_eq!(coverage.at(1, 1), Some(255), "the ring is drawn");
        assert_eq!(coverage.at(5, 5), Some(0), "the hole is not drawn");
        assert_eq!(coverage.at(3, 3), Some(0), "the hole's corner is not drawn");
        assert_eq!(
            coverage.at(2, 5),
            Some(255),
            "the ring left of the hole is drawn"
        );
    }

    #[test]
    fn a_curve_is_flattened_and_covered() {
        // A half circle above the baseline: a quadratic from the left, through a control point
        // above the middle, to the right, closed along the baseline.
        let contour = Contour::new(vec![
            OutlinePoint::on_curve(0.0, 0.0),
            OutlinePoint::control(10.0, 20.0),
            OutlinePoint::on_curve(20.0, 0.0),
        ]);
        let coverage = rasterise(&[contour]).expect("a curve rasterises");
        assert!(coverage.width >= 19, "the curve spans its width");
        assert!(coverage.height >= 9, "the curve rises to its control point");
        assert!(!coverage.is_empty(), "a curve encloses an area");
        // The apex is under the control point, near the middle of the map.
        let middle = coverage.width / 2;
        assert!(
            coverage.at(middle, 0).unwrap_or(0) > 0,
            "the top of the curve is drawn at its middle"
        );
        // The shape is the area between the arc and the baseline, so the top-left pixel — above
        // the arc, which starts at the baseline — is outside it, and the bottom-left pixel, where
        // the arc crosses, is partly covered.
        assert_eq!(
            coverage.at(0, 0),
            Some(0),
            "above the arc is outside the shape"
        );
        let crossing = coverage.at(0, coverage.height - 1).unwrap_or(0);
        assert!(
            crossing > 0 && crossing < 255,
            "the pixel the arc crosses is partly covered, got {crossing}"
        );
    }

    #[test]
    fn two_control_points_in_a_row_imply_a_point_on_the_curve() {
        // The font stores a smooth curve as control points with no on-curve point between them.
        // A flattener that ignored the implied point would leave a notch in the outline.
        let contour = Contour::new(vec![
            OutlinePoint::on_curve(0.0, 0.0),
            OutlinePoint::control(0.0, 10.0),
            OutlinePoint::control(10.0, 10.0),
            OutlinePoint::on_curve(10.0, 0.0),
        ]);
        let coverage = rasterise(&[contour]).expect("a smooth curve rasterises");
        assert!(!coverage.is_empty());
        let middle = coverage.width / 2;
        assert!(
            coverage.at(middle, 1).unwrap_or(0) > 0,
            "the implied point is on the curve, so the top edge is smooth"
        );
    }

    #[test]
    fn an_outline_that_encloses_nothing_is_refused() {
        assert_eq!(rasterise(&[]), Err(GlyphError::Empty));
        // A single point is not a contour with an area.
        let point = Contour::new(vec![OutlinePoint::on_curve(1.0, 1.0)]);
        assert_eq!(rasterise(&[point]), Err(GlyphError::Empty));
    }

    #[test]
    fn an_outline_larger_than_a_coverage_map_may_be_is_refused() {
        let huge = square(0.0, 0.0, 4_096.0, false);
        let error = rasterise(&[huge]).expect_err("a huge outline is refused");
        assert!(matches!(error, GlyphError::TooLarge { .. }), "{error}");
    }

    #[test]
    fn the_map_records_where_it_sits_in_the_outline() {
        let coverage = rasterise(&[square(5.0, 7.0, 4.0, false)]).expect("a square rasterises");
        assert!((coverage.origin_x - 5.0).abs() < f32::EPSILON);
        assert!(
            (coverage.origin_y - 11.0).abs() < f32::EPSILON,
            "the top edge is the origin's y"
        );
    }
}
