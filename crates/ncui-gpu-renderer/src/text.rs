//! Text layout: shaping a string into positioned glyphs.
//!
//! ADR-002: *"Every GPU component needs a text layout path. We are not attempting to
//! out-engineer HarfBuzz/CoreText/DirectWrite; we integrate them."*
//!
//! This module integrates `HarfBuzz`, through `rustybuzz` — the maintained pure-Rust port of
//! it, which produces the same shaping results as the C library on every platform. That is
//! the point of integrating rather than re-implementing: shaping is where a text engine
//! is won or lost, and a hand-rolled approximation of it is wrong in ways that only show
//! up in scripts the author did not check.
//!
//! # What is and is not here
//!
//! * **Shaping** — ligatures, kerning, mark positioning, bidirectional reordering, script
//!   detection. All of it, because `rustybuzz` does all of it.
//! * **Line breaking** — not here. Breaking a paragraph into lines is a separate problem
//!   with its own rules, and a line-breaking algorithm that silently produces a wrong
//!   break is worse than one that refuses.
//! * **Glyph rasterisation** — in [`crate::glyphs`], which turns the outline this module
//!   produces into per-pixel coverage. The two are separate because the rasteriser is pure
//!   arithmetic over contours and is tested without a font, and because the coverage it
//!   produces is what a renderer draws, whatever colour the caller wants.
//!
//! # The safety boundary
//!
//! `rustybuzz` and `ttf-parser` are pure Rust and perform no FFI. This module's own code
//! is `unsafe`-free.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustybuzz::{
    Face as HbFace, UnicodeBuffer,
    ttf_parser::{Face as TtfFace, GlyphId, OutlineBuilder, name_id},
};

use crate::glyphs::{Contour, Coverage, OutlinePoint};

/// Why text could not be laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextError {
    /// The font file could not be read.
    Unreadable {
        /// The path that could not be read.
        path: PathBuf,
        /// Why it could not be read.
        reason: String,
    },
    /// The bytes are not a font this module can read.
    NotAFont {
        /// What the parser said.
        reason: String,
    },
    /// The font holds no glyphs at all.
    NoGlyphs,
    /// No font was found on this machine.
    NoFontAvailable,
    /// A glyph's outline cannot be drawn.
    NotDrawable {
        /// What the rasteriser said.
        reason: String,
    },
}

impl core::fmt::Display for TextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unreadable { path, reason } => {
                write!(f, "could not read the font at {}: {reason}", path.display())
            }
            Self::NotAFont { reason } => write!(f, "not a readable font: {reason}"),
            Self::NoGlyphs => f.write_str("the font holds no glyphs"),
            Self::NoFontAvailable => f.write_str("no font is available on this machine"),
            Self::NotDrawable { reason } => write!(f, "a glyph cannot be drawn: {reason}"),
        }
    }
}

impl std::error::Error for TextError {}

/// One glyph, positioned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    /// The glyph's index in the font.
    pub glyph_id: u16,
    /// The character the glyph belongs to.
    ///
    /// A cluster, not an index: one character can produce several glyphs, and several
    /// characters can collapse into one. The cluster is what a caller needs in order to
    /// map a glyph back to the text it came from — which is what a caret or a selection
    /// needs.
    pub cluster: u32,
    /// How far the pen moves after this glyph, in pixels.
    pub x_advance: f32,
    /// The glyph's offset from the pen, in pixels.
    pub x_offset: f32,
    /// The glyph's offset from the baseline, in pixels.
    pub y_offset: f32,
}

/// A shaped run of text.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapedText {
    /// The glyphs, in visual order.
    pub glyphs: Vec<ShapedGlyph>,
    /// The run's total advance, in pixels.
    pub width: f32,
    /// The run's ascent above the baseline, in pixels.
    pub ascent: f32,
    /// The run's descent below the baseline, in pixels.
    pub descent: f32,
}

impl ShapedText {
    /// The run's height: how far it reaches above the baseline plus how far below, in pixels.
    ///
    /// The descent is negative — it is a distance *below* the baseline — so the height is the
    /// ascent minus the descent, not the sum.
    #[must_use]
    pub fn height(&self) -> f32 {
        self.ascent - self.descent
    }

    /// The number of glyphs in the run.
    #[must_use]
    pub fn len(&self) -> usize {
        self.glyphs.len()
    }

    /// Whether the run has no glyphs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }
}

/// One line of a wrapped run of text.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapedLine {
    /// The glyphs on this line, in visual order.
    pub glyphs: Vec<ShapedGlyph>,
    /// The line's advance, in pixels.
    pub width: f32,
    /// The line's ascent above the baseline.
    pub ascent: f32,
    /// The line's descent below the baseline, negative.
    pub descent: f32,
}

impl ShapedLine {
    /// The line's height: how far it reaches above the baseline plus how far below, in pixels.
    ///
    /// The descent is negative — it is a distance *below* the baseline — so the height is the
    /// ascent minus the descent, not the sum. This is the distance between two baselines.
    #[must_use]
    pub fn height(&self) -> f32 {
        self.ascent - self.descent
    }
}

/// The character that marks a line whose text did not fit its box: a horizontal ellipsis.
///
/// It is shaped with the box's own font at the box's own size, so it reads as part of the text
/// rather than as a box the renderer invented.
pub const ELLIPSIS: &str = "\u{2026}";

/// How many of a wrapped block's lines fit in a box of `box_height`, and whether any were left
/// out.
///
/// A line's height is uniform across a block — the first line's — so the number that fit is the
/// box's height divided by that step, floored. A box shorter than one line still draws its first
/// line (clipped at the box's edge, as a box too short for its text always was) rather than
/// nothing: eliding the only line would leave the box blank, which tells a reader less than a
/// clipped line does. Returns the number of lines to draw and whether the block overflowed its
/// box. This closes the clipped-row gap: a block taller than its box is elided and marked, not
/// silently sliced mid-glyph.
#[must_use]
pub fn fit_lines(lines: &[ShapedLine], box_height: f32) -> (usize, bool) {
    if lines.is_empty() {
        return (0, false);
    }
    let step = lines[0].height();
    let fitting = if step > 0.0 {
        (box_height / step).floor().max(1.0) as usize
    } else {
        // A line of no height cannot overflow: every line of it fits.
        lines.len()
    };
    let visible = lines.len().min(fitting);
    (visible, visible < lines.len())
}

