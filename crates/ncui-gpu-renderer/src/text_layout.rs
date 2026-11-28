//! The glyph atlas: packing rasterised coverage into one texture, and placing shaped
//! glyphs where the text run asked for them.
//!
//! Text on a GPU is textured quads: the glyph's coverage lives in a texture, the quad
//! samples it, and the coverage byte modulates the colour's alpha. The atlas is the
//! texture half of that — a single `R8` image every glyph shares, because one texture
//! per glyph is one bind group per glyph and a draw call per glyph, and a line of text
//! is forty glyphs.
//!
//! # What is pure and what is not
//!
//! Everything in this module is CPU arithmetic over bytes: the packing, the coverage
//! copy, the shaped-glyph placement. The GPU mirror — the texture these bytes are
//! uploaded into — is `render::wgpu_pass`'s business, and it reads this module's dirty
//! list rather than diffing a megabyte per frame. The split means the whole layout
//! path is testable on a machine with no display, which is the same argument
//! [`crate::render::RenderPlan`] makes for vertices.
//!
//! # Why shelf packing
//!
//! Glyphs in a run share a size, so they share a height, and shelves waste almost
//! nothing on them. A skyline or a binary-tree packer is more clever for rectangles
//! that differ wildly; text does not, and cleverness in packing is complexity the
//! content never pays for. When the atlas fills the answer is a refusal — an editor's
//! chrome at two or three sizes is a few hundred glyphs, a megabyte of `R8` holds
//! thousands, and a producer that exhausts it is leaking sizes, which the refusal
//! names.

use std::collections::HashMap;

use crate::atlas::GlyphCache;
use crate::text::Font;

/// The atlas edge, in pixels. One megabyte of coverage: a full editor chrome at three
/// sizes is a few percent of it.
pub const ATLAS_SIZE: u32 = 1024;

/// The gap between packed glyphs. Coverage is sampled with a nearest filter, so a
/// single pixel is enough — the padding exists so a glyph's edge pixels never share a
/// texel row with the next glyph's, which fractional quad positions would otherwise
/// occasionally sample.
pub const GLYPH_PADDING: u32 = 1;

/// Bytes per atlas pixel. The atlas is `Rgba8` — not `R8` — because glyphs are not the
/// only content it holds: bitmap blits are full-colour, and one texture sampled by one
/// shader path is the design. A glyph's coverage lives in the alpha channel of white
/// pixels, so the fragment multiply that draws a bitmap's own colour draws a glyph's
/// colour exactly the same way. The cost is four bytes where one would do for glyphs;
/// the win is there is one atlas, one bind group and no branch anywhere.
pub const ATLAS_BPP: u32 = 4;

/// Why text could not be laid out into the atlas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtlasError {
    /// The atlas has no room left. The honest answer to a leak of sizes or of glyphs:
    /// eviction is a policy, and a policy chosen silently is a bug with a rate limit.
    Full,
    /// Shaping the run failed.
    Shaping {
        /// What the shaper said.
        reason: String,
    },
}

impl core::fmt::Display for AtlasError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Full => write!(
                f,
                "the glyph atlas is full: too many distinct glyphs or sizes"
            ),
            Self::Shaping { reason } => write!(f, "the run could not be shaped: {reason}"),
        }
    }
}

impl std::error::Error for AtlasError {}

/// Where one glyph's coverage sits in the atlas.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtlasEntry {
    /// The left edge of the coverage within the texture, in pixels.
    pub x: u32,
    /// The top edge.
    pub y: u32,
    /// The coverage width.
    pub width: u32,
    /// The coverage height.
    pub height: u32,
}

impl AtlasEntry {
    /// The entry as texture coordinates, inset by half a pixel so a quad edge samples
    /// the centre of the edge texel rather than the boundary between two.
    #[must_use]
    pub fn uv(&self) -> [f32; 4] {
        let size = ATLAS_SIZE as f32;
        [
            (self.x as f32 + 0.5) / size,
            (self.y as f32 + 0.5) / size,
            (self.x as f32 + self.width as f32 - 0.5) / size,
            (self.y as f32 + self.height as f32 - 0.5) / size,
        ]
    }
}

/// A region of the atlas whose bytes changed since the last upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyRegion {
    /// The left edge, in pixels.
    pub x: u32,
    /// The top edge.
    pub y: u32,
    /// The width.
    pub width: u32,
    /// The height.
    pub height: u32,
}

