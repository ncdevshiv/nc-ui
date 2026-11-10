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
    Face as HbFace, GlyphBuffer, UnicodeBuffer,
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
        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(text);
        let glyphs = rustybuzz::shape(&face, &[], buffer);
        Ok(self.finish(&glyphs, size_px, ascender, descender))
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

    /// Turns a shaped buffer into pixels.
    fn finish(
        &self,
        glyphs: &GlyphBuffer,
        size_px: f32,
        ascender: i16,
        descender: i16,
    ) -> ShapedText {
        let scale = if self.units_per_em == 0 {
            1.0
        } else {
            size_px / f32::from(self.units_per_em)
        };
        let infos = glyphs.glyph_infos();
        let positions = glyphs.glyph_positions();
        let mut out = Vec::with_capacity(infos.len());
        for (info, position) in infos.iter().zip(positions.iter()) {
            out.push(ShapedGlyph {
                glyph_id: info.glyph_id as u16,
                cluster: info.cluster,
                x_advance: position.x_advance as f32 * scale,
                x_offset: position.x_offset as f32 * scale,
                y_offset: position.y_offset as f32 * scale,
            });
        }
        let width = out.iter().map(|glyph| glyph.x_advance).sum();
        ShapedText {
            glyphs: out,
            width,
            ascent: f32::from(ascender) * scale,
            descent: f32::from(descender) * scale,
        }
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
}
