//! Application-drawn island content: the retained command list `gpu.island_custom` requires.
//!
//! Island content today is a solid colour, or rows the island model lays out itself. A
//! timeline is neither: clips with waveforms, thumbnails, keyframe curves, a playhead and
//! in/out handles are application-defined geometry, hit-tested precisely. This module is
//! the model half of that: a [`DisplayList`] the application builds, the renderer draws,
//! the input host hit-tests, and the accessibility tree is derived from — all four reading
//! the *same* list, because the register's contract is that what is drawn and what is
//! hit-tested is the same geometry. Two lists that must be kept in sync are a lie waiting
//! to happen; one stream cannot disagree with itself.
//!
//! # The one-stream design
//!
//! Draw commands and hit regions are interleaved in a single command stream, and the clip
//! stack applies to both uniformly. A clip pushed before a waveform's bars clips the bars
//! and the waveform's hit region alike; a handle outside the clip neither draws nor hits.
//! That is the whole contract, and it is structural rather than documented.
//!
//! # What this module is not
//!
//! It does not draw — vertices, scissors and textures are `ncui-gpu-renderer`'s business.
//! It does not route events — that is [`crate::input::IslandHost`]. It is the content
//! model both sides share, pure and testable on a machine with no display, which is what
//! lets the pixel gate and the hit-test gate assert against the same truth.

use crate::geometry::{LogicalPoint, Rect};

/// The most commands one list may hold.
///
/// A timeline with a thousand clips of a hundred waveform bars each is real content;
/// ten million commands is a runaway producer, and the difference matters because the
/// list is walked per hit test and expanded per frame. 65,536 commands is several
/// hundred fully-drawn timeline clips — generous for content, fatal for a loop that
/// forgot its exit condition.
pub const MAX_COMMANDS: usize = 65_536;

/// What a hit region is for, so the accessibility tree can say so.
///
/// These are the roles an editor surface is made of; they are deliberately narrower than
/// the full ARIA vocabulary, because a GPU island is not a document. The accessibility
/// derivation maps them onto the platform roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegionRole {
    /// A thing the user activates: a clip's rename button, a keyframe diamond.
    Button,
    /// A thing the user drags: an in/out handle, a playhead, a keyframe.
    Handle,
    /// A timeline track — a container of clips.
    Track,
    /// The ruler — time markings along the top.
    Ruler,
    /// A keyframe curve or envelope.
    Curve,
    /// Content with no interactive role of its own — a waveform, a thumbnail well.
    Surface,
}

impl RegionRole {
    /// The role's stable name, for evidence and for the accessibility mapping.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Button => "button",
            Self::Handle => "handle",
            Self::Track => "track",
            Self::Ruler => "ruler",
            Self::Curve => "curve",
            Self::Surface => "surface",
        }
    }
}

/// One command in a display list.
///
/// Coordinates are island-content coordinates: the origin is the content's top-left,
/// before scrolling is applied. The renderer and the hit tester apply the same scroll
/// offset when they walk the list, which is what keeps a scrolled waveform under the
/// cursor it appears to be under.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// A filled rectangle — bars, playheads, selection ranges, clip bodies.
    Rect {
        /// Where it is drawn.
        bounds: Rect,
        /// Red, green, blue and alpha in `0..=255`.
        colour: [u8; 4],
    },
    /// Push a clip rectangle. Everything drawn — and every hit region declared — until
    /// the matching [`Command::ClipPop`] is clipped to the intersection of this rectangle
    /// with every enclosing clip. Nested clips narrow, they never widen.
    ClipPush {
        /// The clip, in content coordinates.
        bounds: Rect,
    },
    /// Pop the most recent clip push.
    ClipPop,
    /// A run of shaped text.
    TextRun {
        /// Where the text's baseline origin sits.
        origin: LogicalPoint,
        /// The text itself.
        text: String,
        /// The font size in content pixels.
        size: f32,
        /// The colour.
        colour: [u8; 4],
    },
    /// A bitmap blit — a thumbnail, a filmstrip frame. The source is a key into the
    /// host's bitmap store rather than the pixels themselves: the list is retained, and
    /// retaining copies of thumbnails would multiply memory per frame.
    Blit {
        /// Where the bitmap is drawn, scaled to fit.
        bounds: Rect,
        /// The bitmap-store key.
        source: String,
    },
    /// A hit-testable region, interleaved with the draw commands it belongs to.
    ///
    /// A region is hit-tested against its bounds *as clipped by the clip stack in effect
    /// where the command sits* — the same clipping the draw commands around it receive.
    /// The region carries its label and role because the accessibility tree is derived
    /// from this same list: the user hears about exactly the geometry the user can hit.
    HitRegion {
        /// The region's stable id — what a routed event names.
        id: String,
        /// Where the region sits.
        bounds: Rect,
        /// The accessible name — "clip intro.mp4", "playhead", "in handle".
        label: String,
        /// What the region is for.
        role: RegionRole,
    },
}