/// The CPU mirror of the content texture: shelf packing, pixel bytes, dirty tracking.
///
/// Glyph coverage and bitmap pixels share the one texture, because the pipeline is one
/// textured path: a glyph's texels are white with coverage in alpha, a bitmap's are its
/// own pixels, and the fragment multiply that draws the one draws the other. The mirror
/// holds every byte the texture holds, because an upload after a device loss is a copy,
/// not a re-rasterisation of every glyph the frame happens to use.
#[derive(Debug)]
pub struct ContentAtlas {
    /// The bytes, row-major, four per pixel.
    bytes: Vec<u8>,
    /// Where glyphs are packed, by glyph and the bits of the rasterisation size — the
    /// same key the [`GlyphCache`] uses, so a glyph is packed at most once per size.
    entries: HashMap<(u16, u32), AtlasEntry>,
    /// Where bitmaps are packed, by the store key they were packed for. A key reinserted
    /// with new pixels packs again somewhere else — the old space is dead until eviction
    /// lands, and the atlas-full refusal is what keeps that leak honest rather than silent.
    bitmap_entries: HashMap<String, AtlasEntry>,
    /// The shelf cursor: the x the next glyph starts at within the current shelf.
    cursor_x: u32,
    /// The top of the current shelf.
    cursor_y: u32,
    /// The height of the current shelf — the tallest glyph in it plus padding.
    shelf_height: u32,
    /// What changed since the last upload, as rectangles of pixels.
    dirty: Vec<DirtyRegion>,
}

impl Default for ContentAtlas {
    fn default() -> Self {
        Self::new()
    }
}

