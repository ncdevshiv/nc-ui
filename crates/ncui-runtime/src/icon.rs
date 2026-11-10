//! Reading an application's icon: a file on disk into the pixels a window carries.
//!
//! A window's icon is a file an author drops into an application directory and names in the
//! manifest. Every platform wants the same thing at the end — straight RGBA, top row first — so
//! something has to read the file, and that something is here rather than in the window host: the
//! host is the only place that knows a platform's window-creation call, and a host that also knew
//! the byte layout of an icon file would be two unrelated decisions in one place.
//!
//! # What is read, and what is refused
//!
//! Two containers, because they are the two the platforms themselves use: BMP, and ICO, which is a
//! directory of BMPs at several sizes. Inside either, the uncompressed (`BI_RGB`) bitmap at 1, 4, 8,
//! 24 or 32 bits per pixel — every depth an icon is actually stored at.
//!
//! Everything else is refused with a message naming what was found and why it cannot be read: a
//! PNG-compressed ICO entry, a run-length compressed bitmap, a bit depth no icon uses. A refusal is
//! the honest answer here — a decoder that guessed at a format it does not know would hand the
//! platform pixels that are not the picture the author drew — and it is reported to the person who
//! named the file, which is the person who can replace it.

use ncui_webview::{Icon, MAX_ICON_EDGE};

/// The largest icon file that will be read.
///
/// An icon is a handful of small bitmaps; the largest real one is a few hundred kilobytes. A file
/// past this is either not an icon or is a header claiming dimensions it does not have, and reading
/// it would spend memory on a picture no window can show.
pub const MAX_ICON_FILE_BYTES: usize = 4 * 1024 * 1024;

/// The signature every PNG file starts with, which an ICO entry may hold in place of a bitmap.
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// An icon file that cannot be turned into pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconError {
    /// The file is empty.
    Empty,
    /// The file is larger than any icon, so it is not read.
    TooLarge {
        /// How many bytes it holds.
        bytes: usize,
    },
    /// The bytes are a format this decoder does not read.
    Unsupported {
        /// The container the bytes turned out to be.
        format: &'static str,
        /// Why it cannot be read.
        reason: String,
    },
    /// The file ends before the part being read does.
    Truncated {
        /// The container being read.
        format: &'static str,
        /// How many bytes the part needs.
        needed: usize,
        /// How many bytes are there.
        got: usize,
    },
    /// The image claims a size an image cannot have.
    BadDimensions {
        /// The width the header claims.
        width: i64,
        /// The height the header claims.
        height: i64,
    },
    /// The image is larger than a window's icon may be.
    TooBig {
        /// The width the header claims.
        width: u32,
        /// The height the header claims.
        height: u32,
    },
}

impl core::fmt::Display for IconError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => f.write_str("the file is empty, so there is no image in it"),
            Self::TooLarge { bytes } => write!(
                f,
                "the file is {bytes} bytes, past the {MAX_ICON_FILE_BYTES} an icon may be"
            ),
            Self::Unsupported { format, reason } => {
                write!(f, "{format} cannot be read as an icon: {reason}")
            }
            Self::Truncated {
                format,
                needed,
                got,
            } => write!(
                f,
                "{format} ends early: {needed} bytes are needed and the file holds {got}"
            ),
            Self::BadDimensions { width, height } => {
                write!(f, "an image of {width}x{height} is not an image")
            }
            Self::TooBig { width, height } => write!(
                f,
                "an icon of {width}x{height} is larger than the {MAX_ICON_EDGE} pixels an edge may be"
            ),
        }
    }
}

impl std::error::Error for IconError {}

/// Reads `bytes` as an icon, in whichever of the two containers they are.
///
/// # Errors
///
/// Returns [`IconError`] when the bytes are not a BMP or an ICO, when they are damaged, or when the
/// image they describe is not one a window can carry.
pub fn decode(bytes: &[u8]) -> Result<Icon, IconError> {
    if bytes.is_empty() {
        return Err(IconError::Empty);
    }
    if bytes.len() > MAX_ICON_FILE_BYTES {
        return Err(IconError::TooLarge { bytes: bytes.len() });
    }
    if bytes.starts_with(b"BM") {
        return decode_bmp(bytes);
    }
    // An icon directory: reserved zero, type one. Read before anything else so a file that is an
    // ICO is never mistaken for something else by a header that happens to look similar.
    if u16_at(bytes, 0) == Some(0) && u16_at(bytes, 2) == Some(1) {
        return decode_ico(bytes);
    }
    Err(IconError::Unsupported {
        format: "the file",
        reason: "it is neither a BMP (which starts with `BM`) nor an icon directory".to_owned(),
    })
}