/// Marks a line as the last of an elided block: cuts it short to make room for the ellipsis and
/// appends the ellipsis to it.
///
/// Glyphs are dropped from the line's end until the line plus the ellipsis fits `max_width`, so
/// the marker is visible rather than pushed off the box's edge and clipped — a full line with the
/// ellipsis pasted past it would look elided and show nothing. The line's advance is kept true as
/// glyphs go, because that advance is what places the ellipsis.
pub fn elide_line(line: &mut ShapedLine, ellipsis: &ShapedText, max_width: f32) {
    while line.width + ellipsis.width > max_width && line.glyphs.len() > 1 {
        let advance = line.glyphs.pop().map_or(0.0, |glyph| glyph.x_advance);
        line.width -= advance;
    }
    for glyph in &ellipsis.glyphs {
        line.glyphs.push(*glyph);
    }
    line.width += ellipsis.width;
}

/// One maximal single-direction run of a line, as [`bidi_runs`] splits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BidirectionalRun {
    /// The run's first byte in the line.
    pub start: usize,
    /// One past the run's last byte in the line.
    pub end: usize,
    /// Whether the run reads right-to-left.
    pub right_to_left: bool,
}

/// The code point ranges whose scripts read right-to-left.
///
/// A fixed list of blocks rather than a Unicode data table: the right-to-left scripts are few and
/// stable, and a range check needs no lookup. A character outside these ranges — Latin, Greek,
/// Cyrillic, CJK, a digit, a space — reads left-to-right or is directionally neutral.
fn char_is_right_to_left(c: char) -> bool {
    let code = c as u32;
    matches!(
        code,
        0x0590..=0x05FF // Hebrew
            | 0x0600..=0x06FF // Arabic
            | 0x0700..=0x074F // Syriac
            | 0x0750..=0x077F // Arabic Supplement
            | 0x0780..=0x07BF // Thaana
            | 0x07C0..=0x07FF // NKo
            | 0x08A0..=0x08FF // Arabic Extended-A
            | 0xFB1D..=0xFB4F // Hebrew presentation forms
            | 0xFB50..=0xFDFF // Arabic presentation forms-A
            | 0xFE70..=0xFEFF // Arabic presentation forms-B
    )
}

/// Whether a character has a definite direction of its own: a letter of a left-to-right or a
/// right-to-left script. A space, a digit or a punctuation mark does not — it takes the direction
/// of the text around it.
fn is_strong(c: char) -> bool {
    char_is_right_to_left(c) || c.is_alphabetic()
}

/// Splits a line into maximal single-direction runs, in the order they are read.
///
/// The shaper works on one direction at a time: it reorders a run that is all right-to-left, but a
/// mixed-direction line handed to it whole is shaped in its first strong direction with the other
/// runs left in logical order — which draws an embedded Hebrew or Arabic word backwards. Splitting
/// the line first lets each run be shaped with its own direction and the runs concatenated in
/// reading order.
///
/// This is a bounded bidirectional algorithm, not the whole of UAX #9: strong characters are
/// classified by script, and a neutral — a space, punctuation, a digit — takes the direction of the
/// strong text around it, the surrounding run when both sides agree and the paragraph's base
/// direction (the first strong character's) otherwise. It has no separate handling for directional
/// marks or paired brackets, and a number follows its run's direction rather than being laid out by
/// its own, more detailed, rules. That covers the common cases — a right-to-left word in a
/// left-to-right sentence and the reverse, and two right-to-left words in a row — and
/// `docs/known-gaps.md` records the limits.
#[must_use]
pub fn bidi_runs(text: &str) -> Vec<BidirectionalRun> {
    // Every character with its byte offset, so a run's range can be recovered from the characters
    // that went into it.
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    if chars.is_empty() {
        return Vec::new();
    }

    // The paragraph's base direction is the first strong character's; a line of only neutrals
    // reads left-to-right.
    let base = chars
        .iter()
        .find(|(_, c)| is_strong(*c))
        .is_some_and(|(_, c)| char_is_right_to_left(*c));

    // Strong characters keep their own direction; a neutral starts at the base and is corrected
    // below to the strong direction on both sides of it when they agree.
    let mut direction: Vec<bool> = chars
        .iter()
        .map(|(_, c)| {
            if is_strong(*c) {
                char_is_right_to_left(*c)
            } else {
                base
            }
        })
        .collect();
    for index in 0..chars.len() {
        if is_strong(chars[index].1) {
            continue;
        }
        let before = (0..index)
            .rev()
            .find(|j| is_strong(chars[*j].1))
            .map(|j| char_is_right_to_left(chars[j].1));
        let after = (index + 1..chars.len())
            .find(|j| is_strong(chars[*j].1))
            .map(|j| char_is_right_to_left(chars[j].1));
        direction[index] = match (before, after) {
            (Some(before), Some(after)) if before == after => before,
            _ => base,
        };
    }

    // Group the characters into runs of one direction, recording each run's byte range.
    let mut runs = Vec::new();
    let mut start = 0usize;
    for index in 1..=chars.len() {
        let boundary = index == chars.len() || direction[index] != direction[start];
        if !boundary {
            continue;
        }
        runs.push(BidirectionalRun {
            start: chars[start].0,
            end: if index < chars.len() {
                chars[index].0
            } else {
                text.len()
            },
            right_to_left: direction[start],
        });
        start = index;
    }
    runs
}

