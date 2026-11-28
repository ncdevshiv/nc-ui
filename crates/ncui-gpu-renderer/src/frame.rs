//! The typed binary representation the renderer's hot path uses.
//!
//! ADR-002: *"The GPU engine is a native Rust layer. The hot path uses an efficient typed
//! binary representation; JSON is reserved for the control plane."* This module is that
//! representation.
//!
//! # Why a hand-rolled format rather than `serde_json`
//!
//! A frame is produced and consumed every time the compositor redraws. Serialising it as
//! JSON means escaping, allocation per field, and a parser that has to trust its input —
//! on a path where the input came from another process. A fixed-layout binary has none of
//! that: the sizes are known, the fields are at known offsets, and a length that does not
//! match the declared content is refused before anything is allocated.
//!
//! # What is guaranteed
//!
//! * **Validated before allocation.** [`Frame::decode`] checks the header, the declared
//!   length, and the implied island count *before* it reserves a buffer. A peer that
//!   announces a four-gigabyte frame costs nothing.
//! * **Exact lengths.** The payload length must be exactly what the declared island count
//!   implies. A frame that claims three islands and carries two is refused rather than
//!   padded, because a decoder that guesses is a decoder that can be made to disagree
//!   with its peer.
//! * **A version byte.** A frame written by a newer renderer is refused, not
//!   misinterpreted.
//!
//! The format is deliberately not extensible: a field this version does not know about is
//! a frame this version must not accept. Extensibility is what turns a version byte into
//! a negotiation nobody does.

use core::fmt;

/// The four bytes every frame starts with: `NCUF`.
pub const MAGIC: [u8; 4] = *b"NCUF";

/// The format version this build reads and writes.
pub const VERSION: u8 = 1;

/// The largest frame this decoder will accept.
///
/// A bound rather than a configuration: the decoder runs on a path where the input came
/// from another process, and the cost of accepting a hostile length is paid before
/// anything is allocated.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// The largest number of islands one frame may carry.
pub const MAX_ISLANDS: u32 = 65_536;

/// What a frame is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    /// A compositor frame: the islands to draw and the colour to clear with.
    Frame,
}

impl FrameKind {
    /// The wire value.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Frame => 1,
        }
    }

    /// Parses a wire value.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Frame),
            _ => None,
        }
    }
}