impl ContentAtlas {
    /// An empty atlas with its first texel white.
    ///
    /// The white texel is what solid quads sample: the pipeline runs one textured path
    /// for everything, and a rectangle is a quad over coverage that is uniformly one.
    /// Baking it in means the shader has no branch and the solid path has no special
    /// case — which is what keeps a rectangle and a glyph in one draw call.
    #[must_use]
    pub fn new() -> Self {
        let mut bytes = vec![0u8; (ATLAS_SIZE * ATLAS_SIZE * ATLAS_BPP) as usize];
        // The white texel: white in colour, one in alpha. Solid quads sample it, so
        // what comes out of the multiply is their own colour unchanged.
        bytes[0] = 255;
        bytes[1] = 255;
        bytes[2] = 255;
        bytes[3] = 255;
        Self {
            bytes,
            entries: HashMap::new(),
            bitmap_entries: HashMap::new(),
            // The first shelf starts one pixel in: the white texel owns (0,0) and the
            // padding rule applies to it like anything else.
            cursor_x: GLYPH_PADDING,
            cursor_y: GLYPH_PADDING,
            shelf_height: 0,
            dirty: vec![DirtyRegion {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        }
    }

    /// The texture coordinates of the white texel — what every solid quad samples.
    #[must_use]
    pub fn white_uv() -> [f32; 2] {
        let size = ATLAS_SIZE as f32;
        [0.5 / size, 0.5 / size]
    }

    /// The bytes, row-major — what the GPU upload copies from.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The regions that changed since the last `take_dirty`.
    #[must_use]
    pub fn dirty(&self) -> &[DirtyRegion] {
        &self.dirty
    }

    /// Marks the atlas uploaded: the dirty list starts over.
    pub fn take_dirty(&mut self) {
        self.dirty.clear();
    }

    /// The entry for a glyph already packed, keyed the way the [`GlyphCache`] keys.
    #[must_use]
    pub fn entry(&self, glyph_id: u16, size_px: f32) -> Option<AtlasEntry> {
        self.entries.get(&(glyph_id, size_px.to_bits())).copied()
    }

    /// Allocates shelf space for a rectangle of pixels — the one piece of packing
    /// glyphs and bitmaps share.
    fn allocate(&mut self, width: u32, height: u32) -> Result<AtlasEntry, AtlasError> {
        if width + GLYPH_PADDING * 2 > ATLAS_SIZE {
            return Err(AtlasError::Full);
        }
        if self.cursor_x + width + GLYPH_PADDING > ATLAS_SIZE {
            // The shelf is out of width: close it and open the next one.
            self.cursor_y += self.shelf_height + GLYPH_PADDING;
            self.cursor_x = GLYPH_PADDING;
            self.shelf_height = 0;
        }
        if self.cursor_y + height + GLYPH_PADDING > ATLAS_SIZE {
            return Err(AtlasError::Full);
        }
        let entry = AtlasEntry {
            x: self.cursor_x,
            y: self.cursor_y,
            width,
            height,
        };
        self.cursor_x += width + GLYPH_PADDING;
        self.shelf_height = self.shelf_height.max(height);
        Ok(entry)
    }

    /// Packs a bitmap's pixels under its store key, or returns the entry already there.
    ///
    /// The same key packed again returns the first packing: the atlas cannot know the
    /// pixels changed, so replacing a bitmap means the store hands out a new key — the
    /// versioned-key pattern the bitmap store documents.
    ///
    /// # Errors
    ///
    /// Returns [`AtlasError::Full`] when the bitmap fits in no shelf.
    pub fn pack_bitmap(
        &mut self,
        key: &str,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<AtlasEntry, AtlasError> {
        if let Some(entry) = self.bitmap_entries.get(key) {
            return Ok(*entry);
        }
        if width == 0 || height == 0 || rgba.len() != (width * height * 4) as usize {
            return Err(AtlasError::Shaping {
                reason: "bitmap dimensions and pixel length disagree".to_owned(),
            });
        }
        let entry = self.allocate(width, height)?;
        for row in 0..height {
            let dst = (((entry.y + row) * ATLAS_SIZE + entry.x) * ATLAS_BPP) as usize;
            let src = (row * width * 4) as usize;
            let len = (width * 4) as usize;
            self.bytes[dst..dst + len].copy_from_slice(&rgba[src..src + len]);
        }
        self.dirty.push(DirtyRegion {
            x: entry.x,
            y: entry.y,
            width,
            height,
        });
        self.bitmap_entries.insert(key.to_owned(), entry);
        Ok(entry)
    }

    /// Packs a coverage into the atlas, or returns the entry it already has.
    ///
    /// A coverage that draws nothing never reaches here — the caller knows it draws
    /// nothing from the cache and skips it — so every entry is at least one pixel.
    ///
    /// # Errors
    ///
    /// Returns [`AtlasError::Full`] when the glyph fits in no shelf and a new shelf
    /// fits nowhere.
    pub fn pack(
        &mut self,
        glyph_id: u16,
        size_px: f32,
        coverage: &crate::glyphs::Coverage,
    ) -> Result<AtlasEntry, AtlasError> {
        let key = (glyph_id, size_px.to_bits());
        if let Some(entry) = self.entries.get(&key) {
            return Ok(*entry);
        }
        let width = coverage.width.max(1);
        let height = coverage.height.max(1);
        if width + GLYPH_PADDING * 2 > ATLAS_SIZE {
            // A glyph wider than the atlas can never pack; saying so is more useful
            // than a full-atlas error, because the producer cannot fix it by using
            // fewer glyphs.
            return Err(AtlasError::Full);
        }
        let entry = self.allocate(width, height)?;
        let entry = AtlasEntry {
            width: coverage.width,
            height: coverage.height,
            ..entry
        };
        for row in 0..coverage.height {
            for column in 0..coverage.width {
                let dst = (((entry.y + row) * ATLAS_SIZE + entry.x + column) * ATLAS_BPP) as usize;
                let alpha = coverage.alpha[(row * coverage.width + column) as usize];
                self.bytes[dst] = 255;
                self.bytes[dst + 1] = 255;
                self.bytes[dst + 2] = 255;
                self.bytes[dst + 3] = alpha;
            }
        }
        self.dirty.push(DirtyRegion {
            x: entry.x,
            y: entry.y,
            width: coverage.width,
            height: coverage.height,
        });
        self.cursor_x += width + GLYPH_PADDING;
        self.shelf_height = self.shelf_height.max(height);
        self.entries.insert(key, entry);
        Ok(entry)
    }

    /// How many distinct glyphs are packed.
    #[must_use]
    pub fn packed(&self) -> usize {
        self.entries.len()
    }
}

/// One glyph of a laid-out run: where its quad goes and what its coverage samples.
///
/// Positions are in the caller's coordinate space — content coordinates for a display
/// list — because the run origin the caller passes is in that space already. The quad
/// is the coverage's bitmap, axis-aligned; clipping it is the consumer's business, and
/// clipping a textured quad means moving its texture coordinates proportionally, which
/// is why the entry's full uv is carried rather than a lookup key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionedGlyph {
    /// The quad's left edge.
    pub x: f32,
    /// The quad's top edge.
    pub y: f32,
    /// The quad's width.
    pub width: f32,
    /// The quad's height.
    pub height: f32,
    /// The coverage in the atlas, as left, top, right, bottom texture coordinates.
    pub uv: [f32; 4],
}

/// Lays out one run of text: shapes it, packs its glyphs, positions their quads.
///
/// `origin` is the text's baseline origin — the same semantics the display list's
/// `TextRun` carries. Glyphs whose coverage is empty — spaces, control characters —
/// advance the pen and produce no quad, which is exactly what drawing them would do.
///
/// The font comes from the cache rather than as a separate argument: a cache and a
/// font that disagreed would position glyphs from one font with coverage from another,
/// and the type system can rule that out by making the disagreement impossible.
///
/// # Errors
///
/// Returns [`AtlasError::Shaping`] when the run cannot be shaped and
/// [`AtlasError::Full`] when a glyph fits nowhere.
pub fn layout_run(
    cache: &GlyphCache,
    atlas: &mut ContentAtlas,
    origin: (f32, f32),
    text: &str,
    size_px: f32,
) -> Result<Vec<PositionedGlyph>, AtlasError> {
    let font: &Font = cache.font();
    let shaped = font
        .shape(text, size_px)
        .map_err(|error| AtlasError::Shaping {
            reason: error.to_string(),
        })?;
    let mut glyphs = Vec::with_capacity(shaped.len());
    let mut pen_x = origin.0;
    for glyph in &shaped.glyphs {
        let coverage = cache.coverage(glyph.glyph_id, size_px);
        if let Some(coverage) = coverage
            && !coverage.is_empty()
        {
            let entry = atlas.pack(glyph.glyph_id, size_px, &coverage)?;
            // The outline's coordinates are the font's — y grows upward — and the
            // coverage's origin is where its top-left pixel sits in them. Screen
            // coordinates grow downward, so the bitmap's top edge in them is the
            // baseline minus the outline-space distance to the top. The shaped
            // offsets are font-space too: x_offset along the pen, y_offset up.
            let quad_x = pen_x + glyph.x_offset + coverage.origin_x;
            let quad_y = origin.1 - glyph.y_offset - coverage.origin_y;
            glyphs.push(PositionedGlyph {
                x: quad_x,
                y: quad_y,
                width: coverage.width as f32,
                height: coverage.height as f32,
                uv: entry.uv(),
            });
        }
        pen_x += glyph.x_advance;
    }
    Ok(glyphs)
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    reason = "test assertions must abort loudly on failure; fontless-machine skips report on stderr"
)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::default_font;

    fn cache() -> Option<GlyphCache> {
        default_font().ok().map(GlyphCache::new)
    }

    /// The glyph id a font gives a letter, because a glyph id is an index into the
    /// font's own tables, not a codepoint — 'H' as a number is either, depending on
    /// who is asked, and only one of those is right.
    fn glyph_id(cache: &GlyphCache, letter: &str) -> Option<u16> {
        cache
            .font()
            .shape(letter, 16.0)
            .ok()?
            .glyphs
            .first()
            .map(|g| g.glyph_id)
    }

    #[test]
    fn the_white_texel_is_white() {
        let atlas = ContentAtlas::new();
        assert_eq!(atlas.bytes()[0], 255);
        assert_eq!(atlas.bytes()[3], 255);
        assert_eq!(atlas.bytes()[4], 0, "nothing else is lit at birth");
    }

    #[test]
    fn a_glyph_packs_once_per_size() {
        let Some(cache) = cache() else {
            eprintln!("no system font on this machine; skipping");
            return;
        };
        let mut atlas = ContentAtlas::new();
        let h = glyph_id(&cache, "H").expect("H shapes in any latin font");
        let coverage = cache.coverage(h, 16.0).expect("H rasterises in any font");
        let first = atlas.pack(h, 16.0, &coverage).expect("packs");
        let second = atlas.pack(h, 16.0, &coverage).expect("packs again");
        assert_eq!(first, second, "the same key reuses the same entry");
        assert_eq!(atlas.packed(), 1);
    }

    #[test]
    fn packed_bytes_match_the_coverage() {
        let Some(cache) = cache() else {
            eprintln!("no system font on this machine; skipping");
            return;
        };
        let mut atlas = ContentAtlas::new();
        let h = glyph_id(&cache, "H").expect("H shapes");
        let coverage = cache.coverage(h, 16.0).expect("H rasterises");
        let entry = atlas.pack(h, 16.0, &coverage).expect("packs");
        // A pixel the coverage lights must be lit in the atlas at the entry's position.
        let mut checked = false;
        for y in 0..coverage.height {
            for x in 0..coverage.width {
                let alpha = coverage.at(x, y).unwrap_or(0);
                if alpha > 0 {
                    let at = (((entry.y + y) * ATLAS_SIZE + entry.x + x) * ATLAS_BPP) as usize;
                    assert_eq!(
                        atlas.bytes()[at + 3],
                        alpha,
                        "pixel ({x},{y}) alpha must copy exactly"
                    );
                    assert_eq!(atlas.bytes()[at], 255, "glyph pixels are white in colour");
                    checked = true;
                }
            }
        }
        assert!(checked, "an H at 16px lights some pixel");
    }

    #[test]
    fn shelves_advance_and_never_overlap() {
        let mut atlas = ContentAtlas::new();
        // Hand-built coverages of different heights force shelf breaks the test controls.
        let mut entries = Vec::new();
        for index in 0..50u16 {
            let height = 10 + u32::from(index % 5) * 3;
            let coverage = crate::glyphs::Coverage {
                width: 20,
                height,
                origin_x: 0.0,
                origin_y: 0.0,
                alpha: vec![255; (20 * height) as usize],
            };
            entries.push(atlas.pack(index, 16.0, &coverage).expect("packs"));
        }
        for (i, a) in entries.iter().enumerate() {
            for b in entries.iter().skip(i + 1) {
                let overlap = a.x < b.x + b.width
                    && b.x < a.x + a.width
                    && a.y < b.y + b.height
                    && b.y < a.y + a.height;
                assert!(!overlap, "entries {a:?} and {b:?} overlap");
            }
        }
    }

    #[test]
    fn a_full_atlas_refuses_rather_than_corrupting() {
        let mut atlas = ContentAtlas::new();
        // Fill the atlas with one tall glyph per shelf column until packing must fail.
        let tall = crate::glyphs::Coverage {
            width: ATLAS_SIZE - GLYPH_PADDING * 2,
            height: ATLAS_SIZE / 2,
            origin_x: 0.0,
            origin_y: 0.0,
            alpha: vec![255; ((ATLAS_SIZE - GLYPH_PADDING * 2) * (ATLAS_SIZE / 2)) as usize],
        };
        atlas.pack(1, 16.0, &tall).expect("the first fits");
        let result = atlas.pack(2, 16.0, &tall);
        assert_eq!(result, Err(AtlasError::Full));
    }

    #[test]
    fn layout_positions_glyphs_along_the_pen() {
        let Some(cache) = cache() else {
            eprintln!("no system font on this machine; skipping");
            return;
        };
        let mut atlas = ContentAtlas::new();
        let glyphs = layout_run(&cache, &mut atlas, (10.0, 30.0), "HH", 16.0).expect("lays out");
        assert!(
            glyphs.len() >= 2,
            "HH is two drawable glyphs in any latin font, got {}",
            glyphs.len()
        );
        assert!(
            glyphs[1].x > glyphs[0].x,
            "the second H sits right of the first: {:?} then {:?}",
            glyphs[0],
            glyphs[1]
        );
        for glyph in &glyphs {
            assert!(glyph.width > 0.0 && glyph.height > 0.0);
            // The quad sits near the baseline: above it mostly, never below the
            // descent line by more than the size itself.
            assert!(glyph.y < 30.0 + 16.0, "glyph top {glyph:?} is sane");
            assert!(glyph.y + glyph.height > 30.0 - 16.0);
        }
    }

    #[test]
    fn a_space_advances_without_a_quad() {
        let Some(cache) = cache() else {
            eprintln!("no system font on this machine; skipping");
            return;
        };
        let mut atlas = ContentAtlas::new();
        let together = layout_run(&cache, &mut atlas, (0.0, 30.0), "HH", 16.0).expect("lays out");
        let apart = layout_run(&cache, &mut atlas, (0.0, 30.0), "H H", 16.0).expect("lays out");
        assert_eq!(apart.len(), together.len(), "the space draws nothing");
        assert!(
            apart[1].x > together[1].x,
            "the space still advances the pen: {} vs {}",
            apart[1].x,
            together[1].x
        );
    }

    #[test]
    fn dirty_regions_cover_every_pack() {
        let Some(cache) = cache() else {
            eprintln!("no system font on this machine; skipping");
            return;
        };
        let mut atlas = ContentAtlas::new();
        atlas.take_dirty();
        assert!(atlas.dirty().is_empty());
        layout_run(&cache, &mut atlas, (0.0, 30.0), "abc", 16.0).expect("lays out");
        assert!(
            atlas.dirty().len() >= 3,
            "three distinct letters pack three regions, got {}",
            atlas.dirty().len()
        );
    }
}