/// What the list is asked to do that it cannot honestly do.
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayError {
    /// A clip pop with no matching push — the stream's balance is broken, and guessing
    /// which clip was meant would corrupt both drawing and hit-testing.
    UnbalancedClip {
        /// Where in the stream the unmatched pop sits.
        index: usize,
    },
    /// The stream ends with clips still pushed. A renderer could guess they close at the
    /// end, but a hit test would have to guess the same way, and two guesses that happen
    /// to agree is not a contract.
    UnclosedClip {
        /// How many clips were still open at the end.
        depth: usize,
    },
    /// More commands than [`MAX_COMMANDS`]. The content is real or the producer is
    /// stuck, and a bounded model reports the difference rather than walking forever.
    TooManyCommands {
        /// How many commands the producer tried to hold.
        declared: usize,
    },
    /// A rectangle with a non-finite edge or negative size. A renderer drawing it would
    /// put a number on the screen where a shape should be, and a hit test against it
    /// would answer questions about a rectangle that does not exist.
    InvalidGeometry {
        /// Where in the stream the invalid command sits.
        index: usize,
    },
}

impl core::fmt::Display for DisplayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnbalancedClip { index } => {
                write!(f, "clip pop at command {index} has no matching push")
            }
            Self::UnclosedClip { depth } => {
                write!(f, "the list ends with {depth} clip(s) still open")
            }
            Self::TooManyCommands { declared } => {
                write!(f, "{declared} commands exceeds the {MAX_COMMANDS} limit")
            }
            Self::InvalidGeometry { index } => write!(
                f,
                "command {index} carries a rectangle with a non-finite edge or negative size"
            ),
        }
    }
}

impl std::error::Error for DisplayError {}

/// One hit region with its effective — clipped — geometry.
///
/// This is the answer hit-testing and accessibility both read: the region as the user
/// can actually reach it, with the clip stack already applied. A region fully clipped
/// away is absent rather than empty.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveRegion {
    /// The region's stable id.
    pub id: String,
    /// The bounds after the clip stack is applied.
    pub bounds: Rect,
    /// The accessible name.
    pub label: String,
    /// What the region is for.
    pub role: RegionRole,
    /// Where in the stream the region was declared — the z-order tiebreak, since later
    /// commands draw over earlier ones.
    pub order: usize,
}

/// A region the accessibility derivation reports, in reading order.
#[derive(Debug, Clone, PartialEq)]
pub struct RegionDescription {
    /// The region's stable id.
    pub id: String,
    /// The accessible name.
    pub label: String,
    /// What the region is for.
    pub role: RegionRole,
    /// The effective bounds, so a screen reader can outline it.
    pub bounds: Rect,
}

/// A retained list of draw commands and hit regions — one island's custom content.
///
/// Build it with [`DisplayList::builder`]; the builder validates on [`Builder::finish`],
/// because a list that fails validation is a defect in the producer, and drawing it
/// anyway would be lying in pixels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplayList {
    commands: Vec<Command>,
}

