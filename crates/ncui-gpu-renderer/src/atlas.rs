//! A cache of rasterised glyphs, so a frame that repeats a line does not rasterise it again.
//!
//! Shaping and rasterising are the two halves of drawing text, and they have different shapes of
//! cost. Shaping a line is one pass over its characters and depends on the whole line; rasterising a
//! glyph is a walk over its outline and depends on nothing but the glyph and the size. A page that
//! redraws the same text every frame — which is what a page does — pays the second cost again for
//! every glyph, every frame, to produce exactly the same coverage it produced last time.
//!
//! This is the answer to that: the coverage of a glyph is worked out once and handed out as a shared
//! value after that. The measurement that motivated it is in `crates/ncui-bench/benches/latency.rs`,
//! where rasterising one 16-pixel glyph medians 8.5 microseconds — a figure that is small once and
//! is a frame's whole budget when a line of forty of them is drawn sixty times a second.
//!
//! # Why a bound rather than growth
//!
//! A cache keyed by what a page asks for is a cache a page can grow: a page that draws a thousand
//! different strings at a hundred sizes would fill memory with coverage nobody will ask for again.
//! The cache holds a fixed number of glyphs and is cleared when it is full, which is the honest
//! behaviour — the alternative is a leak with a rate limit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::glyphs::Coverage;
use crate::text::Font;

/// How many glyphs one cache holds before it is cleared.
///
/// A glyph at a typical size is a few hundred bytes of coverage, so this is a few hundred kilobytes
/// — bounded, and large enough that a screen of text at one or two sizes fits.
pub const MAX_CACHED_GLYPHS: usize = 4_096;

/// What one entry holds: the coverage of a glyph, or the knowledge that it draws nothing.
type Entry = Option<Arc<Coverage>>;

/// The entries, by glyph and the bits of the size it was rasterised at.
type Entries = Mutex<HashMap<(u16, u32), Entry>>;

/// The coverage of the glyphs a font has been asked for, by glyph and size.
///
/// Shared rather than copied: handing out an `Arc` costs an atomic increment, and copying a
/// coverage map per glyph would cost more than rasterising it did.
#[derive(Debug)]
pub struct GlyphCache {
    /// The font the coverage comes from.
    font: Font,
    /// The coverage, by glyph and the size it was rasterised at.
    ///
    /// The size is a `f32` in the API and a `u32` here, because a float cannot be a map key: the
    /// bits of the value are what identify it, so two sizes that differ by less than a float can
    /// distinguish are two entries rather than one wrong one.
    entries: Entries,
}

impl GlyphCache {
    /// A cache over `font`.
    #[must_use]
    pub fn new(font: Font) -> Self {
        Self {
            font,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The font this cache rasterises with.
    #[must_use]
    pub fn font(&self) -> &Font {
        &self.font
    }

    /// The coverage of `glyph_id` at `size_px`, or `None` when the glyph draws nothing.
    ///
    /// A glyph that draws nothing — a space, or a glyph whose outline encloses no area — is cached
    /// as `None`, because working that out is as expensive as rasterising a glyph that does.
    #[must_use]
    pub fn coverage(&self, glyph_id: u16, size_px: f32) -> Option<Arc<Coverage>> {
        let key = (glyph_id, size_px.to_bits());
        {
            let entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = entries.get(&key) {
                return cached.clone();
            }
        }
        let computed = self
            .font
            .glyph_coverage(glyph_id, size_px)
            .ok()
            .flatten()
            .map(Arc::new);
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A cache that is full is cleared rather than grown: what is in it is worth less than the
        // memory it would take to keep, and clearing it costs one rasterisation per glyph after.
        if entries.len() >= MAX_CACHED_GLYPHS {
            entries.clear();
        }
        entries.insert(key, computed.clone());
        computed
    }

    /// How many glyphs are cached.
    #[must_use]
    pub fn cached(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Forgets everything, which is what a font or a size change needs.
    pub fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
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
    use crate::text::default_font;

    /// A font from the machine's own fonts, or none.
    fn system_font() -> Option<Font> {
        default_font().ok()
    }

    #[test]
    fn a_glyph_is_rasterised_once_and_handed_out_after_that() {
        let Some(font) = system_font() else {
            return;
        };
        let shaped = font.shape("o", 32.0).expect("shaped");
        let Some(glyph) = shaped.glyphs.first() else {
            return;
        };
        let cache = GlyphCache::new(font);
        assert_eq!(cache.cached(), 0);

        let first = cache
            .coverage(glyph.glyph_id, 32.0)
            .expect("the letter draws");
        assert_eq!(cache.cached(), 1, "one glyph is cached");
        let second = cache
            .coverage(glyph.glyph_id, 32.0)
            .expect("the letter draws");
        // The same value, not a copy: a second rasterisation would produce an equal map in a
        // different allocation, and this is what tells the two apart.
        assert!(
            Arc::ptr_eq(&first, &second),
            "the second ask must be answered from the cache"
        );
        assert_eq!(cache.cached(), 1, "and must not add a second entry");

        // A different size is a different glyph as far as coverage is concerned.
        let larger = cache
            .coverage(glyph.glyph_id, 64.0)
            .expect("the letter draws");
        assert_eq!(cache.cached(), 2, "a second size is a second entry");
        assert!(!Arc::ptr_eq(&first, &larger));
    }

    #[test]
    fn a_glyph_that_draws_nothing_is_cached_as_drawing_nothing() {
        // Working out that a space draws nothing costs what rasterising a glyph costs, so the
        // answer is remembered the same way.
        let Some(font) = system_font() else {
            return;
        };
        let shaped = font.shape(" ", 32.0).expect("shaped");
        let Some(glyph) = shaped.glyphs.first() else {
            return;
        };
        let cache = GlyphCache::new(font);
        assert!(cache.coverage(glyph.glyph_id, 32.0).is_none());
        assert_eq!(cache.cached(), 1, "the answer is cached");

        cache.clear();
        assert_eq!(cache.cached(), 0, "clearing forgets everything");
    }
}