/// Reads a Windows bitmap file.
///
/// # Errors
///
/// Returns [`IconError`] when the file is not a bitmap this decoder reads.
pub fn decode_bmp(bytes: &[u8]) -> Result<Icon, IconError> {
    const FORMAT: &str = "the bitmap";
    // The file header is 14 bytes: `BM`, the file size, two reserved words, and the offset the
    // pixels start at. The offset is authoritative, because a file may carry a gap between the
    // header and its pixels.
    if bytes.len() < 14 {
        return Err(IconError::Truncated {
            format: FORMAT,
            needed: 14,
            got: bytes.len(),
        });
    }
    if !bytes.starts_with(b"BM") {
        return Err(IconError::Unsupported {
            format: FORMAT,
            reason: "a bitmap file starts with `BM`".to_owned(),
        });
    }
    let pixels = u32_at(bytes, 10).ok_or(IconError::Truncated {
        format: FORMAT,
        needed: 14,
        got: bytes.len(),
    })?;
    // A width past what a pointer can name is a width the file cannot hold either, so it falls
    // through to the bounds check below rather than being refused twice.
    let pixels = usize::try_from(pixels).unwrap_or(usize::MAX);
    let header = read_header(bytes, 14, Some(pixels), FORMAT)?;
    let (icon, _) = read_pixels(bytes, &header, FORMAT)?;
    Ok(icon)
}

/// Reads an icon directory, choosing the largest image in it.
///
/// # Errors
///
/// Returns [`IconError`] when the directory is damaged, when it holds no image this decoder reads,
/// or when the chosen image cannot be decoded.
pub fn decode_ico(bytes: &[u8]) -> Result<Icon, IconError> {
    const FORMAT: &str = "the icon";
    if bytes.len() < 6 {
        return Err(IconError::Truncated {
            format: FORMAT,
            needed: 6,
            got: bytes.len(),
        });
    }
    let count = u16_at(bytes, 4).unwrap_or(0);
    if count == 0 {
        return Err(IconError::Unsupported {
            format: FORMAT,
            reason: "the directory holds no images".to_owned(),
        });
    }
    let count = usize::from(count);
    let entries_end = 6 + count * 16;
    if bytes.len() < entries_end {
        return Err(IconError::Truncated {
            format: FORMAT,
            needed: entries_end,
            got: bytes.len(),
        });
    }
    // Every entry that fits in the file and names a size an icon may be. An entry that does not is
    // skipped rather than fatal: an icon file with one damaged image and three good ones still has
    // an image in it, and refusing the whole file would throw away the three.
    let mut best: Option<Entry> = None;
    for index in 0..count {
        let at = 6 + index * 16;
        let Some(entry) = read_entry(bytes, at, FORMAT)? else {
            continue;
        };
        let better = best.as_ref().is_none_or(|current| {
            (entry.width * entry.height, entry.bits)
                > (current.width * current.height, current.bits)
        });
        if better {
            best = Some(entry);
        }
    }
    let Some(entry) = best else {
        return Err(IconError::Unsupported {
            format: FORMAT,
            reason: "no image in the directory is one a window can carry".to_owned(),
        });
    };
    let data = &bytes[entry.offset..entry.offset + entry.length];
    if data.starts_with(&PNG_SIGNATURE) {
        return Err(IconError::Unsupported {
            format: FORMAT,
            reason: "the largest image is a PNG, which needs a decompressor this runtime does not \
                     carry; store the icon as an uncompressed bitmap instead"
                .to_owned(),
        });
    }
    // An ICO entry holds a bare device-independent bitmap: the header at the start, and — unlike a
    // BMP file — no 14-byte file header, so the pixels follow the palette directly.
    let mut header = read_header(data, 0, None, FORMAT)?;
    // The height in an icon's bitmap header counts the picture *and* the mask that follows it, so
    // it is twice the picture's height. The directory says how tall the picture really is, and the
    // two are told apart by whether the header's height is the picture's height or twice it — a
    // writer that left the mask out may have written the picture's height alone.
    let doubled = entry.height.saturating_mul(2);
    if header.height != entry.height && header.height != doubled {
        return Err(IconError::Unsupported {
            format: FORMAT,
            reason: format!(
                "the directory says the image is {} pixels tall and its bitmap says {}",
                entry.height, header.height
            ),
        });
    }
    // The width has one answer in both places — the directory's byte and the bitmap's own field —
    // so a disagreement means one of them is wrong, and a picture read under the wrong width would
    // be a picture of the wrong shape.
    if header.width != entry.width {
        return Err(IconError::Unsupported {
            format: FORMAT,
            reason: format!(
                "the directory says the image is {} pixels wide and its bitmap says {}",
                entry.width, header.width
            ),
        });
    }
    header.height = entry.height;
    let (mut icon, alpha_was_absent) = read_pixels(data, &header, FORMAT)?;
    // The AND mask that follows the pixels is what gives an icon its transparency at depths with no
    // alpha channel. At 32 bits the alpha channel is used instead, and a writer that left it all
    // zero meant "no alpha information", which is what the mask is for.
    let mask_at = header.pixels.saturating_add(
        header
            .stride
            .saturating_mul(usize::try_from(header.height).unwrap_or(0)),
    );
    if header.bits < 32 || alpha_was_absent {
        if mask_at >= data.len() {
            return Err(IconError::Truncated {
                format: FORMAT,
                needed: mask_at.saturating_add(1),
                got: data.len(),
            });
        }
        apply_mask(&mut icon.rgba, data, mask_at, &header, FORMAT)?;
    }
    Ok(icon)
}