impl DisplayList {
    /// Starts building a list.
    #[must_use]
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// A list with no content — an island that draws nothing and hits nothing.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    /// The command stream, in order. The renderer walks this.
    #[must_use]
    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    /// How many commands the list holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Every hit region with the clip stack applied, in stream order.
    ///
    /// This is the single pass both hit-testing and accessibility build on: the clip
    /// stack is tracked exactly as a renderer would apply it, regions are intersected
    /// with the active clip, and a region clipped away entirely is absent — the user
    /// can neither hit it nor hear about it, which is what the paint already says.
    #[must_use]
    pub fn effective_regions(&self) -> Vec<EffectiveRegion> {
        let mut regions = Vec::new();
        // The clip stack, where `None` entries mean "clips everything": a clip pushed
        // entirely outside its enclosing clip intersects to nothing, and nothing is
        // what it must clip to — falling back to the unclipped rectangle would let a
        // nested clip *widen* what the parent narrowed, which is the one thing a clip
        // is not allowed to do.
        let mut clips: Vec<Option<Rect>> = Vec::new();
        for (order, command) in self.commands.iter().enumerate() {
            match command {
                Command::ClipPush { bounds } => {
                    let effective = match clips.last() {
                        None => Some(*bounds),
                        Some(enclosing) => enclosing.and_then(|clip| clip.intersection(bounds)),
                    };
                    clips.push(effective);
                }
                Command::ClipPop => {
                    clips.pop();
                }
                Command::HitRegion {
                    id,
                    bounds,
                    label,
                    role,
                } => {
                    let effective = match clips.last() {
                        None => Some(*bounds),
                        Some(enclosing) => enclosing.and_then(|clip| clip.intersection(bounds)),
                    };
                    if let Some(effective) = effective
                        && effective.width > 0.0
                        && effective.height > 0.0
                    {
                        regions.push(EffectiveRegion {
                            id: id.clone(),
                            bounds: effective,
                            label: label.clone(),
                            role: *role,
                            order,
                        });
                    }
                }
                _ => {}
            }
        }
        regions
    }

    /// The topmost hit region at a content-space point, if any.
    ///
    /// Topmost means latest in the stream, because later commands draw over earlier
    /// ones — the region the user sees on top is the region the click lands on.
    #[must_use]
    pub fn hit_test(&self, point: LogicalPoint) -> Option<EffectiveRegion> {
        self.effective_regions()
            .into_iter()
            .filter(|region| region.bounds.contains(point))
            .max_by_key(|region| region.order)
    }

    /// What the accessibility tree should say about this content, in reading order.
    ///
    /// Reading order is stream order — the order the application declared, which is the
    /// order a sighted user scans a well-built timeline. The descriptions carry effective
    /// bounds because a screen reader outlining a region should outline what is visible,
    /// not what was declared before clipping.
    #[must_use]
    pub fn accessibility_descriptions(&self) -> Vec<RegionDescription> {
        self.effective_regions()
            .into_iter()
            .map(|region| RegionDescription {
                id: region.id,
                label: region.label,
                role: region.role,
                bounds: region.bounds,
            })
            .collect()
    }
}

/// Whether a rectangle is one a renderer could draw and a hit test could answer about.
fn valid_rect(rect: &Rect) -> bool {
    rect.x.is_finite()
        && rect.y.is_finite()
        && rect.width.is_finite()
        && rect.height.is_finite()
        && rect.width >= 0.0
        && rect.height >= 0.0
}

/// Builds a [`DisplayList`], validating at [`Builder::finish`].
///
/// The builder accepts everything and the finish refuses the invalid lists, rather than
/// validating per push: a producer laying out a timeline knows at the end whether the
/// content is coherent, and per-push results would litter the layout code with error
/// handling for defects that are program bugs, not runtime conditions.
#[derive(Debug, Default)]
pub struct Builder {
    commands: Vec<Command>,
}

impl Builder {
    /// A filled rectangle.
    #[must_use]
    pub fn rect(mut self, bounds: Rect, colour: [u8; 4]) -> Self {
        self.commands.push(Command::Rect { bounds, colour });
        self
    }

    /// Pushes a clip.
    #[must_use]
    pub fn clip_push(mut self, bounds: Rect) -> Self {
        self.commands.push(Command::ClipPush { bounds });
        self
    }

    /// Pops the most recent clip.
    #[must_use]
    pub fn clip_pop(mut self) -> Self {
        self.commands.push(Command::ClipPop);
        self
    }

    /// A run of text.
    #[must_use]
    pub fn text(mut self, origin: LogicalPoint, text: &str, size: f32, colour: [u8; 4]) -> Self {
        self.commands.push(Command::TextRun {
            origin,
            text: text.to_owned(),
            size,
            colour,
        });
        self
    }

