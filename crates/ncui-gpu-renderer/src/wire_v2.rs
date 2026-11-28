//! The version-2 frame wire format: custom islands cross the boundary.
//!
//! Version 1 of the format is fixed-length records only — clear colour and solid
//! islands. A [`crate::frame::CustomIsland`] carries a display list, and a display list
//! carries strings, so v1 refuses it loudly (`CustomContentNotEncodable`). That refusal
//! is honest but it means a frame with custom content cannot cross a process boundary at
//! all. Version 2 is the answer: variable-length records with a tag per command, decoded
//! *through the display list's own builder* so a malformed stream fails the same
//! validation a mistaken producer would — clip balance, geometry finiteness, command
//! count. Two validations that must agree are one validation too many.
//!
//! # Layout
//!
//! After the shared twelve-byte header (`nc-frame`, version 2):
//!
//! ```text
//! [0..4]    clear colour, rgba
//! [4..8]    solid island count, u32-le
//!           then count × 24-byte solid island records, exactly as v1
//! [..+4]    custom island count, u32-le
//!           then per island: x, y, width, height, scroll_x, scroll_y as f32-le (24 B),
//!           a u32-le command count, and the commands
//! ```
//!
//! Each command starts with a one-byte tag. `Rect` carries four f32 bounds and four
//! colour bytes. `ClipPush` carries four f32 bounds; `ClipPop` carries nothing.
//! `TextRun` carries two f32 origin coordinates, one f32 size, four colour bytes and a
//! length-prefixed UTF-8 string. `Blit` carries four f32 bounds and a length-prefixed
//! source key. `HitRegion` carries four f32 bounds, a one-byte role, and two
//! length-prefixed strings (id, then label). Lengths are u32-le; a string longer than
//! [`MAX_STRING_BYTES`] is refused, because a region label is a phrase and a runaway
//! length is a corrupt stream.
//!
//! A v2 frame may carry zero custom islands: the version byte describes what the format
//! *can* carry, and `encode()` picks the smallest version the content needs, so a
//! solid-only frame still travels as v1 and a v1 peer never sees a byte it cannot read.

use ncui_gpu::display::{Command, DisplayList, MAX_COMMANDS, RegionRole};
use ncui_gpu::geometry::{LogicalPoint, Rect};

use crate::frame::{CustomIsland, Frame, FrameError, FrameHeader, MAX_FRAME_BYTES, MAX_ISLANDS};

/// The version byte v2 frames carry.
pub const VERSION_V2: u8 = 2;

/// The longest string a command may carry, in bytes. A hit-region label is a sentence;
/// this cap is sixty-four thousand times generous and still a cap.
pub const MAX_STRING_BYTES: u32 = 65_536;

const TAG_RECT: u8 = 1;
const TAG_CLIP_PUSH: u8 = 2;
const TAG_CLIP_POP: u8 = 3;
const TAG_TEXT_RUN: u8 = 4;
const TAG_BLIT: u8 = 5;
const TAG_HIT_REGION: u8 = 6;

/// The per-island fixed record: viewport, scroll offsets and the command count.
const ISLAND_FIXED_BYTES: usize = 28;

/// One encoded string's overhead.
const STRING_LEN_BYTES: usize = 4;