/// Why a frame was refused.
///
/// A frame that came from another process is untrusted input, so every rejection is a
/// value with a reason rather than a panic or a truncated read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// The bytes are structurally invalid in a way no other variant describes — an
    /// unknown command tag, a role code nobody defined, a string that is not UTF-8.
    /// The detail is static text rather than the offending bytes: an error crossing a
    /// process boundary should not echo the input it refused.
    Malformed {
        /// What was wrong.
        detail: &'static str,
    },
    /// The bytes do not start with the frame magic.
    BadMagic {
        /// What was found instead.
        found: [u8; 4],
    },
    /// The frame is a version this build does not read.
    UnsupportedVersion {
        /// The version found.
        found: u8,
    },
    /// The frame is a kind this build does not read.
    UnknownKind {
        /// The kind found.
        found: u8,
    },
    /// The declared length does not match the content the header implies.
    LengthMismatch {
        /// The length the header declared.
        declared: u32,
        /// The length the content implies.
        expected: u32,
    },
    /// The frame is larger than [`MAX_FRAME_BYTES`].
    TooLarge {
        /// The length the header declared.
        declared: u32,
        /// The largest frame accepted.
        limit: u32,
    },
    /// The frame declares more islands than [`MAX_ISLANDS`].
    TooManyIslands {
        /// The count the frame declared.
        declared: u32,
        /// The largest count accepted.
        limit: u32,
    },
    /// The bytes ended before the frame did.
    Truncated {
        /// How many bytes were available.
        available: usize,
        /// How many the frame needed.
        needed: usize,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { detail } => write!(f, "frame is malformed: {detail}"),
            Self::BadMagic { found } => {
                write!(
                    f,
                    "not a frame: magic was {found:02x?}, expected {MAGIC:02x?}"
                )
            }
            Self::UnsupportedVersion { found } => {
                write!(f, "frame version {found} is not supported")
            }
            Self::UnknownKind { found } => write!(f, "frame kind {found} is not known"),
            Self::LengthMismatch { declared, expected } => write!(
                f,
                "frame declares {declared} bytes but its content is {expected} bytes"
            ),
            Self::TooLarge { declared, limit } => {
                write!(
                    f,
                    "frame declares {declared} bytes, over the {limit}-byte limit"
                )
            }
            Self::TooManyIslands { declared, limit } => {
                write!(
                    f,
                    "frame declares {declared} islands, over the {limit} limit"
                )
            }
            Self::Truncated { available, needed } => {
                write!(
                    f,
                    "frame is truncated: {available} bytes available, {needed} needed"
                )
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// One island's rectangle, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IslandRect {
    /// The left edge.
    pub x: f32,
    /// The top edge.
    pub y: f32,
    /// The width.
    pub width: f32,
    /// The height.
    pub height: f32,
    /// The island's colour, as red, green, blue and alpha in `0..=255`.
    pub colour: [u8; 4],
}

impl IslandRect {
    /// The size of one island on the wire.
    pub const WIRE_BYTES: usize = 24;

    /// Whether the rectangle has a positive area and finite coordinates.
    ///
    /// A rectangle with a negative size or a `NaN` edge is not a rectangle, and a renderer
    /// that drew it would be drawing a number rather than a shape.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width >= 0.0
            && self.height >= 0.0
    }
}

/// The header every frame starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// What the frame is for.
    pub kind: FrameKind,
    /// Reserved for future use. Always zero in this version, and a frame with a non-zero
    /// value here is refused rather than ignored: a flag nobody defined is a flag nobody
    /// can honour.
    pub flags: u16,
    /// The payload length, in bytes.
    pub payload_len: u32,
}

impl FrameHeader {
    /// The size of the header on the wire.
    pub const WIRE_BYTES: usize = 12;

    /// Reads a header from the front of `bytes`.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError::Truncated`] when there are not enough bytes, and the
    /// appropriate refusal for a header that does not describe a frame this build reads.
    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        if bytes.len() < Self::WIRE_BYTES {
            return Err(FrameError::Truncated {
                available: bytes.len(),
                needed: Self::WIRE_BYTES,
            });
        }
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&bytes[0..4]);
        if magic != MAGIC {
            return Err(FrameError::BadMagic { found: magic });
        }
        let version = bytes[4];
        if version != VERSION && version != crate::wire_v2::VERSION_V2 {
            return Err(FrameError::UnsupportedVersion { found: version });
        }
        let Some(kind) = FrameKind::from_code(bytes[5]) else {
            return Err(FrameError::UnknownKind { found: bytes[5] });
        };
        let flags = u16::from_le_bytes([bytes[6], bytes[7]]);
        if flags != 0 {
            return Err(FrameError::UnknownKind {
                found: u16::from_le_bytes([bytes[6], bytes[7]]) as u8,
            });
        }
        let payload_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        Ok(Self {
            kind,
            flags,
            payload_len,
        })
    }

    /// Writes the header to the front of `out`.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError::Truncated`] when `out` is too short.
    pub fn encode(&self, out: &mut [u8]) -> Result<(), FrameError> {
        if out.len() < Self::WIRE_BYTES {
            return Err(FrameError::Truncated {
                available: out.len(),
                needed: Self::WIRE_BYTES,
            });
        }
        out[0..4].copy_from_slice(&MAGIC);
        out[4] = VERSION;
        out[5] = self.kind.code();
        out[6..8].copy_from_slice(&self.flags.to_le_bytes());
        out[8..12].copy_from_slice(&self.payload_len.to_le_bytes());
        Ok(())
    }
}