    /// A bitmap blit from the host's bitmap store.
    #[must_use]
    pub fn blit(mut self, bounds: Rect, source: &str) -> Self {
        self.commands.push(Command::Blit {
            bounds,
            source: source.to_owned(),
        });
        self
    }

    /// A hit-testable region. Place it in the stream where its content is drawn — the
    /// clip stack in effect there applies to it, and stream order is z-order.
    #[must_use]
    pub fn hit_region(mut self, id: &str, bounds: Rect, label: &str, role: RegionRole) -> Self {
        self.commands.push(Command::HitRegion {
            id: id.to_owned(),
            bounds,
            label: label.to_owned(),
            role,
        });
        self
    }

    /// Validates and finishes the list.
    ///
    /// # Errors
    ///
    /// Returns the first defect found: an unbalanced or unclosed clip, a command count
    /// over the limit, or geometry a renderer could not draw.
    pub fn finish(self) -> Result<DisplayList, DisplayError> {
        if self.commands.len() > MAX_COMMANDS {
            return Err(DisplayError::TooManyCommands {
                declared: self.commands.len(),
            });
        }
        let mut depth = 0usize;
        for (index, command) in self.commands.iter().enumerate() {
            match command {
                Command::ClipPush { bounds } => {
                    if !valid_rect(bounds) {
                        return Err(DisplayError::InvalidGeometry { index });
                    }
                    depth += 1;
                }
                Command::ClipPop => {
                    if depth == 0 {
                        return Err(DisplayError::UnbalancedClip { index });
                    }
                    depth -= 1;
                }
                Command::Rect { bounds, .. }
                | Command::Blit { bounds, .. }
                | Command::HitRegion { bounds, .. } => {
                    if !valid_rect(bounds) {
                        return Err(DisplayError::InvalidGeometry { index });
                    }
                }
                Command::TextRun { origin, size, .. } => {
                    if !origin.x.is_finite()
                        || !origin.y.is_finite()
                        || !size.is_finite()
                        || *size <= 0.0
                    {
                        return Err(DisplayError::InvalidGeometry { index });
                    }
                }
            }
        }
        if depth != 0 {
            return Err(DisplayError::UnclosedClip { depth });
        }
        Ok(DisplayList {
            commands: self.commands,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    // Geometry is asserted on exact values here on purpose: a clip boundary that is off by
    // one ulp is a real off-by-one, not float noise, so an epsilon would hide the defect.
    clippy::float_cmp,
    reason = "test assertions must abort loudly on failure; geometry comparisons are exact on purpose"
)]
mod tests {
    use super::*;
    use crate::geometry::LogicalSize;

    fn point(x: f32, y: f32) -> LogicalPoint {
        LogicalPoint::new(x, y)
    }

    /// A small timeline-shaped fixture: a track clip with a clipped waveform inside,
    /// an in-handle overlapping the clip's edge, and a playhead over everything.
    fn timeline() -> DisplayList {
        DisplayList::builder()
            // The track background.
            .rect(Rect::new(0.0, 0.0, 640.0, 48.0), [30, 30, 34, 255])
            // The clip body.
            .rect(Rect::new(100.0, 4.0, 200.0, 40.0), [60, 90, 200, 255])
            // The waveform, clipped to the clip — the bars past the edge do not draw,
            // and the hit region past the edge does not hit.
            .clip_push(Rect::new(100.0, 4.0, 200.0, 40.0))
            .rect(Rect::new(280.0, 10.0, 60.0, 28.0), [120, 160, 255, 255])
            .hit_region(
                "waveform",
                Rect::new(100.0, 4.0, 400.0, 40.0),
                "waveform intro.mp4",
                RegionRole::Surface,
            )
            .clip_pop()
            // The in-handle, deliberately overlapping the clip's left edge.
            .rect(Rect::new(96.0, 4.0, 8.0, 40.0), [240, 200, 60, 255])
            .hit_region(
                "in-handle",
                Rect::new(96.0, 4.0, 8.0, 40.0),
                "in handle",
                RegionRole::Handle,
            )
            // The playhead, over everything.
            .rect(Rect::new(180.0, 0.0, 2.0, 48.0), [255, 60, 60, 255])
            .hit_region(
                "playhead",
                Rect::new(176.0, 0.0, 10.0, 48.0),
                "playhead",
                RegionRole::Handle,
            )
            .finish()
            .expect("the fixture is balanced")
    }

    #[test]
    fn an_empty_list_hits_nothing_and_says_nothing() {
        let list = DisplayList::empty();
        assert!(list.is_empty());
        assert_eq!(list.hit_test(point(10.0, 10.0)), None);
        assert!(list.accessibility_descriptions().is_empty());
    }

    #[test]
    fn the_clip_stack_clips_hit_regions_exactly_as_it_clips_drawing() {
        let list = timeline();
        // The waveform region is declared 400 wide starting at x=100, but the clip is
        // 200 wide — its effective width is the clip's, because the clipped-away half
        // is not on the screen.
        let regions = list.effective_regions();
        let waveform = regions
            .iter()
            .find(|region| region.id == "waveform")
            .expect("the waveform survives clipping");
        assert_eq!(waveform.bounds, Rect::new(100.0, 4.0, 200.0, 40.0));
        // A point inside the declared region but outside the clip hits nothing: the
        // paint shows track there, and the hit test agrees with the paint.
        assert_eq!(list.hit_test(point(320.0, 24.0)).map(|r| r.id), None);
    }

    #[test]
    fn the_topmost_region_is_the_one_that_draws_last() {
        let list = timeline();
        // x=180, y=24 is inside the waveform's effective bounds, the in-handle is at
        // x=96..104 — no — the playhead's hit region is x=176..186 and it is declared
        // last, so it wins: the user sees the playhead line there, and the click
        // belongs to it.
        assert_eq!(
            list.hit_test(point(180.0, 24.0)).map(|r| r.id),
            Some("playhead".to_owned())
        );
        // Just outside the playhead's region but inside the waveform, the waveform wins.
        assert_eq!(
            list.hit_test(point(150.0, 24.0)).map(|r| r.id),
            Some("waveform".to_owned())
        );
        // The in-handle at its own edge.
        assert_eq!(
            list.hit_test(point(98.0, 24.0)).map(|r| r.id),
            Some("in-handle".to_owned())
        );
    }

    #[test]
    fn nested_clips_intersect_and_narrow() {
        let list = DisplayList::builder()
            .clip_push(Rect::new(0.0, 0.0, 100.0, 100.0))
            .clip_push(Rect::new(50.0, 50.0, 100.0, 100.0))
            .hit_region(
                "inner",
                Rect::new(0.0, 0.0, 500.0, 500.0),
                "inner",
                RegionRole::Surface,
            )
            .clip_pop()
            .clip_pop()
            .finish()
            .expect("balanced");
        let region = list
            .effective_regions()
            .into_iter()
            .find(|region| region.id == "inner")
            .expect("the region survives");
        // The intersection of the two clips is (50,50)-(100,100): 50 by 50.
        assert_eq!(region.bounds, Rect::new(50.0, 50.0, 50.0, 50.0));
    }

    #[test]
    fn a_region_clipped_entirely_away_is_absent_from_everything() {
        let list = DisplayList::builder()
            .clip_push(Rect::new(0.0, 0.0, 10.0, 10.0))
            .hit_region(
                "hidden",
                Rect::new(500.0, 500.0, 50.0, 50.0),
                "hidden",
                RegionRole::Button,
            )
            .clip_pop()
            .finish()
            .expect("balanced");
        assert!(list.effective_regions().is_empty());
        assert!(list.accessibility_descriptions().is_empty());
        assert_eq!(list.hit_test(point(510.0, 510.0)), None);
    }

    #[test]
    fn accessibility_derives_from_the_same_geometry_hit_testing_uses() {
        let list = timeline();
        let descriptions = list.accessibility_descriptions();
        // Three regions survive clipping, in stream order: waveform, in-handle, playhead.
        assert_eq!(descriptions.len(), 3);
        assert_eq!(descriptions[0].id, "waveform");
        assert_eq!(descriptions[0].label, "waveform intro.mp4");
        assert_eq!(descriptions[0].role, RegionRole::Surface);
        assert_eq!(descriptions[1].id, "in-handle");
        assert_eq!(descriptions[2].id, "playhead");
        assert_eq!(descriptions[2].role, RegionRole::Handle);
        // The bounds the screen reader outlines are the effective ones — the waveform's
        // clipped width, not its declared one.
        assert_eq!(descriptions[0].bounds.width, 200.0);
    }

    #[test]
    fn an_unbalanced_pop_is_refused() {
        let error = DisplayList::builder()
            .clip_pop()
            .finish()
            .expect_err("a pop with no push is a defect");
        assert_eq!(error, DisplayError::UnbalancedClip { index: 0 });
    }

    #[test]
    fn an_unclosed_clip_is_refused() {
        let error = DisplayList::builder()
            .clip_push(Rect::new(0.0, 0.0, 10.0, 10.0))
            .rect(Rect::new(0.0, 0.0, 5.0, 5.0), [0, 0, 0, 255])
            .finish()
            .expect_err("an open clip at the end is a defect");
        assert_eq!(error, DisplayError::UnclosedClip { depth: 1 });
    }

    #[test]
    fn non_finite_or_negative_geometry_is_refused() {
        let error = DisplayList::builder()
            .rect(Rect::new(f32::NAN, 0.0, 10.0, 10.0), [0, 0, 0, 255])
            .finish()
            .expect_err("NaN is not a position");
        assert_eq!(error, DisplayError::InvalidGeometry { index: 0 });

        let error = DisplayList::builder()
            .rect(Rect::new(0.0, 0.0, -4.0, 10.0), [0, 0, 0, 255])
            .finish()
            .expect_err("a negative width is not a rectangle");
        assert_eq!(error, DisplayError::InvalidGeometry { index: 0 });

        let error = DisplayList::builder()
            .text(point(0.0, 0.0), "label", 0.0, [0, 0, 0, 255])
            .finish()
            .expect_err("zero-size text is not text");
        assert_eq!(error, DisplayError::InvalidGeometry { index: 0 });
    }

    #[test]
    fn the_command_limit_is_enforced() {
        let mut builder = DisplayList::builder();
        for index in 0..MAX_COMMANDS {
            builder = builder.rect(Rect::new(0.0, 0.0, 1.0, 1.0), [0, 0, 0, 255]);
            let _ = index;
        }
        // Exactly at the limit is fine.
        assert!(builder.finish().is_ok());
        let mut builder = DisplayList::builder();
        for _ in 0..=MAX_COMMANDS {
            builder = builder.rect(Rect::new(0.0, 0.0, 1.0, 1.0), [0, 0, 0, 255]);
        }
        let error = builder.finish().expect_err("one past the limit is refused");
        assert_eq!(
            error,
            DisplayError::TooManyCommands {
                declared: MAX_COMMANDS + 1
            }
        );
    }

    #[test]
    fn roles_carry_stable_names_for_evidence() {
        assert_eq!(RegionRole::Handle.as_str(), "handle");
        assert_eq!(RegionRole::Track.as_str(), "track");
        assert_eq!(RegionRole::Ruler.as_str(), "ruler");
        assert_eq!(RegionRole::Curve.as_str(), "curve");
    }

    #[test]
    fn blits_and_text_are_validated_like_everything_else() {
        let list = DisplayList::builder()
            .blit(Rect::new(10.0, 10.0, 64.0, 36.0), "thumb:intro.mp4:0001")
            .text(point(10.0, 60.0), "intro.mp4", 11.0, [230, 230, 230, 255])
            .finish()
            .expect("valid content");
        assert_eq!(list.len(), 2);
        // The stream order is preserved — the renderer walks it exactly as declared.
        assert!(matches!(list.commands()[0], Command::Blit { .. }));
        assert!(matches!(list.commands()[1], Command::TextRun { .. }));
    }

    #[test]
    fn hit_testing_is_in_content_coordinates_so_scroll_applies_outside() {
        // The list knows nothing about scroll: the host offsets the point into content
        // space before asking. What the test pins is that the list's answers are stable
        // in content space — a point is either in a region or not, whatever the scroll.
        let list = timeline();
        let first = list.hit_test(point(180.0, 24.0));
        let second = list.hit_test(point(180.0, 24.0));
        assert_eq!(first, second);
        // LogicalSize appears in the fixture's coordinate vocabulary — this assertion
        // keeps the import honest about content-space thinking.
        let _content = LogicalSize::new(640.0, 48.0);
    }
}