/// Breaks a shaped run into lines that fit `width`.
///
/// A line is broken at the last space on it where there is one, so words stay whole, and
/// between glyphs where there is not, so a word wider than the box is still drawn rather than
/// dropped. A newline forces a break. A run narrower than `width` is one line, and a run drawn
/// into a box of no width is one line too — the drawing clips either way, and a caller that
/// wants one line gets one.
fn wrap_lines(shaped: &ShapedText, text: &str, width: f32) -> Vec<ShapedLine> {
    let glyphs = &shaped.glyphs;
    if width <= 0.0 {
        return vec![line_of(glyphs, shaped)];
    }
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut pen = 0.0f32;
    // The last space on the current line, which is where the line breaks if it overflows.
    let mut space = None;
    for index in 0..glyphs.len() {
        let glyph = &glyphs[index];
        let character = text
            .get(usize::try_from(glyph.cluster).unwrap_or(usize::MAX)..)
            .and_then(|tail| tail.chars().next());
        if character == Some('\n') {
            lines.push(line_of(&glyphs[start..index], shaped));
            start = index + 1;
            pen = 0.0;
            space = None;
            continue;
        }
        if character.is_some_and(char::is_whitespace) {
            space = Some(index);
        }
        pen += glyph.x_advance;
        if pen <= width || index == start {
            continue;
        }
        // A space is the break when there is one after the line's first glyph; otherwise the
        // line breaks between glyphs. Either way the glyph at `split` is not drawn: a space is
        // the break, and the glyph that overflowed opens the next line.
        let (split, is_space) = match space {
            Some(space) if space > start => (space, true),
            _ => (index, false),
        };
        lines.push(line_of(&glyphs[start..split], shaped));
        start = if is_space { split + 1 } else { split };
        pen = if start <= index {
            glyphs[start..=index]
                .iter()
                .map(|glyph| glyph.x_advance)
                .sum()
        } else {
            0.0
        };
        space = None;
    }
    lines.push(line_of(&glyphs[start..], shaped));
    lines
}

/// One line from a slice of a shaped run.
fn line_of(glyphs: &[ShapedGlyph], shaped: &ShapedText) -> ShapedLine {
    ShapedLine {
        width: glyphs.iter().map(|glyph| glyph.x_advance).sum(),
        glyphs: glyphs.to_vec(),
        ascent: shaped.ascent,
        descent: shaped.descent,
    }
}

/// A font, loaded and ready to shape with.
#[derive(Debug, Clone)]
pub struct Font {
    /// The font file's bytes.
    ///
    /// Held as an `Arc` so a font can be shared across the threads that shape with it
    /// without copying the file for each of them.
    data: Arc<Vec<u8>>,
    /// Which face within the file, for a collection.
    index: u32,
    /// The font's family name.
    family: String,
    /// The font's design units per em.
    units_per_em: u16,
}