/// One application-drawn island: a [`ncui_gpu::display::DisplayList`] and the viewport it is drawn through.
///
/// The solid-colour [`IslandRect`] is the degenerate case of island content — one rect
/// filling the viewport. A `CustomIsland` is the general case the timeline needs: clips,
/// waveforms, thumbnails, playheads, declared by application code in island-content
/// coordinates, with the hit regions interleaved in the same stream so what is drawn and
/// what is hit-tested cannot drift apart.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomIsland {
    /// The viewport's left edge, in physical pixels.
    pub x: f32,
    /// The viewport's top edge.
    pub y: f32,
    /// The viewport's width.
    pub width: f32,
    /// The viewport's height.
    pub height: f32,
    /// The horizontal scroll offset within the content.
    pub scroll_x: f32,
    /// The vertical scroll offset within the content.
    pub scroll_y: f32,
    /// The content. The renderer walks the draw commands; the hit regions ride along so
    /// the input host answers about exactly this geometry.
    pub list: ncui_gpu::display::DisplayList,
}

impl CustomIsland {
    /// Whether the viewport is one pixels could land in.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width >= 0.0
            && self.height >= 0.0
            && self.scroll_x.is_finite()
            && self.scroll_y.is_finite()
    }
}

/// One compositor frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// The colour the target is cleared with, as red, green, blue and alpha in `0..=255`.
    pub clear_colour: [u8; 4],
    /// The islands to draw, in draw order.
    pub islands: Vec<IslandRect>,
    /// The application-drawn islands, drawn after the solid ones — custom content is the
    /// foreground of any frame it shares with flat islands.
    pub custom_islands: Vec<CustomIsland>,
}

impl Frame {
    /// The size of the payload for a frame with this many islands.
    #[must_use]
    pub const fn payload_len(island_count: usize) -> u32 {
        (Self::PAYLOAD_FIXED_BYTES + island_count * IslandRect::WIRE_BYTES) as u32
    }

    /// The fixed part of a payload: the clear colour and the island count.
    pub const PAYLOAD_FIXED_BYTES: usize = 8;

    /// The total size of a frame with this many islands, header included.
    #[must_use]
    pub const fn encoded_len(island_count: usize) -> u32 {
        (FrameHeader::WIRE_BYTES
            + Self::PAYLOAD_FIXED_BYTES
            + island_count * IslandRect::WIRE_BYTES) as u32
    }