fn write_f32(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn write_rect(out: &mut Vec<u8>, rect: Rect) {
    write_f32(out, rect.x);
    write_f32(out, rect.y);
    write_f32(out, rect.width);
    write_f32(out, rect.height);
}

fn encoded_command_len(command: &Command) -> usize {
    match command {
        Command::Rect { .. } => 1 + 16 + 4,
        Command::ClipPush { .. } => 1 + 16,
        Command::ClipPop => 1,
        Command::TextRun { text, .. } => 1 + 12 + 4 + STRING_LEN_BYTES + text.len(),
        Command::Blit { source, .. } => 1 + 16 + STRING_LEN_BYTES + source.len(),
        Command::HitRegion { id, label, .. } => {
            1 + 16 + 1 + STRING_LEN_BYTES + id.len() + STRING_LEN_BYTES + label.len()
        }
    }
}

fn encode_command(out: &mut Vec<u8>, command: &Command) {
    match command {
        Command::Rect { bounds, colour } => {
            out.push(TAG_RECT);
            write_rect(out, *bounds);
            out.extend_from_slice(colour);
        }
        Command::ClipPush { bounds } => {
            out.push(TAG_CLIP_PUSH);
            write_rect(out, *bounds);
        }
        Command::ClipPop => out.push(TAG_CLIP_POP),
        Command::TextRun {
            origin,
            text,
            size,
            colour,
        } => {
            out.push(TAG_TEXT_RUN);
            write_f32(out, origin.x);
            write_f32(out, origin.y);
            write_f32(out, *size);
            out.extend_from_slice(colour);
            write_string(out, text);
        }
        Command::Blit { bounds, source } => {
            out.push(TAG_BLIT);
            write_rect(out, *bounds);
            write_string(out, source);
        }
        Command::HitRegion {
            id,
            bounds,
            label,
            role,
        } => {
            out.push(TAG_HIT_REGION);
            write_rect(out, *bounds);
            out.push(role_code(*role));
            write_string(out, id);
            write_string(out, label);
        }
    }
}

fn role_code(role: RegionRole) -> u8 {
    match role {
        RegionRole::Button => 1,
        RegionRole::Handle => 2,
        RegionRole::Track => 3,
        RegionRole::Ruler => 4,
        RegionRole::Curve => 5,
        RegionRole::Surface => 6,
    }
}

fn role_from_code(code: u8) -> Option<RegionRole> {
    match code {
        1 => Some(RegionRole::Button),
        2 => Some(RegionRole::Handle),
        3 => Some(RegionRole::Track),
        4 => Some(RegionRole::Ruler),
        5 => Some(RegionRole::Curve),
        6 => Some(RegionRole::Surface),
        _ => None,
    }
}

/// Encodes a frame as version 2.
///
/// # Errors
///
/// Returns [`FrameError::TooManyIslands`] over the island limit and
/// [`FrameError::TooLarge`] over the frame byte limit. The encoding is sized before it
/// is written, so a refusal costs nothing beyond the count.
pub fn encode_v2(frame: &Frame) -> Result<Vec<u8>, FrameError> {
    let total_islands = frame.islands.len() as u64 + frame.custom_islands.len() as u64;
    if frame.islands.len() as u64 > u64::from(MAX_ISLANDS)
        || frame.custom_islands.len() as u64 > u64::from(MAX_ISLANDS)
        || total_islands > u64::from(MAX_ISLANDS) * 2
    {
        return Err(FrameError::TooManyIslands {
            declared: total_islands as u32,
            limit: MAX_ISLANDS,
        });
    }
    let mut payload_len = 8 + frame.islands.len() * 24 + 4;
    for island in &frame.custom_islands {
        payload_len += ISLAND_FIXED_BYTES;
        for command in island.list.commands() {
            payload_len += encoded_command_len(command);
        }
    }
    let total = FrameHeader::WIRE_BYTES + payload_len;
    if total as u64 > u64::from(MAX_FRAME_BYTES) {
        return Err(FrameError::TooLarge {
            declared: total as u32,
            limit: MAX_FRAME_BYTES,
        });
    }
    let mut out = Vec::with_capacity(total);
    out.resize(FrameHeader::WIRE_BYTES, 0);
    FrameHeader {
        kind: crate::frame::FrameKind::Frame,
        flags: 0,
        payload_len: payload_len as u32,
    }
    .encode(&mut out)?;
    out[4] = VERSION_V2;
    out.extend_from_slice(&frame.clear_colour);
    out.extend_from_slice(&(frame.islands.len() as u32).to_le_bytes());
    for island in &frame.islands {
        write_f32(&mut out, island.x);
        write_f32(&mut out, island.y);
        write_f32(&mut out, island.width);
        write_f32(&mut out, island.height);
        out.extend_from_slice(&island.colour);
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    out.extend_from_slice(&(frame.custom_islands.len() as u32).to_le_bytes());
    for island in &frame.custom_islands {
        write_f32(&mut out, island.x);
        write_f32(&mut out, island.y);
        write_f32(&mut out, island.width);
        write_f32(&mut out, island.height);
        write_f32(&mut out, island.scroll_x);
        write_f32(&mut out, island.scroll_y);
        out.extend_from_slice(&(island.list.len() as u32).to_le_bytes());
        for command in island.list.commands() {
            encode_command(&mut out, command);
        }
    }
    Ok(out)
}

/// A cursor over the payload that refuses to over-read.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], FrameError> {
        if self.bytes.len() - self.at < count {
            return Err(FrameError::Truncated {
                available: self.bytes.len() - self.at,
                needed: count,
            });
        }
        let slice = &self.bytes[self.at..self.at + count];
        self.at += count;
        Ok(slice)
    }

    fn f32(&mut self) -> Result<f32, FrameError> {
        let bytes = self.take(4)?;
        Ok(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u32(&mut self) -> Result<u32, FrameError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u8(&mut self) -> Result<u8, FrameError> {
        Ok(self.take(1)?[0])
    }

    fn string(&mut self) -> Result<String, FrameError> {
        let len = self.u32()?;
        if len > MAX_STRING_BYTES {
            return Err(FrameError::Malformed {
                detail: "a command string exceeds the length cap",
            });
        }
        let bytes = self.take(len as usize)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| FrameError::Malformed {
            detail: "a command string is not valid UTF-8",
        })
    }

    fn rect(&mut self) -> Result<Rect, FrameError> {
        Ok(Rect {
            x: self.f32()?,
            y: self.f32()?,
            width: self.f32()?,
            height: self.f32()?,
        })
    }
}

/// Decodes a version-2 payload — the bytes after the header.
///
/// The command stream is decoded into the display list's own builder, so a malformed
/// stream (an unbalanced clip, a negative rectangle, a runaway command count) fails the
/// same validation a mistaken producer would. A decoder that accepted what the builder
/// refuses would be a second, weaker contract — exactly what the one-stream design
/// exists to prevent.
///
/// # Errors
///
/// Returns the refusal that describes what was wrong with the bytes.
pub fn decode_v2(body: &[u8], payload_len: u32) -> Result<Frame, FrameError> {
    if body.len() < payload_len as usize {
        return Err(FrameError::Truncated {
            available: body.len(),
            needed: payload_len as usize,
        });
    }
    let mut reader = Reader {
        bytes: &body[..payload_len as usize],
        at: 0,
    };
    let colour_bytes = reader.take(4)?;
    let mut clear_colour = [0u8; 4];
    clear_colour.copy_from_slice(colour_bytes);
    let solid_count = reader.u32()?;
    if solid_count > MAX_ISLANDS {
        return Err(FrameError::TooManyIslands {
            declared: solid_count,
            limit: MAX_ISLANDS,
        });
    }
    let mut islands = Vec::with_capacity(solid_count as usize);
    for _ in 0..solid_count {
        let x = reader.f32()?;
        let y = reader.f32()?;
        let width = reader.f32()?;
        let height = reader.f32()?;
        let colour_bytes = reader.take(4)?;
        let mut colour = [0u8; 4];
        colour.copy_from_slice(colour_bytes);
        reader.take(4)?;
        islands.push(crate::frame::IslandRect {
            x,
            y,
            width,
            height,
            colour,
        });
    }
    let custom_count = reader.u32()?;
    if custom_count > MAX_ISLANDS {
        return Err(FrameError::TooManyIslands {
            declared: custom_count,
            limit: MAX_ISLANDS,
        });
    }
    let mut custom_islands = Vec::with_capacity(custom_count as usize);
    for _ in 0..custom_count {
        let x = reader.f32()?;
        let y = reader.f32()?;
        let width = reader.f32()?;
        let height = reader.f32()?;
        let scroll_x = reader.f32()?;
        let scroll_y = reader.f32()?;
        let command_count = reader.u32()?;
        if command_count as usize > MAX_COMMANDS {
            return Err(FrameError::Malformed {
                detail: "an island declares more commands than the display list cap",
            });
        }
        let mut builder = DisplayList::builder();
        for _ in 0..command_count {
            builder = decode_command(&mut reader, builder)?;
        }
        let list = builder.finish().map_err(|_| FrameError::Malformed {
            detail: "the command stream fails display-list validation",
        })?;
        let island = CustomIsland {
            x,
            y,
            width,
            height,
            scroll_x,
            scroll_y,
            list,
        };
        if !island.is_valid() {
            return Err(FrameError::Malformed {
                detail: "an island viewport carries non-finite or negative geometry",
            });
        }
        custom_islands.push(island);
    }
    if reader.at != payload_len as usize {
        return Err(FrameError::LengthMismatch {
            declared: payload_len,
            expected: reader.at as u32,
        });
    }
    Ok(Frame {
        clear_colour,
        islands,
        custom_islands,
    })
}

fn decode_command(
    reader: &mut Reader<'_>,
    builder: ncui_gpu::display::Builder,
) -> Result<ncui_gpu::display::Builder, FrameError> {
    let tag = reader.u8()?;
    Ok(match tag {
        TAG_RECT => {
            let bounds = reader.rect()?;
            let colour_bytes = reader.take(4)?;
            let mut colour = [0u8; 4];
            colour.copy_from_slice(colour_bytes);
            builder.rect(bounds, colour)
        }
        TAG_CLIP_PUSH => {
            let bounds = reader.rect()?;
            builder.clip_push(bounds)
        }
        TAG_CLIP_POP => builder.clip_pop(),
        TAG_TEXT_RUN => {
            let x = reader.f32()?;
            let y = reader.f32()?;
            let size = reader.f32()?;
            let colour_bytes = reader.take(4)?;
            let mut colour = [0u8; 4];
            colour.copy_from_slice(colour_bytes);
            let text = reader.string()?;
            builder.text(LogicalPoint { x, y }, &text, size, colour)
        }
        TAG_BLIT => {
            let bounds = reader.rect()?;
            let source = reader.string()?;
            builder.blit(bounds, &source)
        }
        TAG_HIT_REGION => {
            let bounds = reader.rect()?;
            let role_code = reader.u8()?;
            let Some(role) = role_from_code(role_code) else {
                return Err(FrameError::Malformed {
                    detail: "a hit region carries an unknown role code",
                });
            };
            let id = reader.string()?;
            let label = reader.string()?;
            builder.hit_region(&id, bounds, &label, role)
        }
        _ => {
            return Err(FrameError::Malformed {
                detail: "a command carries an unknown tag",
            });
        }
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions must abort loudly on failure"
)]
mod tests {
    use super::*;
    use crate::frame::IslandRect;

    fn custom_frame() -> Frame {
        let list = DisplayList::builder()
            .rect(Rect::new(0.0, 0.0, 200.0, 40.0), [30, 30, 46, 255])
            .clip_push(Rect::new(0.0, 0.0, 100.0, 40.0))
            .rect(Rect::new(90.0, 4.0, 30.0, 30.0), [200, 80, 80, 255])
            .clip_pop()
            .text(
                LogicalPoint { x: 8.0, y: 20.0 },
                "clip one.mp4",
                12.0,
                [230, 230, 235, 255],
            )
            .blit(Rect::new(4.0, 4.0, 32.0, 18.0), "thumb:intro")
            .hit_region(
                "playhead",
                Rect::new(60.0, 0.0, 2.0, 40.0),
                "playhead",
                RegionRole::Handle,
            )
            .finish()
            .expect("the fixture list is valid");
        Frame {
            clear_colour: [10, 10, 14, 255],
            islands: vec![IslandRect {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
                colour: [20, 20, 28, 255],
            }],
            custom_islands: vec![CustomIsland {
                x: 40.0,
                y: 100.0,
                width: 720.0,
                height: 200.0,
                scroll_x: 12.5,
                scroll_y: 0.0,
                list,
            }],
        }
    }

    #[test]
    fn v2_round_trip_preserves_everything() {
        let frame = custom_frame();
        let bytes = encode_v2(&frame).unwrap();
        assert_eq!(bytes[4], VERSION_V2);
        let payload_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let decoded = decode_v2(&bytes[FrameHeader::WIRE_BYTES..], payload_len).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn v2_refuses_truncation_at_every_step() {
        let frame = custom_frame();
        let bytes = encode_v2(&frame).unwrap();
        let payload_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        for end in 0..payload_len as usize {
            assert!(
                decode_v2(
                    &bytes[FrameHeader::WIRE_BYTES..FrameHeader::WIRE_BYTES + end],
                    payload_len
                )
                .is_err(),
                "truncation at {end} must be refused"
            );
        }
    }

    #[test]
    fn v2_refuses_an_unbalanced_clip_stream() {
        // A raw clip-pop with no push cannot be produced by the builder, so encode the
        // bytes by hand: one custom island, one command, tag ClipPop.
        let mut payload = Vec::new();
        payload.extend_from_slice(&[10u8, 10, 14, 255]);
        payload.extend_from_slice(&0u32.to_le_bytes()); // no solid islands
        payload.extend_from_slice(&1u32.to_le_bytes()); // one custom island
        for value in [0.0f32, 0.0, 100.0, 100.0, 0.0, 0.0] {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        payload.extend_from_slice(&1u32.to_le_bytes()); // one command
        payload.push(TAG_CLIP_POP);
        assert!(decode_v2(&payload, payload.len() as u32).is_err());
    }

    #[test]
    fn v2_refuses_an_unknown_tag_and_role() {
        let frame = custom_frame();
        let bytes = encode_v2(&frame).unwrap();
        let payload_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        // Flip the first command's tag byte to something undefined. The first command
        // starts after the header, 8 fixed bytes, one 24-byte solid island, 4 count
        // bytes, 28 island fixed bytes.
        let first_tag = FrameHeader::WIRE_BYTES + 8 + 24 + 4 + 28;
        let mut corrupted = bytes.clone();
        corrupted[first_tag] = 99;
        let body = &corrupted[FrameHeader::WIRE_BYTES..];
        assert!(decode_v2(body, payload_len).is_err());
    }

    #[test]
    fn v2_refuses_trailing_garbage() {
        let frame = custom_frame();
        let bytes = encode_v2(&frame).unwrap();
        let payload_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        // Claiming one fewer payload byte than encoded must mismatch, not pass.
        let body = &bytes[FrameHeader::WIRE_BYTES..];
        assert!(decode_v2(body, payload_len - 1).is_err());
    }

    #[test]
    fn frame_decode_dispatches_v2_and_encode_picks_the_minimal_version() {
        // The public seam is Frame::decode — a peer never calls the versioned decoder
        // directly. A custom frame must round-trip through it, and a solid-only frame
        // must encode as v1: the smallest version the content needs.
        let frame = custom_frame();
        let bytes = Frame::encode(&frame).unwrap();
        assert_eq!(bytes[4], VERSION_V2);
        let decoded = Frame::decode(&bytes).unwrap();
        assert_eq!(decoded, frame);

        let solid_only = Frame {
            custom_islands: vec![],
            ..custom_frame()
        };
        let bytes = Frame::encode(&solid_only).unwrap();
        assert_eq!(bytes[4], crate::frame::VERSION);
        let decoded = Frame::decode(&bytes).unwrap();
        assert_eq!(decoded, solid_only);
    }

    #[test]
    fn v2_round_trip_with_zero_custom_islands() {
        let frame = Frame {
            clear_colour: [1, 2, 3, 255],
            islands: vec![],
            custom_islands: vec![],
        };
        let bytes = encode_v2(&frame).unwrap();
        let payload_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let decoded = decode_v2(&bytes[FrameHeader::WIRE_BYTES..], payload_len).unwrap();
        assert_eq!(decoded, frame);
    }
}