/// One image in an icon directory, as the directory describes it.
#[derive(Debug, Clone, Copy)]
struct Entry {
    /// The width the entry names, with zero meaning 256.
    width: u32,
    /// The height the entry names, with zero meaning 256.
    height: u32,
    /// The bit depth the entry names, used to break a tie between images of one size.
    bits: u16,
    /// Where the image starts in the file.
    offset: usize,
    /// How many bytes the image is.
    length: usize,
}

/// Reads one directory entry, or `None` when it names an image that is not in the file.
///
/// # Errors
///
/// Returns [`IconError::Unsupported`] when the entry is not an icon image at all.
fn read_entry(bytes: &[u8], at: usize, format: &'static str) -> Result<Option<Entry>, IconError> {
    let byte = |offset: usize| bytes.get(at + offset).copied().unwrap_or(0);
    // Zero in the width or height field means 256, which is the largest size the byte can name.
    let edge = |value: u8| if value == 0 { 256 } else { u32::from(value) };
    let width = edge(byte(0));
    let height = edge(byte(1));
    let planes = u16_at(bytes, at + 4).unwrap_or(0);
    let bits = u16_at(bytes, at + 6).unwrap_or(0);
    let length = u32_at(bytes, at + 8).unwrap_or(0);
    let offset = u32_at(bytes, at + 12).unwrap_or(0);
    if planes != 1 {
        return Err(IconError::Unsupported {
            format,
            reason: format!("an image in the directory names {planes} planes, and an icon has one"),
        });
    }
    if width > MAX_ICON_EDGE || height > MAX_ICON_EDGE {
        return Ok(None);
    }
    let (Ok(offset), Ok(length)) = (usize::try_from(offset), usize::try_from(length)) else {
        return Ok(None);
    };
    if length == 0 || offset.saturating_add(length) > bytes.len() {
        return Ok(None);
    }
    Ok(Some(Entry {
        width,
        height,
        bits,
        offset,
        length,
    }))
}

/// The parts of a device-independent bitmap header this decoder acts on.
#[derive(Debug, Clone, Copy)]
struct Header {
    /// The image's width in pixels.
    width: u32,
    /// The image's height in pixels, always positive.
    height: u32,
    /// Whether the rows are stored from the top down.
    top_down: bool,
    /// How many bits each pixel takes.
    bits: u16,
    /// Where the colour table starts, at the depths that have one.
    palette: usize,
    /// How many colours the table holds.
    colours: u32,
    /// Where the pixel array starts.
    pixels: usize,
    /// How many bytes one row of pixels takes, padded to a four-byte boundary.
    stride: usize,
}