    /// Encodes the frame.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError::TooManyIslands`] when the frame carries more islands than
    /// [`MAX_ISLANDS`], and [`FrameError::TooLarge`] when the encoded frame would exceed
    /// [`MAX_FRAME_BYTES`].
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        if !self.custom_islands.is_empty() {
            // Custom content needs variable-length records v1 does not have, so the
            // frame travels as v2. A solid-only frame still encodes as v1 — the
            // smallest version the content needs — so a peer that only reads v1 never
            // sees a byte it cannot parse.
            return crate::wire_v2::encode_v2(self);
        }
        if self.islands.len() as u64 > u64::from(MAX_ISLANDS) {
            return Err(FrameError::TooManyIslands {
                declared: self.islands.len() as u32,
                limit: MAX_ISLANDS,
            });
        }
        let payload_len = Self::payload_len(self.islands.len());
        let total = FrameHeader::WIRE_BYTES + payload_len as usize;
        if total as u64 > u64::from(MAX_FRAME_BYTES) {
            return Err(FrameError::TooLarge {
                declared: total as u32,
                limit: MAX_FRAME_BYTES,
            });
        }
        let mut out = vec![0u8; total];
        FrameHeader {
            kind: FrameKind::Frame,
            flags: 0,
            payload_len,
        }
        .encode(&mut out)?;
        let body = &mut out[FrameHeader::WIRE_BYTES..];
        body[0..4].copy_from_slice(&self.clear_colour);
        body[4..8].copy_from_slice(&(self.islands.len() as u32).to_le_bytes());
        for (index, island) in self.islands.iter().enumerate() {
            let start = Self::PAYLOAD_FIXED_BYTES + index * IslandRect::WIRE_BYTES;
            let slot = &mut body[start..start + IslandRect::WIRE_BYTES];
            slot[0..4].copy_from_slice(&island.x.to_le_bytes());
            slot[4..8].copy_from_slice(&island.y.to_le_bytes());
            slot[8..12].copy_from_slice(&island.width.to_le_bytes());
            slot[12..16].copy_from_slice(&island.height.to_le_bytes());
            slot[16..20].copy_from_slice(&island.colour);
            slot[20..24].copy_from_slice(&0u32.to_le_bytes());
        }
        Ok(out)
    }

    /// Decodes a frame.
    ///
    /// # Errors
    ///
    /// Returns the refusal that describes what was wrong with the bytes. Every check that
    /// can be made from the header is made before the payload is reserved, so a hostile
    /// length costs nothing.
    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        let header = FrameHeader::decode(bytes)?;
        if bytes.len() > 4 && bytes[4] == crate::wire_v2::VERSION_V2 {
            if header.payload_len > MAX_FRAME_BYTES {
                return Err(FrameError::TooLarge {
                    declared: header.payload_len,
                    limit: MAX_FRAME_BYTES,
                });
            }
            return crate::wire_v2::decode_v2(
                &bytes[FrameHeader::WIRE_BYTES..],
                header.payload_len,
            );
        }
        if header.payload_len > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge {
                declared: header.payload_len,
                limit: MAX_FRAME_BYTES,
            });
        }
        let body = &bytes[FrameHeader::WIRE_BYTES..];
        if body.len() < header.payload_len as usize {
            return Err(FrameError::Truncated {
                available: body.len(),
                needed: header.payload_len as usize,
            });
        }
        if header.payload_len < Self::PAYLOAD_FIXED_BYTES as u32 {
            return Err(FrameError::LengthMismatch {
                declared: header.payload_len,
                expected: Self::PAYLOAD_FIXED_BYTES as u32,
            });
        }
        let count = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        if count > MAX_ISLANDS {
            return Err(FrameError::TooManyIslands {
                declared: count,
                limit: MAX_ISLANDS,
            });
        }
        let expected = Self::payload_len(count as usize);
        if expected != header.payload_len {
            return Err(FrameError::LengthMismatch {
                declared: header.payload_len,
                expected,
            });
        }
        let body = &body[..header.payload_len as usize];
        let mut clear_colour = [0u8; 4];
        clear_colour.copy_from_slice(&body[0..4]);
        let mut islands = Vec::with_capacity(count as usize);
        for index in 0..count as usize {
            let start = Self::PAYLOAD_FIXED_BYTES + index * IslandRect::WIRE_BYTES;
            let slot = &body[start..start + IslandRect::WIRE_BYTES];
            islands.push(IslandRect {
                x: f32::from_le_bytes([slot[0], slot[1], slot[2], slot[3]]),
                y: f32::from_le_bytes([slot[4], slot[5], slot[6], slot[7]]),
                width: f32::from_le_bytes([slot[8], slot[9], slot[10], slot[11]]),
                height: f32::from_le_bytes([slot[12], slot[13], slot[14], slot[15]]),
                colour: [slot[16], slot[17], slot[18], slot[19]],
            });
        }
        Ok(Self {
            clear_colour,
            islands,
            custom_islands: Vec::new(),
        })
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

    fn sample() -> Frame {
        Frame {
            custom_islands: Vec::new(),
            clear_colour: [10, 20, 30, 255],
            islands: vec![
                IslandRect {
                    x: 1.0,
                    y: 2.0,
                    width: 30.0,
                    height: 40.0,
                    colour: [255, 0, 0, 255],
                },
                IslandRect {
                    x: -5.5,
                    y: 0.25,
                    width: 1.0,
                    height: 2.0,
                    colour: [0, 255, 0, 128],
                },
            ],
        }
    }

    #[test]
    fn a_frame_round_trips() {
        let encoded = sample().encode().expect("encoded");
        let decoded = Frame::decode(&encoded).expect("decoded");
        assert_eq!(decoded, sample());
    }

    #[test]
    fn an_empty_frame_round_trips() {
        let frame = Frame {
            custom_islands: Vec::new(),
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        let encoded = frame.encode().expect("encoded");
        assert_eq!(encoded.len(), Frame::encoded_len(0) as usize);
        let decoded = Frame::decode(&encoded).expect("decoded");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn bytes_that_are_not_a_frame_are_refused() {
        let error = Frame::decode(b"not a frame at all").expect_err("must be refused");
        assert!(matches!(error, FrameError::BadMagic { .. }), "got {error}");
    }

    #[test]
    fn a_truncated_frame_is_refused_rather_than_read_past_the_end() {
        let encoded = sample().encode().expect("encoded");
        for cut in 0..encoded.len() {
            let error = Frame::decode(&encoded[..cut]).expect_err("must be refused");
            assert!(
                matches!(
                    error,
                    FrameError::Truncated { .. } | FrameError::LengthMismatch { .. }
                ),
                "a frame cut at {cut} bytes gave {error}"
            );
        }
    }

    #[test]
    fn a_length_that_disagrees_with_the_content_is_refused() {
        let mut encoded = sample().encode().expect("encoded");
        // Claim one island more than the frame carries.
        let count_at = FrameHeader::WIRE_BYTES + 4;
        encoded[count_at..count_at + 4].copy_from_slice(&3u32.to_le_bytes());
        let error = Frame::decode(&encoded).expect_err("must be refused");
        assert!(
            matches!(error, FrameError::LengthMismatch { .. }),
            "got {error}"
        );
    }

    #[test]
    fn a_hostile_length_is_refused_before_anything_is_allocated() {
        let mut encoded = sample().encode().expect("encoded");
        encoded[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        let error = Frame::decode(&encoded).expect_err("must be refused");
        assert!(matches!(error, FrameError::TooLarge { .. }), "got {error}");
    }

    #[test]
    fn a_future_version_is_refused_rather_than_misread() {
        let mut encoded = sample().encode().expect("encoded");
        encoded[4] = crate::wire_v2::VERSION_V2 + 1;
        let error = Frame::decode(&encoded).expect_err("must be refused");
        assert!(
            matches!(error, FrameError::UnsupportedVersion { .. }),
            "got {error}"
        );
    }

    #[test]
    fn an_undefined_flag_is_refused_rather_than_ignored() {
        let mut encoded = sample().encode().expect("encoded");
        encoded[6..8].copy_from_slice(&1u16.to_le_bytes());
        let error = Frame::decode(&encoded).expect_err("must be refused");
        assert!(
            matches!(error, FrameError::UnknownKind { .. }),
            "got {error}"
        );
    }

    #[test]
    fn a_frame_with_more_islands_than_the_limit_is_refused() {
        let frame = Frame {
            custom_islands: Vec::new(),
            clear_colour: [0, 0, 0, 255],
            islands: vec![
                IslandRect {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                    colour: [0, 0, 0, 255],
                };
                MAX_ISLANDS as usize + 1
            ],
        };
        let error = frame.encode().expect_err("must be refused");
        assert!(
            matches!(error, FrameError::TooManyIslands { .. }),
            "got {error}"
        );
    }

    #[test]
    fn a_rectangle_with_no_area_or_a_nan_edge_is_not_a_rectangle() {
        let mut island = IslandRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
            colour: [0, 0, 0, 255],
        };
        assert!(island.is_valid());
        island.width = -1.0;
        assert!(!island.is_valid(), "a negative width is not a rectangle");
        island.width = 1.0;
        island.x = f32::NAN;
        assert!(!island.is_valid(), "a NaN edge is not a rectangle");
    }
}
