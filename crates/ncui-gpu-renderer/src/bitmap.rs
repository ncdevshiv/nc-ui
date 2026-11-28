//! The bitmap store: the pixels a blit command names, held by the host.
//!
//! A display list's `Blit` carries a key rather than pixels because the list is
//! retained — a filmstrip of forty thumbnails copied into every frame would multiply
//! memory by the frame count. The pixels live here instead: the application decodes a
//! thumbnail once, inserts it under a key of its choosing, and the blit commands name
//! that key. The render expansion resolves keys against the store and packs what it
//! finds into the content atlas.
//!
//! # Replacement and versioning
//!
//! Bitmaps in an editor are not static: a thumbnail arrives after its decode finishes,
//! a filmstrip sharpens as more frames land. Replacing a key is therefore a first-class
//! operation — but the atlas cannot overwrite a packing in place, because a frame
//! already planned may still sample it. The store's answer is generational keys: every
//! insert of the same key advances a generation, and the atlas packs under
//! `key#generation`. The old packing's space is dead weight until atlas eviction lands
//! — an honest cost, surfaced by the atlas-full refusal rather than hidden.
//!
//! # What a missing key means
//!
//! A blit whose key is absent draws nothing — the well stays transparent. That is not
//! the loud refusal text runs get, and the difference is deliberate: a text run's
//! content always exists (the string is in the command), so drawing nothing would be a
//! lie; a bitmap's content may genuinely not exist *yet* — the decode is in flight —
//! and a transparent well is the truth of that. An application that wants a stand-in
//! draws one as a rect beneath the blit, in the same stream.

use std::collections::HashMap;

/// One bitmap: dimensions and tightly-packed rgba bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct Bitmap {
    /// The width in pixels.
    pub width: u32,
    /// The height in pixels.
    pub height: u32,
    /// The pixels, row-major, four bytes per pixel.
    pub rgba: Vec<u8>,
}

impl Bitmap {
    /// A bitmap, or `None` when the dimensions and the byte count disagree — a bitmap
    /// that cannot agree with itself about its size is a defect in the producer, and
    /// drawing it anyway would sample memory that is not the image.
    #[must_use]
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Option<Self> {
        if width == 0 || height == 0 || rgba.len() != (width * height * 4) as usize {
            return None;
        }
        Some(Self {
            width,
            height,
            rgba,
        })
    }
}

/// The host's bitmaps, by key.
#[derive(Debug, Default)]
pub struct BitmapStore {
    /// The live bitmaps.
    bitmaps: HashMap<String, Bitmap>,
    /// How many times each key has been inserted — the generation the atlas packs
    /// under, so a replacement never collides with the packing a planned frame uses.
    generations: HashMap<String, u32>,
}

impl BitmapStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a bitmap under `key`.
    ///
    /// Returns `false` — the insertion refused — when the bitmap's dimensions and pixel
    /// bytes disagree. Replacement is the normal case documented above: the generation
    /// advances, the next frame packs the new pixels, and frames already planned keep
    /// sampling the packing they were planned against.
    pub fn insert(&mut self, key: &str, width: u32, height: u32, rgba: Vec<u8>) -> bool {
        let Some(bitmap) = Bitmap::new(width, height, rgba) else {
            return false;
        };
        *self.generations.entry(key.to_owned()).or_insert(0) += 1;
        self.bitmaps.insert(key.to_owned(), bitmap);
        true
    }

    /// The bitmap under `key`, if one is live.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Bitmap> {
        self.bitmaps.get(key)
    }

    /// The key the atlas packs this bitmap under: the store key plus its generation.
    #[must_use]
    pub fn packing_key(&self, key: &str) -> Option<String> {
        self.generations
            .get(key)
            .map(|generation| format!("{key}#{generation}"))
    }

    /// Removes a bitmap. A blit that still names it draws the transparent well —
    /// removal is how an application says the content is gone, and the well is the
    /// truth of gone.
    pub fn remove(&mut self, key: &str) {
        self.bitmaps.remove(key);
    }

    /// How many bitmaps are live.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bitmaps.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bitmaps.is_empty()
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bitmap_whose_bytes_disagree_with_its_size_is_refused() {
        assert!(Bitmap::new(4, 4, vec![0; 63]).is_none());
        assert!(Bitmap::new(4, 4, vec![0; 65]).is_none());
        assert!(Bitmap::new(0, 4, vec![0; 0]).is_none());
        assert!(Bitmap::new(4, 4, vec![0; 64]).is_some());
    }

    #[test]
    fn insertion_and_replacement_advance_the_generation() {
        let mut store = BitmapStore::new();
        assert!(store.insert("thumb:1", 2, 2, vec![0; 16]));
        let first = store.packing_key("thumb:1").unwrap();
        assert!(store.insert("thumb:1", 2, 2, vec![255; 16]));
        let second = store.packing_key("thumb:1").unwrap();
        assert_ne!(first, second, "a replacement packs elsewhere");
    }

    #[test]
    fn a_disagreeing_insert_changes_nothing() {
        let mut store = BitmapStore::new();
        assert!(!store.insert("bad", 2, 2, vec![0; 3]));
        assert!(store.get("bad").is_none());
        assert!(store.packing_key("bad").is_none());
    }

    #[test]
    fn removal_leaves_no_bitmap() {
        let mut store = BitmapStore::new();
        store.insert("thumb:1", 2, 2, vec![0; 16]);
        store.remove("thumb:1");
        assert!(store.get("thumb:1").is_none());
        assert!(store.is_empty());
    }
}