/// Reads a device-independent bitmap header at `at`.
///
/// `pixels_override` is where a bitmap *file* says its pixels are; inside an icon directory there is
/// no such offset, and the pixels follow the palette, so the caller passes `None`.
///
/// # Errors
///
/// Returns [`IconError`] for a header this decoder does not read, or one that describes an image
/// that cannot exist.
fn read_header(
    bytes: &[u8],
    at: usize,
    pixels_override: Option<usize>,
    format: &'static str,
) -> Result<Header, IconError> {
    let header_size = u32_at(bytes, at).ok_or(IconError::Truncated {
        format,
        needed: at.saturating_add(4),
        got: bytes.len(),
    })?;
    let header_size = usize::try_from(header_size).unwrap_or(0);
    if header_size < 40 {
        return Err(IconError::Unsupported {
            format,
            reason: format!(
                "its header is {header_size} bytes, and the bitmap header this decoder reads is at \
                 least 40"
            ),
        });
    }
    if bytes.len() < at.saturating_add(header_size) {
        return Err(IconError::Truncated {
            format,
            needed: at.saturating_add(header_size),
            got: bytes.len(),
        });
    }
    let raw_width = i64::from(i32_at(bytes, at + 4).unwrap_or(0));
    let raw_height = i64::from(i32_at(bytes, at + 8).unwrap_or(0));
    if raw_width <= 0 || raw_height == 0 {
        return Err(IconError::BadDimensions {
            width: raw_width,
            height: raw_height,
        });
    }
    // A negative height means the rows are stored from the top down. That is the one thing the sign
    // is for; the magnitude is the image's height either way.
    let top_down = raw_height < 0;
    let width = u32::try_from(raw_width).unwrap_or(0);
    let height = u32::try_from(raw_height.unsigned_abs()).unwrap_or(u32::MAX);
    if width > MAX_ICON_EDGE || height > MAX_ICON_EDGE {
        return Err(IconError::TooBig { width, height });
    }
    let planes = u16_at(bytes, at + 12).unwrap_or(0);
    if planes != 1 {
        return Err(IconError::Unsupported {
            format,
            reason: format!("it names {planes} planes, and a bitmap has one"),
        });
    }
    let bits = u16_at(bytes, at + 14).unwrap_or(0);
    if !matches!(bits, 1 | 4 | 8 | 24 | 32) {
        return Err(IconError::Unsupported {
            format,
            reason: format!(
                "it stores {bits} bits per pixel, and the depths an icon uses are 1, 4, 8, 24 and 32"
            ),
        });
    }
    let compression = u32_at(bytes, at + 16).unwrap_or(0);
    if compression != 0 {
        return Err(IconError::Unsupported {
            format,
            reason: format!(
                "it is compressed (method {compression}); only uncompressed bitmaps are read, \
                 because a compressed one needs a decompressor per method"
            ),
        });
    }
    // The palette, at depths that have one. A header may name how many colours it used, and a
    // writer that named none meant "as many as the depth allows".
    let palette_entries = if bits <= 8 {
        let named = if header_size >= 36 {
            u32_at(bytes, at + 32).unwrap_or(0)
        } else {
            0
        };
        if named == 0 { 1_u32 << bits } else { named }
    } else {
        0
    };
    let palette = at.saturating_add(header_size);
    let palette_bytes = usize::try_from(palette_entries)
        .unwrap_or(0)
        .saturating_mul(4);
    let palette_end = palette.saturating_add(palette_bytes);
    let pixels = match pixels_override {
        Some(offset) if offset >= palette_end => offset,
        Some(_) | None => palette_end,
    };
    let stride = row_stride(width, bits);
    Ok(Header {
        width,
        height,
        top_down,
        bits,
        palette,
        colours: palette_entries,
        pixels,
        stride,
    })
}

/// How many bytes one row of `width` pixels at `bits` takes, padded to a four-byte boundary.
///
/// Every row of a bitmap starts on a four-byte boundary, whatever the pixels in it need. Reading
/// without the padding puts every row after the first at the wrong offset, which is the classic way
/// a hand-written bitmap reader produces a picture that is merely skewed.
fn row_stride(width: u32, bits: u16) -> usize {
    let bits = u32::from(bits);
    let per_row = width.saturating_mul(bits).saturating_add(31) / 32;
    usize::try_from(per_row).unwrap_or(0).saturating_mul(4)
}