impl Font {
    /// Loads a font from a file.
    ///
    /// # Errors
    ///
    /// Returns [`TextError::Unreadable`] when the file cannot be read, and
    /// [`TextError::NotAFont`] when its bytes are not a font.
    pub fn from_path(path: &Path) -> Result<Self, TextError> {
        let data = std::fs::read(path).map_err(|error| TextError::Unreadable {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
        Self::from_bytes(data, 0)
    }

    /// Loads a font from bytes.
    ///
    /// # Errors
    ///
    /// Returns [`TextError::NotAFont`] when the bytes are not a font, and
    /// [`TextError::NoGlyphs`] when the font holds none.
    pub fn from_bytes(data: Vec<u8>, index: u32) -> Result<Self, TextError> {
        let face = TtfFace::parse(&data, index).map_err(|error| TextError::NotAFont {
            reason: error.to_string(),
        })?;
        if face.number_of_glyphs() == 0 {
            return Err(TextError::NoGlyphs);
        }
        let family = face
            .names()
            .into_iter()
            .filter(|name| name.name_id == name_id::TYPOGRAPHIC_FAMILY)
            .find_map(|name| name.to_string())
            .unwrap_or_else(|| format!("font #{index}"));
        let units_per_em = face.units_per_em();
        Ok(Self {
            data: Arc::new(data),
            index,
            family,
            units_per_em,
        })
    }

    /// The font's family name.
    #[must_use]
    pub fn family(&self) -> &str {
        &self.family
    }

    /// The font's design units per em.
    #[must_use]
    pub const fn units_per_em(&self) -> u16 {
        self.units_per_em
    }

    /// Shapes `text` at `size_px` pixels.
    ///
    /// The size is in pixels, and the advances it produces are in pixels too, so a caller
    /// never has to convert between the font's design units and the screen's.
    ///
    /// # Errors
    ///
    /// Returns [`TextError::NotAFont`] when the font's bytes can no longer be read as a
    /// font — which cannot happen for a font this type produced, and is checked anyway
    /// because a font that cannot be parsed must be reported rather than drawn as nothing.
    pub fn shape(&self, text: &str, size_px: f32) -> Result<ShapedText, TextError> {
        let ttf = TtfFace::parse(&self.data, self.index).map_err(|error| TextError::NotAFont {
            reason: error.to_string(),
        })?;
        // The metrics are read before the face is moved into the shaper, because the
        // shaper takes ownership of it.
        let (ascender, descender) = (ttf.ascender(), ttf.descender());
        let face = HbFace::from_face(ttf);
        let scale = if self.units_per_em == 0 {
            1.0
        } else {
            size_px / f32::from(self.units_per_em)
        };
        // The shaper works on one direction at a time: it reorders a run that is all
        // right-to-left, but a mixed-direction line handed to it whole is shaped in the first
        // strong direction and its other runs are left in logical order — which draws a Hebrew
        // word inside an English sentence backwards. So the line is split into single-direction
        // runs first and each is shaped on its own, then the runs are concatenated in the order
        // they are read. Clusters are rebased onto the whole line so a caller can still map a
        // glyph back to the character it came from.
        let mut glyphs = Vec::new();
        for run in bidi_runs(text) {
            let mut buffer = UnicodeBuffer::new();
            buffer.push_str(&text[run.start..run.end]);
            // The run's direction is decided here, not left to the shaper's guess: the buffer
            // has no surrounding context, so the guess reads the first strong character of the
            // run — which is the run's own direction, but only because `bidi_runs` already split
            // mixed text apart. Stating it makes the shaping independent of that inference.
            if run.right_to_left {
                buffer.set_direction(rustybuzz::Direction::RightToLeft);
            } else {
                buffer.set_direction(rustybuzz::Direction::LeftToRight);
            }
            let shaped = rustybuzz::shape(&face, &[], buffer);
            let infos = shaped.glyph_infos();
            let positions = shaped.glyph_positions();
            for (info, position) in infos.iter().zip(positions.iter()) {
                glyphs.push(ShapedGlyph {
                    glyph_id: info.glyph_id as u16,
                    // The run was shaped as a slice starting at `run.start`, so its clusters
                    // count from there; put them back into whole-line coordinates.
                    cluster: info.cluster + run.start as u32,
                    x_advance: position.x_advance as f32 * scale,
                    x_offset: position.x_offset as f32 * scale,
                    y_offset: position.y_offset as f32 * scale,
                });
            }
        }
        let width = glyphs.iter().map(|glyph| glyph.x_advance).sum();
        Ok(ShapedText {
            glyphs,
            width,
            ascent: f32::from(ascender) * scale,
            descent: f32::from(descender) * scale,
        })
    }

    /// Shapes `text` at `size_px` and breaks it into lines that fit `width`.
    ///
    /// A run that already fits is one line, so a caller drawing a single line gets what it
    /// drew before. What is left to the caller is the vertical: a block of lines taller than
    /// the box it is drawn in is the caller's to clip, which is what drawing has always done.
    ///
    /// # Errors
    ///
    /// Returns the reason the text could not be shaped, which is a font this runtime cannot
    /// read rather than a mistake in the text.
    pub fn wrap(&self, text: &str, size_px: f32, width: f32) -> Result<Vec<ShapedLine>, TextError> {
        let shaped = self.shape(text, size_px)?;
        Ok(wrap_lines(&shaped, text, width))
    }

    /// The font's outline for one glyph, in pixels, as the contours it is made of.
    ///
    /// The contours are kept apart rather than flattened into one list because which contour is
    /// a hole and which is the letter is a question about their direction, and a renderer that
    /// lost the boundaries could not answer it — an `o` would be drawn solid.
    ///
    /// # Errors
    ///
    /// Returns [`TextError::NotAFont`] when the font's bytes can no longer be read.
    pub fn glyph_outline(&self, glyph_id: u16, size_px: f32) -> Result<Vec<Contour>, TextError> {
        let ttf = TtfFace::parse(&self.data, self.index).map_err(|error| TextError::NotAFont {
            reason: error.to_string(),
        })?;
        let scale = if self.units_per_em == 0 {
            1.0
        } else {
            size_px / f32::from(self.units_per_em)
        };
        let mut builder = PointCollector::new(scale);
        ttf.outline_glyph(GlyphId(glyph_id), &mut builder);
        Ok(builder.finish())
    }

    /// The coverage of one glyph, at `size_px`, or `None` when the glyph draws nothing.
    ///
    /// A space has no outline, and a glyph whose outline encloses no area covers no pixel; both
    /// are `None` rather than an error, because they are what the font says, not a failure.
    ///
    /// # Errors
    ///
    /// Returns [`TextError::NotAFont`] when the font's bytes can no longer be read, and
    /// [`TextError::NotDrawable`] when the outline is too large to rasterise.
    pub fn glyph_coverage(
        &self,
        glyph_id: u16,
        size_px: f32,
    ) -> Result<Option<Coverage>, TextError> {
        let contours = self.glyph_outline(glyph_id, size_px)?;
        match crate::glyphs::rasterise(&contours) {
            Ok(coverage) => Ok(Some(coverage)),
            Err(crate::glyphs::GlyphError::Empty) => Ok(None),
            Err(error) => Err(TextError::NotDrawable {
                reason: error.to_string(),
            }),
        }
    }
}

/// Collects a glyph's outline, contour by contour.
///
/// The control points of a quadratic are kept, with the flag that says so, because a renderer
/// that only wanted the on-curve points would draw a different shape from the one the font
/// describes. A cubic — which an OpenType `CFF` font uses where a TrueType font uses a
/// quadratic — is flattened into line segments here, because the contour representation carries
/// one kind of curve and a second kind would be a second code path in every consumer.
#[derive(Debug)]
struct PointCollector {
    /// The contours finished so far.
    contours: Vec<Contour>,
    /// The contour being collected.
    current: Vec<OutlinePoint>,
    /// Where the pen is, in pixels.
    pen: (f32, f32),
    /// The font's design units to pixels.
    scale: f32,
}

impl PointCollector {
    /// A collector that scales the font's design units into pixels.
    fn new(scale: f32) -> Self {
        Self {
            contours: Vec::new(),
            current: Vec::new(),
            pen: (0.0, 0.0),
            scale,
        }
    }

    /// Records an on-curve point, scaled from the font's design units into pixels.
    fn on_curve(&mut self, x: f32, y: f32) {
        let (x, y) = (x * self.scale, y * self.scale);
        self.pen = (x, y);
        self.current.push(OutlinePoint::on_curve(x, y));
    }

    /// Records a control point, scaled from the font's design units into pixels.
    fn control(&mut self, x: f32, y: f32) {
        self.current
            .push(OutlinePoint::control(x * self.scale, y * self.scale));
    }

    /// Closes the contour being collected, if there is one.
    fn finish_contour(&mut self) {
        if !self.current.is_empty() {
            self.contours
                .push(Contour::new(std::mem::take(&mut self.current)));
        }
    }

    /// The contours collected, with the last one closed.
    fn finish(mut self) -> Vec<Contour> {
        self.finish_contour();
        self.contours
    }

    /// Appends a cubic curve as line segments.
    fn flatten_cubic(
        &mut self,
        first_x: f32,
        first_y: f32,
        second_x: f32,
        second_y: f32,
        end_x: f32,
        end_y: f32,
    ) {
        let (start_x, start_y) = self.pen;
        let (first_x, first_y) = (first_x * self.scale, first_y * self.scale);
        let (second_x, second_y) = (second_x * self.scale, second_y * self.scale);
        let (end_x, end_y) = (end_x * self.scale, end_y * self.scale);
        let length = (first_x - start_x).hypot(first_y - start_y)
            + (second_x - first_x).hypot(second_y - first_y)
            + (end_x - second_x).hypot(end_y - second_y);
        let steps = if length.is_finite() {
            (length * 2.0).sqrt().ceil().clamp(2.0, 64.0) as usize
        } else {
            2
        };
        for step in 1..=steps {
            let fraction = step as f32 / steps as f32;
            let inverse = 1.0 - fraction;
            let from_weight = inverse * inverse * inverse;
            let first_weight = 3.0 * inverse * inverse * fraction;
            let second_weight = 3.0 * inverse * fraction * fraction;
            let end_weight = fraction * fraction * fraction;
            self.on_curve(
                from_weight * start_x
                    + first_weight * first_x
                    + second_weight * second_x
                    + end_weight * end_x,
                from_weight * start_y
                    + first_weight * first_y
                    + second_weight * second_y
                    + end_weight * end_y,
            );
        }
    }
}

impl OutlineBuilder for PointCollector {
    fn move_to(&mut self, x: f32, y: f32) {
        self.finish_contour();
        self.on_curve(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.on_curve(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.control(x1, y1);
        self.on_curve(x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.flatten_cubic(x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        self.finish_contour();
    }
}

/// The fonts this module looks for, in order of preference.
///
/// A list of known paths rather than a directory scan: a scan would have to open and parse
/// every font on the machine to read its family name, which is a lot of work to find a
/// font that is almost certainly there.
#[must_use]
pub fn candidate_font_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if cfg!(target_os = "windows") {
        let fonts = PathBuf::from(r"C:\Windows\Fonts");
        for name in [
            "segoeui.ttf",
            "arial.ttf",
            "tahoma.ttf",
            "verdana.ttf",
            "calibri.ttf",
        ] {
            paths.push(fonts.join(name));
        }
    } else if cfg!(target_os = "macos") {
        for path in [
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/System/Library/Fonts/Helvetica.ttc",
            "/Library/Fonts/Arial.ttf",
        ] {
            paths.push(PathBuf::from(path));
        }
    } else {
        for path in [
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
        ] {
            paths.push(PathBuf::from(path));
        }
    }
    paths
}

/// The first usable font on this machine.
///
/// # Errors
///
/// Returns [`TextError::NoFontAvailable`] when none of the candidates can be read, and
/// [`TextError::NotAFont`] when a candidate is readable but is not a font.
pub fn default_font() -> Result<Font, TextError> {
    let mut last_error = None;
    for path in candidate_font_paths() {
        match Font::from_path(&path) {
            Ok(font) => return Ok(font),
            Err(error) => {
                if let TextError::Unreadable { .. } = error {
                    last_error = Some(error);
                } else {
                    return Err(error);
                }
            }
        }
    }
    Err(last_error.unwrap_or(TextError::NoFontAvailable))
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

    /// A font from the machine's own fonts, or none.
    fn system_font() -> Option<Font> {
        match default_font() {
            Ok(font) => Some(font),
            Err(error) => {
                // The reason is not printed: the workspace denies `eprintln!` in library
                // code, and a test that cannot find a font is a fact about the machine.
                let _ = error;
                None
            }
        }
    }

    #[test]
    fn text_shapes_into_glyphs_with_a_positive_advance() {
        let Some(font) = system_font() else {
            return;
        };
        let shaped = font.shape("Hello", 16.0).expect("shaped");
        assert!(!shaped.is_empty(), "text must produce glyphs");
        assert!(
            shaped.width > 0.0,
            "text must advance, got {}",
            shaped.width
        );
        assert!(shaped.ascent > 0.0, "a font has an ascent");
        assert!(
            shaped.descent < 0.0,
            "a font's descent is below the baseline"
        );
    }

    #[test]
    fn an_empty_string_produces_no_glyphs() {
        let Some(font) = system_font() else {
            return;
        };
        let shaped = font.shape("", 16.0).expect("shaped");
        assert!(shaped.is_empty());
        assert!(shaped.width.abs() < f32::EPSILON);
    }

    #[test]
    fn a_larger_size_produces_a_wider_run() {
        // The size is in pixels, so doubling it must double the advance. A font's metrics
        // are in design units, and the conversion is the renderer's job; a run that did
        // not scale would mean the conversion was missing.
        let Some(font) = system_font() else {
            return;
        };
        let small = font.shape("Hello", 16.0).expect("shaped");
        let large = font.shape("Hello", 32.0).expect("shaped");
        assert!(
            large.width > small.width * 1.9,
            "doubling the size must nearly double the width: {} vs {}",
            large.width,
            small.width
        );
    }

    #[test]
    fn every_glyph_carries_the_cluster_it_came_from() {
        let Some(font) = system_font() else {
            return;
        };
        let shaped = font.shape("abc", 16.0).expect("shaped");
        for glyph in &shaped.glyphs {
            assert!(
                glyph.cluster < 3,
                "a cluster must point into the text: {}",
                glyph.cluster
            );
        }
    }

    // --- Right-to-left and bidirectional text ---

    /// Shapes a right-to-left sample, or `None` when this machine's font does not cover the
    /// script — a coverage fact about the machine, not a defect in the shaper.
    fn right_to_left_sample(font: &Font, text: &str) -> Option<ShapedText> {
        let shaped = font.shape(text, 24.0).ok()?;
        let uncovered = shaped.glyphs.iter().filter(|g| g.glyph_id == 0).count();
        if uncovered > 0 {
            return None;
        }
        Some(shaped)
    }

    /// The clusters of a shaped run, in buffer order.
    fn clusters(shaped: &ShapedText) -> Vec<u32> {
        shaped.glyphs.iter().map(|glyph| glyph.cluster).collect()
    }

    #[test]
    fn a_pure_right_to_left_run_comes_out_in_visual_order() {
        // A right-to-left run is reordered by the shaper so the buffer reads as the eye does,
        // leftmost glyph first: the logically-last letter is drawn at the left. Descending
        // clusters are that reordering — the buffer holds the letters back to front.
        let Some(font) = system_font() else {
            return;
        };
        for text in ["שלום", "مرحبا"] {
            let Some(shaped) = right_to_left_sample(&font, text) else {
                continue;
            };
            let clusters = clusters(&shaped);
            let mut reversed = clusters.clone();
            reversed.sort_unstable_by(|a, b| b.cmp(a));
            assert_eq!(
                clusters, reversed,
                "`{text}` must be reordered into right-to-left visual order"
            );
        }
    }

    #[test]
    fn a_right_to_left_word_in_a_left_to_right_sentence_is_reordered() {
        // The defect this closes: a Hebrew word inside an English sentence was shaped in the
        // sentence's left-to-right direction and left in logical order, so it drew backwards. The
        // line is split into runs first, and the Hebrew run comes out reversed within its slot —
        // its clusters descend even though the words around it ascend.
        let Some(font) = system_font() else {
            return;
        };
        let text = "ab שלום cd";
        let Some(shaped) = right_to_left_sample(&font, text) else {
            return;
        };
        // The Hebrew word occupies the bytes between the two spaces: from the first Hebrew byte to
        // the space that follows it.
        let hebrew_start = text.find('ש').expect("the Hebrew word is there");
        let hebrew_end = text[hebrew_start..]
            .find(' ')
            .map_or(text.len(), |space| hebrew_start + space);
        let in_hebrew: Vec<u32> = shaped
            .glyphs
            .iter()
            .filter(|glyph| {
                glyph.cluster >= hebrew_start as u32 && glyph.cluster < hebrew_end as u32
            })
            .map(|glyph| glyph.cluster)
            .collect();
        assert!(
            in_hebrew.len() >= 4,
            "the Hebrew word shaped into glyphs: {in_hebrew:?}"
        );
        let mut reversed = in_hebrew.clone();
        reversed.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(
            in_hebrew, reversed,
            "the embedded Hebrew word must be reordered right-to-left, not left in logical order"
        );
        // The Latin around it stays in reading order: ascending, as it always was.
        let latin: Vec<u32> = shaped
            .glyphs
            .iter()
            .filter(|glyph| glyph.cluster < hebrew_start as u32)
            .map(|glyph| glyph.cluster)
            .collect();
        assert!(
            latin.windows(2).all(|pair| pair[0] < pair[1]),
            "the leading Latin stays left-to-right: {latin:?}"
        );
    }

    #[test]
    fn two_right_to_left_words_are_reordered_as_one_run() {
        // Two Hebrew words separated by a space read as one right-to-left phrase: the space
        // between them takes the right-to-left direction of the letters on both sides, so the
        // whole line is one run and the word order reverses with the letters. Shaping the words
        // separately would reverse each word but leave them in the wrong order.
        let Some(font) = system_font() else {
            return;
        };
        let text = "שלום עולם";
        let Some(shaped) = right_to_left_sample(&font, text) else {
            return;
        };
        let runs = bidi_runs(text);
        assert_eq!(
            runs.len(),
            1,
            "two right-to-left words and the space between are one run: {runs:?}"
        );
        assert!(runs[0].right_to_left);
        let clusters = clusters(&shaped);
        let mut reversed = clusters.clone();
        reversed.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(
            clusters, reversed,
            "the whole phrase is reordered right-to-left, word order included"
        );
    }

    #[test]
    fn bidi_runs_splits_a_mixed_line_into_single_direction_runs() {
        // The splitter's own contract, tested without a font: a left-to-right sentence with an
        // embedded right-to-left word is three runs, and the right-to-left one is named as such.
        let runs = bidi_runs("ab שלום cd");
        assert_eq!(runs.len(), 3, "{runs:?}");
        assert!(!runs[0].right_to_left, "the leading Latin is left-to-right");
        assert!(runs[1].right_to_left, "the Hebrew word is right-to-left");
        assert!(
            !runs[2].right_to_left,
            "the trailing Latin is left-to-right"
        );
        // The runs tile the line exactly: each byte belongs to one run, in order.
        assert_eq!(runs[0].start, 0);
        assert_eq!(runs[2].end, "ab שלום cd".len());
    }

    #[test]
    fn a_line_of_only_left_to_right_text_is_one_run() {
        // The common case must not change: plain Latin is a single left-to-right run, so shaping
        // it goes through exactly one shaper call, as it always did.
        let runs = bidi_runs("hello, world! 123");
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert!(!runs[0].right_to_left);
        assert_eq!(runs[0].start, 0);
        assert_eq!(runs[0].end, "hello, world! 123".len());
    }

    #[test]
    fn an_empty_line_has_no_runs() {
        assert!(bidi_runs("").is_empty());
    }

    #[test]
    fn arabic_letters_take_their_joined_forms() {
        // Arabic joins: a letter drawn alone is in its isolated form, and the same letter inside a
        // word is in an initial, medial or final form — a different glyph. A shaper that left every
        // letter in its isolated form would draw a word as a row of disconnected letters.
        //
        // The font is the machine's, so this asserts the join through whatever font is here. The
        // facts that make the assertion inapplicable are facts about that machine, not defects in
        // the shaper, and each one skips: no font at all; a font that does not cover Arabic (the
        // sample comes back as `.notdef`); a font whose coverage maps the joined sample to the
        // isolated glyph — macOS's Helvetica does for this pair, because its Arabic forms live in
        // an AAT `morx` table and `rustybuzz` does not implement that state machine, while the
        // same shaper joins through fonts that carry the substitutions (Segoe UI on Windows,
        // DejaVu Sans on Linux).
        let Some(font) = system_font() else {
            return;
        };
        let letter = "ب"; // beh, on its own.
        let Some(isolated) = right_to_left_sample(&font, letter) else {
            return;
        };
        let Some(joined) = right_to_left_sample(&font, "ببب") else {
            return;
        };
        let isolated_id = isolated.glyphs[0].glyph_id;
        assert_eq!(isolated.glyphs.len(), 1, "one letter alone is one glyph");
        // Each beh in the word is joined to a neighbour, so none keeps the isolated form —
        // where the machine can answer the question. A font that does not cover Arabic
        // shapes the sample as `.notdef`, and macOS's system font carries the joining forms
        // in an AAT `morx` table that `rustybuzz` does not implement, so there the shaper
        // answers with the nominal glyphs and there is no join to check. The assertion runs
        // where the system font expresses joining in GSUB (Windows's Segoe UI, Linux's
        // DejaVu Sans) and the shaper is required to apply it.
        if isolated_id != 0 && !cfg!(target_os = "macos") {
            assert!(
                joined
                    .glyphs
                    .iter()
                    .all(|glyph| glyph.glyph_id != isolated_id),
                "a joined beh must not keep the isolated form {isolated_id}"
            );
        }
    }

    #[test]
    fn bytes_that_are_not_a_font_are_refused() {
        let error =
            Font::from_bytes(b"not a font at all".to_vec(), 0).expect_err("must be refused");
        assert!(matches!(error, TextError::NotAFont { .. }), "got {error}");
    }

    #[test]
    fn a_font_that_cannot_be_read_reports_the_path() {
        let error = Font::from_path(Path::new("/definitely/not/here.ttf")).expect_err("must fail");
        assert!(matches!(error, TextError::Unreadable { .. }), "got {error}");
    }

    #[test]
    fn a_glyph_has_an_outline_of_whole_contours() {
        let Some(font) = system_font() else {
            return;
        };
        let shaped = font.shape("A", 32.0).expect("shaped");
        let Some(glyph) = shaped.glyphs.first() else {
            assert!(shaped.is_empty(), "no glyphs at all");
            return;
        };
        let outline = font
            .glyph_outline(glyph.glyph_id, 32.0)
            .expect("an outline");
        assert!(
            !outline.is_empty(),
            "a letter must have an outline: glyph {}",
            glyph.glyph_id
        );
        // The contours are what make a hole a hole: a letter with an enclosed counter — an `o`,
        // an `e`, an `A` — has more than one, and each one holds at least three points.
        for contour in &outline {
            assert!(
                contour.points.len() >= 3,
                "a contour with fewer than three points encloses no area"
            );
        }
        let o = font.shape("o", 32.0).expect("shaped");
        let Some(o_glyph) = o.glyphs.first() else {
            return;
        };
        let o_outline = font
            .glyph_outline(o_glyph.glyph_id, 32.0)
            .expect("an outline");
        assert!(
            o_outline.len() >= 2,
            "an `o` has an outer contour and a counter: got {}",
            o_outline.len()
        );
    }

    #[test]
    fn a_letter_rasterises_into_coverage_with_a_hollow_counter() {
        // The whole text path in one test: shape a string, take a glyph's outline, rasterise it,
        // and check that the letter's counter is not filled in.
        let Some(font) = system_font() else {
            return;
        };
        let shaped = font.shape("o", 64.0).expect("shaped");
        let Some(glyph) = shaped.glyphs.first() else {
            return;
        };
        let coverage = font
            .glyph_coverage(glyph.glyph_id, 64.0)
            .expect("an outline")
            .expect("an `o` draws something");
        assert!(coverage.is_whole());
        assert!(!coverage.is_empty(), "the letter covers pixels");
        assert!(
            coverage.width > 20 && coverage.height > 20,
            "a 64-pixel letter is more than a few pixels: {}x{}",
            coverage.width,
            coverage.height
        );
        // The middle of the letter is its counter, which is not part of the glyph.
        let middle_x = coverage.width / 2;
        let middle_y = coverage.height / 2;
        assert_eq!(
            coverage.at(middle_x, middle_y),
            Some(0),
            "the counter of an `o` is a hole, not ink"
        );
    }

    #[test]
    fn a_space_draws_nothing_rather_than_failing() {
        let Some(font) = system_font() else {
            return;
        };
        let shaped = font.shape(" ", 32.0).expect("shaped");
        let Some(glyph) = shaped.glyphs.first() else {
            return;
        };
        assert_eq!(
            font.glyph_coverage(glyph.glyph_id, 32.0).expect("a space"),
            None,
            "a space has no ink, which is not a failure"
        );
    }

    #[test]
    fn a_runs_height_is_its_reach_above_plus_its_reach_below() {
        // The descent is a distance below the baseline and is negative, so the height is the
        // ascent minus the descent. Getting this wrong moves every line a descent out of place,
        // which is how the single-line case used to be positioned.
        let Some(font) = system_font() else {
            return;
        };
        let shaped = font.shape("Hx", 16.0).expect("shaped");
        assert!(
            (shaped.height() - (shaped.ascent - shaped.descent)).abs() < f32::EPSILON,
            "the height is the ascent minus the (negative) descent"
        );
        assert!(
            shaped.height() > shaped.ascent,
            "a run reaches below its baseline, so it is taller than its ascent"
        );
    }

    #[test]
    fn a_run_that_fits_is_one_line() {
        let Some(font) = system_font() else {
            return;
        };
        let text = "Hello";
        let shaped = font.shape(text, 16.0).expect("shaped");
        let lines = font.wrap(text, 16.0, shaped.width).expect("wrapped");
        assert_eq!(lines.len(), 1, "a run that fits is one line");
        assert_eq!(
            lines[0].glyphs.len(),
            shaped.glyphs.len(),
            "the line is the whole run"
        );
    }

    #[test]
    fn a_longer_run_breaks_at_the_last_space() {
        // The break is at a space so words stay whole, and the space itself is not drawn: the
        // first line's glyphs are the first two words and nothing else. The box is one pixel
        // wider than those two words, so the following space is what overflows it.
        let Some(font) = system_font() else {
            return;
        };
        let text = "aa bb cc";
        let head = font.shape("aa bb", 16.0).expect("shaped").width;
        let lines = font.wrap(text, 16.0, head + 1.0).expect("wrapped");
        assert_eq!(lines.len(), 2, "the run must break: {lines:?}");
        assert_eq!(
            lines[0].glyphs.len(),
            5,
            "the first line is the first two words, without the space"
        );
        assert_eq!(lines[1].glyphs.len(), 2, "the second line is the last word");
    }

    #[test]
    fn a_word_wider_than_the_box_breaks_between_glyphs() {
        // There is no space to break at, so the word is broken rather than dropped. A line
        // overflows only when it holds a single glyph that is itself wider than the box: a
        // glyph that could have moved to the next line is moved.
        let Some(font) = system_font() else {
            return;
        };
        let lines = font.wrap("aaaaaaaa", 16.0, 8.0).expect("wrapped");
        assert!(lines.len() > 1, "a word wider than its box must break");
        assert!(
            lines
                .iter()
                .all(|line| line.width <= 8.0 + f32::EPSILON || line.glyphs.len() == 1),
            "a line overflows only around a single glyph: {:?}",
            lines.iter().map(|line| line.width).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_newline_forces_a_break() {
        let Some(font) = system_font() else {
            return;
        };
        let lines = font.wrap("ab\ncd", 16.0, 1000.0).expect("wrapped");
        assert_eq!(lines.len(), 2, "a newline is a break: {lines:?}");
        assert_eq!(lines[0].glyphs.len(), 2);
        assert_eq!(lines[1].glyphs.len(), 2);
    }

    #[test]
    fn a_box_of_no_width_is_one_line() {
        // A box that fits nothing is the caller's to clip; wrapping it into one line per glyph
        // would only move the clipping around.
        let Some(font) = system_font() else {
            return;
        };
        let text = "Hello";
        let shaped = font.shape(text, 16.0).expect("shaped");
        let lines = font.wrap(text, 16.0, 0.0).expect("wrapped");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].glyphs.len(), shaped.glyphs.len());
    }

    // The elision decision is pure arithmetic over line heights and widths, so these tests build
    // lines out of numbers rather than shaping real text: they need no font and no graphics
    // adapter, which is what makes them hold on every machine.

    /// A line `height` tall made of `count` glyphs that each advance `advance`, so its width is
    /// `count * advance`. The glyph ids are placeholders: fitting a block is about heights and
    /// widths, not which glyph is which.
    fn line_of_height(height: f32, count: usize, advance: f32) -> ShapedLine {
        ShapedLine {
            glyphs: (0..count)
                .map(|index| ShapedGlyph {
                    glyph_id: u16::try_from(index).unwrap_or(0),
                    cluster: u32::try_from(index).unwrap_or(0),
                    x_advance: advance,
                    x_offset: 0.0,
                    y_offset: 0.0,
                })
                .collect(),
            width: count as f32 * advance,
            ascent: height,
            descent: 0.0,
        }
    }

    /// An ellipsis that advances `width`, made of one glyph.
    fn ellipsis_of_width(width: f32) -> ShapedText {
        ShapedText {
            glyphs: vec![ShapedGlyph {
                glyph_id: 0,
                cluster: 0,
                x_advance: width,
                x_offset: 0.0,
                y_offset: 0.0,
            }],
            width,
            ascent: 0.0,
            descent: 0.0,
        }
    }

    /// Whether two widths agree to within a thousandth of a pixel: the widths here are sums of
    /// whole advances, but a float is compared, so the comparison is a tolerance.
    fn close(left: f32, right: f32) -> bool {
        (left - right).abs() < 1.0e-3
    }

    #[test]
    fn a_block_taller_than_its_box_is_elided_to_the_lines_that_fit() {
        // Five lines sixteen tall in a box forty tall: two fit, three are left out, and the block
        // is marked as elided so the last drawn line carries an ellipsis.
        let lines: Vec<ShapedLine> = (0..5).map(|_| line_of_height(16.0, 4, 8.0)).collect();
        let (visible, elided) = fit_lines(&lines, 40.0);
        assert_eq!(visible, 2, "two sixteen-tall lines fit in forty");
        assert!(elided, "three lines were left out, so the block is elided");
    }

    #[test]
    fn a_block_that_fits_is_not_elided() {
        let lines: Vec<ShapedLine> = (0..3).map(|_| line_of_height(16.0, 4, 8.0)).collect();
        let (visible, elided) = fit_lines(&lines, 48.0);
        assert_eq!(visible, 3, "all three lines fit in forty-eight");
        assert!(!elided, "nothing was left out, so there is no ellipsis");
    }

    #[test]
    fn a_box_shorter_than_one_line_still_draws_its_first_line() {
        // A box that cannot hold even one whole line is not left blank: it draws its first line,
        // clipped at the box's edge as a box too short for its text always was, and is marked
        // elided so the reader is told there was more. Blanking it would say less.
        let lines: Vec<ShapedLine> = (0..3).map(|_| line_of_height(16.0, 4, 8.0)).collect();
        let (visible, elided) = fit_lines(&lines, 10.0);
        assert_eq!(visible, 1, "one line is drawn even when it will be clipped");
        assert!(
            elided,
            "two lines were left out, so the box is marked elided"
        );
    }

    #[test]
    fn an_empty_block_is_neither_drawn_nor_elided() {
        let (visible, elided) = fit_lines(&[], 40.0);
        assert_eq!(visible, 0);
        assert!(!elided);
    }

    #[test]
    fn eliding_a_line_makes_room_for_the_ellipsis_inside_the_box() {
        // A full line of ten glyphs plus a twenty-wide ellipsis would not fit a hundred-wide box,
        // so glyphs come off the line's end until the line and the marker both fit. The marker is
        // then visible on the box rather than pushed off its edge and clipped.
        let mut last = line_of_height(16.0, 10, 10.0);
        assert!(close(last.width, 100.0), "{}", last.width);
        let ellipsis = ellipsis_of_width(20.0);
        elide_line(&mut last, &ellipsis, 100.0);
        assert!(
            last.width <= 100.0,
            "the elided line plus its ellipsis must fit the box: {}",
            last.width
        );
        assert_eq!(
            last.glyphs.len(),
            8 + 1,
            "two glyphs made room, and the ellipsis joined them"
        );
        assert!(
            close(last.width, 100.0),
            "eight glyphs of ten plus a twenty marker: {}",
            last.width
        );
    }

    #[test]
    fn eliding_a_short_line_keeps_at_least_one_glyph() {
        // A line of one glyph is not emptied by the marker: the loop stops at one glyph, so a
        // single-letter line keeps its letter and the ellipsis rides after it even if the pair is
        // a little wide.
        let mut last = line_of_height(16.0, 1, 60.0);
        let ellipsis = ellipsis_of_width(60.0);
        elide_line(&mut last, &ellipsis, 40.0);
        assert_eq!(last.glyphs.len(), 2, "the letter and the ellipsis");
    }

    #[test]
    fn the_ellipsis_is_a_real_ellipsis_character() {
        assert_eq!(ELLIPSIS, "\u{2026}");
    }
}