/// Reads the pixel array of `header` into a whole icon.
///
/// The second value says whether the source carried an alpha channel that was entirely zero. At
/// 32 bits the format leaves that byte undefined, and a writer that left it zero meant "no alpha
/// information" — which is what the mask after the pixels is for, so the caller has to know before
/// the all-zero rule turns it into opaque.
///
/// # Errors
///
/// Returns [`IconError::Truncated`] when the file ends before the pixels do, and
/// [`IconError::Unsupported`] when the pixel array does not hold what the header claims.
fn read_pixels(
    bytes: &[u8],
    header: &Header,
    format: &'static str,
) -> Result<(Icon, bool), IconError> {
    let height = usize::try_from(header.height).unwrap_or(0);
    let width = usize::try_from(header.width).unwrap_or(0);
    let end = header
        .pixels
        .saturating_add(header.stride.saturating_mul(height));
    if bytes.len() < end {
        return Err(IconError::Truncated {
            format,
            needed: end,
            got: bytes.len(),
        });
    }
    let palette = read_palette(bytes, header, format)?;
    let depth = usize::from(header.bits / 8);
    let mut rgba = vec![0_u8; width.saturating_mul(height).saturating_mul(4)];
    let mut opaque_alpha = false;
    for row in 0..height {
        // The rows of a bitmap are stored from the bottom up unless the header's height is
        // negative, so a row is read from the end of the array as often as from the start.
        let source = if header.top_down {
            row
        } else {
            height - 1 - row
        };
        let start = header.pixels + source * header.stride;
        for column in 0..width {
            let destination = (row * width + column) * 4;
            let pixel = &mut rgba[destination..destination + 4];
            match header.bits {
                1 | 4 | 8 => {
                    let colour = palette
                        .get(packed_index(bytes, start, column, header.bits))
                        .copied()
                        .unwrap_or([0, 0, 0, 255]);
                    pixel.copy_from_slice(&colour);
                }
                _ => {
                    let at = start + column * depth;
                    let Some(window) = bytes.get(at..at + depth) else {
                        return Err(IconError::Truncated {
                            format,
                            needed: at + depth,
                            got: bytes.len(),
                        });
                    };
                    pixel[0] = window[2];
                    pixel[1] = window[1];
                    pixel[2] = window[0];
                    pixel[3] = if depth == 4 { window[3] } else { 255 };
                    opaque_alpha |= pixel[3] != 0;
                }
            }
        }
    }
    // A 32-bit uncompressed bitmap has an alpha byte the format leaves undefined, and writers
    // commonly leave it zero. A picture that is entirely transparent is never what a file meant, so
    // an all-zero alpha channel is read as opaque rather than as an icon nobody can see.
    let alpha_was_absent = header.bits == 32 && !opaque_alpha;
    if alpha_was_absent {
        let (pixels, _) = rgba.as_chunks_mut::<4>();
        for pixel in pixels.iter_mut() {
            pixel[3] = 255;
        }
    }
    Ok((
        Icon {
            width: header.width,
            height: header.height,
            rgba,
        },
        alpha_was_absent,
    ))
}

/// Reads the colour table of a paletted bitmap, as straight RGBA.
///
/// # Errors
///
/// Returns [`IconError::Truncated`] when the file ends before the table does.
fn read_palette(
    bytes: &[u8],
    header: &Header,
    format: &'static str,
) -> Result<Vec<[u8; 4]>, IconError> {
    if header.bits > 8 {
        return Ok(Vec::new());
    }
    let count = usize::try_from(header.colours).unwrap_or(0);
    let end = header.palette.saturating_add(count.saturating_mul(4));
    if bytes.len() < end {
        return Err(IconError::Truncated {
            format,
            needed: end,
            got: bytes.len(),
        });
    }
    // Each entry is four bytes in blue, green, red, unused order — the reverse of the order the
    // pixels come out in, which is why the conversion is here rather than at every lookup.
    let palette = (0..count)
        .map(|index| {
            let at = header.palette + index * 4;
            let entry = &bytes[at..at + 4];
            [entry[2], entry[1], entry[0], 255]
        })
        .collect();
    Ok(palette)
}

/// The palette index of one pixel of a paletted bitmap.
///
/// The pixels of a 1, 4 or 8-bit bitmap are packed into bytes, most significant first: eight
/// one-bit pixels to a byte, two four-bit pixels, or one eight-bit pixel.
fn packed_index(bytes: &[u8], row_start: usize, column: usize, bits: u16) -> usize {
    match bits {
        1 => {
            let byte = bytes.get(row_start + column / 8).copied().unwrap_or(0);
            usize::from((byte >> (7 - column % 8)) & 1)
        }
        4 => {
            let byte = bytes.get(row_start + column / 2).copied().unwrap_or(0);
            let nibble = if column.is_multiple_of(2) {
                byte >> 4
            } else {
                byte & 0x0F
            };
            usize::from(nibble)
        }
        _ => usize::from(bytes.get(row_start + column).copied().unwrap_or(0)),
    }
}

/// Applies the AND mask that follows an icon's pixels.
///
/// The mask has one bit per pixel, set where the pixel shows through the desktop behind the icon.
/// It is how an icon had transparency before alpha channels existed, and it is still what an icon
/// at a depth with no alpha channel relies on.
///
/// # Errors
///
/// Returns [`IconError::Truncated`] when the file ends before the mask does.
fn apply_mask(
    rgba: &mut [u8],
    bytes: &[u8],
    mask_at: usize,
    header: &Header,
    format: &'static str,
) -> Result<(), IconError> {
    let height = usize::try_from(header.height).unwrap_or(0);
    let width = usize::try_from(header.width).unwrap_or(0);
    let stride = row_stride(header.width, 1);
    let end = mask_at.saturating_add(stride.saturating_mul(height));
    if bytes.len() < end {
        return Err(IconError::Truncated {
            format,
            needed: end,
            got: bytes.len(),
        });
    }
    for row in 0..height {
        let source = if header.top_down {
            row
        } else {
            height - 1 - row
        };
        let start = mask_at + source * stride;
        for column in 0..width {
            let byte = bytes.get(start + column / 8).copied().unwrap_or(0);
            let transparent = (byte >> (7 - column % 8)) & 1 == 1;
            if transparent {
                let at = (row * width + column) * 4;
                if let Some(pixel) = rgba.get_mut(at..at + 4) {
                    pixel[3] = 0;
                }
            }
        }
    }
    Ok(())
}

/// Reads a little-endian 16-bit value, or `None` when the file ends before it does.
fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    let pair = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([pair[0], pair[1]]))
}

/// Reads a little-endian 32-bit value, or `None` when the file ends before it does.
fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let word = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
}

/// Reads a little-endian signed 32-bit value, or `None` when the file ends before it does.
fn i32_at(bytes: &[u8], offset: usize) -> Option<i32> {
    u32_at(bytes, offset).map(u32::cast_signed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_u16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn push_i32(out: &mut Vec<u8>, value: i32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    /// A device-independent bitmap header: the 40 bytes every uncompressed bitmap carries.
    fn dib_header(width: i32, height: i32, bits: u16, compression: u32, colours: u32) -> Vec<u8> {
        let mut out = Vec::new();
        push_u32(&mut out, 40);
        push_i32(&mut out, width);
        push_i32(&mut out, height);
        push_u16(&mut out, 1);
        push_u16(&mut out, bits);
        push_u32(&mut out, compression);
        push_u32(&mut out, 0);
        push_u32(&mut out, 0);
        push_u32(&mut out, 0);
        push_u32(&mut out, colours);
        push_u32(&mut out, 0);
        out
    }

    /// A device-independent bitmap: the 40-byte header, then the colour table, then the pixels.
    fn dib(
        width: i32,
        height: i32,
        bits: u16,
        compression: u32,
        colours: u32,
        palette: &[[u8; 3]],
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut out = dib_header(width, height, bits, compression, colours);
        for colour in palette {
            out.extend_from_slice(&[colour[2], colour[1], colour[0], 0]);
        }
        out.extend_from_slice(pixels);
        out
    }

    /// A bitmap file: the 14-byte file header, then a device-independent bitmap.
    fn bmp(width: i32, height: i32, bits: u16, compression: u32, pixels: &[u8]) -> Vec<u8> {
        let body = dib(width, height, bits, compression, 0, &[], pixels);
        let mut out = Vec::new();
        out.extend_from_slice(b"BM");
        push_u32(
            &mut out,
            u32::try_from(14 + body.len()).expect("a small file"),
        );
        push_u32(&mut out, 0);
        push_u32(&mut out, 14 + 40);
        out.extend_from_slice(&body);
        out
    }

    /// An icon directory over the images given as (width byte, height byte, bits, data).
    fn ico(images: &[(u8, u8, u16, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        push_u16(&mut out, 0);
        push_u16(&mut out, 1);
        push_u16(&mut out, u16::try_from(images.len()).expect("a few images"));
        let mut offset = 6 + images.len() * 16;
        for (width, height, bits, data) in images {
            out.push(*width);
            out.push(*height);
            out.push(0);
            out.push(0);
            push_u16(&mut out, 1);
            push_u16(&mut out, *bits);
            push_u32(&mut out, u32::try_from(data.len()).expect("a small image"));
            push_u32(&mut out, u32::try_from(offset).expect("a small file"));
            offset += data.len();
        }
        for (_, _, _, data) in images {
            out.extend_from_slice(data);
        }
        out
    }

    fn pixels(icon: &Icon) -> Vec<[u8; 4]> {
        icon.rgba.as_chunks::<4>().0.to_vec()
    }

    #[test]
    fn a_24_bit_bitmap_is_read_from_the_bottom_row_up() {
        // The defect this closes: a bitmap stores its rows from the bottom, so reading it in order
        // gives a picture that is upside down. Two rows of two pixels, stored bottom first.
        let mut data = Vec::new();
        // The bottom row: blue, white. Each row is padded to four bytes.
        data.extend_from_slice(&[0xFF, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0x00, 0x00]);
        // The top row: red, green.
        data.extend_from_slice(&[0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0x00]);
        let icon = decode(&bmp(2, 2, 24, 0, &data)).expect("a bitmap reads");
        assert_eq!(icon.width, 2);
        assert_eq!(icon.height, 2);
        assert!(icon.is_whole());
        assert_eq!(
            pixels(&icon),
            vec![
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [255, 255, 255, 255],
            ]
        );
    }

    #[test]
    fn a_top_down_bitmap_is_read_as_it_is_stored() {
        // A negative height means the rows are already in the order a window wants them, and
        // flipping them anyway would turn the picture upside down a second time.
        let data = [0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0x00];
        let icon = decode(&bmp(2, -1, 24, 0, &data)).expect("a bitmap reads");
        assert_eq!(pixels(&icon), vec![[255, 0, 0, 255], [0, 255, 0, 255]]);
    }

    #[test]
    fn a_32_bit_bitmap_keeps_the_alpha_it_carries() {
        let data = [0x1E, 0x14, 0x0A, 0x80];
        let icon = decode(&bmp(1, 1, 32, 0, &data)).expect("a bitmap reads");
        assert_eq!(pixels(&icon), vec![[10, 20, 30, 128]]);
    }

    #[test]
    fn a_32_bit_bitmap_whose_alpha_is_all_zero_is_read_as_opaque() {
        // The alpha byte of an uncompressed 32-bit bitmap is undefined, and writers leave it zero.
        // A picture nobody can see is never what the file meant.
        let data = [0x1E, 0x14, 0x0A, 0x00, 0x00, 0x00, 0xFF, 0x00];
        let icon = decode(&bmp(2, 1, 32, 0, &data)).expect("a bitmap reads");
        assert_eq!(pixels(&icon), vec![[10, 20, 30, 255], [255, 0, 0, 255]]);
    }

    #[test]
    fn a_paletted_bitmap_is_read_through_its_colour_table() {
        // Eight-bit pixels, one per byte, with a two-entry table. The table is blue, green, red
        // order and the pixels come out red, green, blue order.
        let body = {
            let mut out = dib_header(3, 1, 8, 0, 2);
            // Entry zero is white, entry one is red, both in blue, green, red order.
            out.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0x00]);
            out.extend_from_slice(&[0x00, 0x00, 0xFF, 0x00]);
            // Three pixels and one byte of padding to the four-byte row boundary.
            out.extend_from_slice(&[0, 1, 0, 0]);
            out
        };
        let mut file = Vec::new();
        file.extend_from_slice(b"BM");
        push_u32(&mut file, u32::try_from(14 + body.len()).expect("small"));
        push_u32(&mut file, 0);
        push_u32(&mut file, 14 + 40 + 8);
        file.extend_from_slice(&body);
        let icon = decode(&file).expect("a paletted bitmap reads");
        assert_eq!(
            pixels(&icon),
            vec![[255, 255, 255, 255], [255, 0, 0, 255], [255, 255, 255, 255],]
        );
    }

    #[test]
    fn an_icon_directory_is_read_at_its_largest_image() {
        // An icon file holds the same picture at several sizes, and the window wants the largest:
        // a platform handed the 16-pixel one would scale it up into a blur.
        // Each entry holds a picture and the mask that follows it, and the height in its header
        // counts both: one pixel tall is a header height of two.
        let small = {
            let mut out = dib(1, 2, 24, 0, 0, &[], &[0x00, 0x00, 0xFF, 0x00]);
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
            out
        };
        let large = {
            let mut out = dib(
                2,
                4,
                24,
                0,
                0,
                &[],
                &[
                    0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00,
                    0x00, 0x00, 0x00,
                ],
            );
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
            out
        };
        let icon = decode(&ico(&[(1, 1, 24, small), (2, 2, 24, large)])).expect("an icon reads");
        assert_eq!((icon.width, icon.height), (2, 2));
        assert_eq!(pixels(&icon)[0], [255, 0, 0, 255]);
    }

    #[test]
    fn an_icon_entry_with_no_alpha_channel_takes_its_transparency_from_the_mask() {
        // Before alpha channels, an icon's transparency was a one-bit mask after the pixels: a set
        // bit means the desktop shows through. Without it every icon would be a solid square.
        let data = {
            let mut out = dib(
                2,
                4,
                24,
                0,
                0,
                &[],
                &[
                    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
                    0xFF, 0x00, 0x00,
                ],
            );
            // The mask, bottom row first, one bit per pixel padded to four bytes. The top row's
            // left pixel is transparent.
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0x80, 0x00, 0x00, 0x00]);
            out
        };
        let icon = decode(&ico(&[(2, 2, 24, data)])).expect("an icon reads");
        assert_eq!(
            pixels(&icon),
            vec![
                [255, 255, 255, 0],
                [255, 255, 255, 255],
                [255, 255, 255, 255],
                [255, 255, 255, 255],
            ],
            "the masked pixel is the one the desktop shows through"
        );
    }

    #[test]
    fn a_32_bit_icon_entry_with_no_alpha_uses_its_mask() {
        let data = {
            let mut out = dib(1, 2, 32, 0, 0, &[], &[0x0A, 0x14, 0x1E, 0x00]);
            // The mask, one bit per pixel padded to four bytes: the single pixel is transparent.
            out.extend_from_slice(&[0x80, 0x00, 0x00, 0x00]);
            out
        };
        let icon = decode(&ico(&[(1, 1, 32, data)])).expect("an icon reads");
        assert_eq!(pixels(&icon), vec![[30, 20, 10, 0]]);
    }

    #[test]
    fn a_compressed_bitmap_is_refused_by_name() {
        // A compressed bitmap needs a decompressor per method. Reading the bytes as pixels anyway
        // would produce a picture that is not the one the author drew.
        let error = decode(&bmp(2, 2, 24, 1, &[0; 16])).expect_err("compression is refused");
        assert!(matches!(error, IconError::Unsupported { .. }), "{error}");
        assert!(error.to_string().contains("compressed"), "{error}");
    }

    #[test]
    fn a_bitmap_at_a_depth_no_icon_uses_is_refused_by_name() {
        let error = decode(&bmp(2, 2, 16, 0, &[0; 8])).expect_err("16 bits is refused");
        assert!(error.to_string().contains("16 bits per pixel"), "{error}");
    }

    #[test]
    fn a_bitmap_that_ends_before_its_pixels_do_is_refused() {
        let error = decode(&bmp(4, 4, 24, 0, &[0; 8])).expect_err("a short file is refused");
        assert!(matches!(error, IconError::Truncated { .. }), "{error}");
    }

    #[test]
    fn an_icon_larger_than_a_window_may_carry_is_refused() {
        let error = decode(&bmp(
            i32::try_from(MAX_ICON_EDGE).expect("small") + 1,
            1,
            24,
            0,
            &[0; 8],
        ))
        .expect_err("an oversized icon is refused");
        assert!(matches!(error, IconError::TooBig { .. }), "{error}");
    }

    #[test]
    fn a_file_that_is_neither_a_bitmap_nor_an_icon_is_refused() {
        let error = decode(b"not an image at all").expect_err("neither container");
        assert!(matches!(error, IconError::Unsupported { .. }), "{error}");
        assert!(IconError::Empty.to_string().contains("empty"));
        assert_eq!(
            decode(&[]).expect_err("an empty file"),
            IconError::Empty,
            "an empty file is not a picture"
        );
    }

    #[test]
    fn an_icon_whose_largest_image_is_a_png_is_refused_by_name() {
        // Modern icons often store a PNG inside the directory. Saying so is the honest answer: a
        // decoder without an inflater cannot produce the picture, and pretending otherwise would
        // hand the platform whatever the PNG's first bytes happen to look like as pixels.
        let png = {
            let mut data = Vec::new();
            data.extend_from_slice(&PNG_SIGNATURE);
            data.extend_from_slice(&[0; 32]);
            data
        };
        let error = decode(&ico(&[(2, 2, 32, png)])).expect_err("a PNG entry is refused");
        assert!(matches!(error, IconError::Unsupported { .. }), "{error}");
        assert!(error.to_string().contains("PNG"), "{error}");
    }

    #[test]
    fn an_icon_whose_image_disagrees_with_its_directory_is_refused() {
        // The directory says the image is two pixels wide and the bitmap says three. Reading the
        // pixels under one of those widths would give a picture of the wrong shape, so a file whose
        // two halves disagree is refused rather than guessed at.
        let data = dib(3, 4, 24, 0, 0, &[], &[0; 24]);
        let error = decode(&ico(&[(2, 2, 24, data)])).expect_err("a disagreement is refused");
        assert!(error.to_string().contains("wide"), "{error}");
    }

    #[test]
    fn an_icon_directory_with_no_readable_image_is_refused() {
        let mut empty = Vec::new();
        push_u16(&mut empty, 0);
        push_u16(&mut empty, 1);
        push_u16(&mut empty, 0);
        let error = decode(&empty).expect_err("an empty directory is refused");
        assert!(error.to_string().contains("no images"), "{error}");

        // An entry that points past the end of the file names an image that is not there.
        let mut dangling = Vec::new();
        push_u16(&mut dangling, 0);
        push_u16(&mut dangling, 1);
        push_u16(&mut dangling, 1);
        dangling.extend_from_slice(&[2, 2, 0, 0]);
        push_u16(&mut dangling, 1);
        push_u16(&mut dangling, 24);
        push_u32(&mut dangling, 64);
        push_u32(&mut dangling, 4096);
        let error = decode(&dangling).expect_err("a dangling entry is refused");
        assert!(
            error.to_string().contains("no image in the directory"),
            "{error}"
        );
    }

    #[test]
    fn a_file_larger_than_any_icon_is_refused_without_being_read() {
        let error = decode(&vec![0_u8; MAX_ICON_FILE_BYTES + 1]).expect_err("too large");
        assert!(matches!(error, IconError::TooLarge { .. }), "{error}");
    }
}
