//! The wire framing: a fixed 12-byte header plus a length-delimited payload.
//!
//! # Why the header is fixed-width
//!
//! Every field is big-endian and fixed-width, so a header can be validated with two
//! comparisons and a `u32` read, with no field-by-field branching and no allocation. The
//! layout is documented in the crate root; this module is its implementation.
//!
//! # The load-bearing property
//!
//! `payload_len` is attacker-controlled, and it is validated against the configured maximum
//! *before* the decoder reserves anything. A four-byte stream whose length field claims
//! 4 GiB therefore costs four bytes and returns
//! [`ErrorKind::LimitExceeded`]. This is the single most important property of the decoder
//! (ADR-003) and it is why [`FrameDecoder::feed`] never grows its buffer past
//! `FRAME_HEADER_BYTES + max_payload_bytes`.
//!
//! A stream that has violated framing is *desynchronised*: nothing after the bad header can
//! be located, so continuing would attribute later bytes to a frame that does not exist. The
//! decoder latches that state and refuses all further work rather than guessing.

use core::fmt;
use std::collections::VecDeque;

use ncui_core::limits::PROTOCOL_MAX_FRAME_BYTES;
use ncui_core::{ErrorKind, NcError, Result};

use crate::codec::{BufReader, BufWriter};

/// ASCII `NCUI` read as a big-endian `u32`.
///
/// Chosen so a desynchronised stream is legible in a hex dump, and specific enough that four
/// random bytes will not accidentally validate. Compared as a number so a caller never has to
/// decode it as text to check it.
pub const FRAME_MAGIC: u32 = u32::from_be_bytes(*b"NCUI");

/// The framing version this build speaks.
///
/// A peer whose version this build does not know is rejected, not coerced: the meaning of a
/// field may have changed, and reading a newer header with older rules is how a protocol
/// silently corrupts data.
pub const PROTOCOL_VERSION: u8 = 1;

/// Size of the frame header in bytes: magic, version, kind, flags, payload length.
pub const FRAME_HEADER_BYTES: usize = 12;

/// The `BATCH` frame flag (bit 0).
///
/// A batch payload is a sequence of complete frames. It exists because at high message rates
/// the per-message write syscall dominates the cost of the message itself, and a transport
/// that batches still frames every individual message. Batches are one level deep: a frame
/// inside a batch must not itself set `BATCH`.
pub const BATCH_FLAG: u16 = 1;

/// A mask of every flag bit this version defines.
const KNOWN_FLAGS: u16 = BATCH_FLAG;

/// What a frame carries.
///
/// The discriminants are part of the wire format and must never be reused for a different
/// meaning; an unknown discriminant is a [`ErrorKind::Protocol`] violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FrameKind {
    /// A [`crate::Message::Request`].
    Request,
    /// A [`crate::Message::Response`].
    Response,
    /// A [`crate::Message::Event`].
    Event,
    /// A [`crate::Message::ErrorResponse`].
    ErrorResponse,
    /// The outer frame of a batch.
    ///
    /// A batch has no message of its own, so labelling the outer frame with the kind of its
    /// first element would be a lie that only shows up in a hex dump months later. Giving the
    /// batch its own kind means the frame header always describes what actually follows. The
    /// `BATCH` flag and this kind must agree: a frame with one and not the other is a
    /// [`ErrorKind::Protocol`] violation in either direction.
    Batch,
}

impl FrameKind {
    /// The wire discriminant.
    const fn as_u8(self) -> u8 {
        match self {
            Self::Request => 0x01,
            Self::Response => 0x02,
            Self::Event => 0x03,
            Self::ErrorResponse => 0x04,
            Self::Batch => 0x05,
        }
    }

    /// Maps a wire discriminant to a kind.
    const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0x01 => Some(Self::Request),
            0x02 => Some(Self::Response),
            0x03 => Some(Self::Event),
            0x04 => Some(Self::ErrorResponse),
            0x05 => Some(Self::Batch),
            _ => None,
        }
    }
}

impl fmt::Display for FrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Request => "request",
            Self::Response => "response",
            Self::Event => "event",
            Self::ErrorResponse => "error_response",
            Self::Batch => "batch",
        };
        f.write_str(name)
    }
}

/// The frame flag bitfield.
///
/// A newtype rather than a bare `u16` so a flag bit cannot be set by accident, and so the
/// decoder can reject a bit it does not understand at the point it is parsed instead of
/// ignoring it later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FrameFlags(u16);

impl FrameFlags {
    /// No flags set.
    pub const NONE: Self = Self(0);

    /// The batch flag, as a value suitable for [`FrameFlags::contains`].
    pub const BATCH: Self = Self(BATCH_FLAG);

    /// Builds flags from a raw bitfield, rejecting bits this version does not define.
    ///
    /// Rejecting unknown bits is deliberate. Ignoring one would mean ignoring a marker the
    /// sender set, and the payload would then be interpreted under rules the sender did not
    /// intend.
    pub fn from_bits(bits: u16) -> Result<Self> {
        if bits & !KNOWN_FLAGS == 0 {
            Ok(Self(bits))
        } else {
            Err(NcError::new(ErrorKind::Protocol {
                reason: format!(
                    "frame sets undefined flag bit(s) 0x{:04x}; this version defines 0x{KNOWN_FLAGS:04x}",
                    bits & !KNOWN_FLAGS
                ),
            }))
        }
    }

    /// The raw bitfield.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Whether every flag in `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// A decoded frame header.
///
/// Holds the payload *length*, not the payload: the decoder reads headers far more often than
/// it moves payloads, and keeping the two apart is what makes "validate then allocate" possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameHeader {
    /// What the payload contains.
    pub kind: FrameKind,
    /// The flag bitfield.
    pub flags: FrameFlags,
    /// The payload length in bytes, already validated against the configured maximum.
    pub payload_len: u32,
}

impl FrameHeader {
    /// Builds a header for a payload of `payload_len` bytes.
    ///
    /// The length is not validated here — the encoder learns the true length from the payload
    /// it is given, so a mismatch is a bug in the caller rather than hostile input.
    #[must_use]
    pub const fn new(kind: FrameKind, flags: FrameFlags, payload_len: u32) -> Self {
        Self {
            kind,
            flags,
            payload_len,
        }
    }

    /// Whether this frame's payload is a sequence of frames.
    #[must_use]
    pub const fn is_batch(self) -> bool {
        self.flags.contains(FrameFlags::BATCH)
    }
}

/// A complete frame: a validated header and the payload it described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    header: FrameHeader,
    payload: Vec<u8>,
}

impl Frame {
    /// Builds a frame from a header and its payload.
    ///
    /// The payload is taken by value because the decoder has just allocated it; an encoder
    /// that wants to keep its buffer should use [`encode_frame`], which only borrows.
    #[must_use]
    pub const fn new(header: FrameHeader, payload: Vec<u8>) -> Self {
        Self { header, payload }
    }

    /// The frame header.
    #[must_use]
    pub const fn header(&self) -> &FrameHeader {
        &self.header
    }

    /// The frame payload.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Consumes the frame, yielding the header and payload separately.
    #[must_use]
    pub fn into_parts(self) -> (FrameHeader, Vec<u8>) {
        (self.header, self.payload)
    }
}

/// Parses and range-checks a frame header from its first 12 bytes.
///
/// Takes a slice rather than a fixed array so it can be pointed at a caller-owned array, which
/// keeps the no-allocation property visible at the call site. Every field is checked before it
/// is used and nothing here allocates.
fn parse_header_bytes(raw: &[u8]) -> Result<FrameHeader> {
    if raw.len() < FRAME_HEADER_BYTES {
        return Err(framing_error(ErrorKind::Protocol {
            reason: format!(
                "frame header needs {FRAME_HEADER_BYTES} bytes, got {}",
                raw.len()
            ),
        }));
    }
    // The length is checked above, so these reads cannot fail; a short read here would still
    // be a malformed header, hence the `Protocol` classification rather than a `Codec` one.
    let protocol = |e: NcError| {
        framing_error(ErrorKind::Protocol {
            reason: e.to_string(),
        })
    };
    let mut reader = BufReader::new(raw, "ipc::frame::header");
    let magic = reader.get_u32().map_err(protocol)?;
    if magic != FRAME_MAGIC {
        return Err(framing_error(ErrorKind::Protocol {
            reason: format!("bad frame magic 0x{magic:08x}, expected 0x{FRAME_MAGIC:08x}"),
        }));
    }
    let version = reader.get_u8().map_err(protocol)?;
    if version != PROTOCOL_VERSION {
        return Err(framing_error(ErrorKind::Protocol {
            reason: format!(
                "unsupported frame version {version}, this build speaks {PROTOCOL_VERSION}"
            ),
        }));
    }
    let kind = FrameKind::from_u8(reader.get_u8().map_err(protocol)?).ok_or_else(|| {
        framing_error(ErrorKind::Protocol {
            reason: "unknown frame kind; this version defines request, response, event, error_response and batch".to_owned(),
        })
    })?;
    let flags = FrameFlags::from_bits(reader.get_u16().map_err(protocol)?)?;
    // The batch marker is carried twice — as a flag and as a kind — so the two can be required
    // to agree. A peer that set only one of them is describing a frame this decoder would have
    // to guess about, and guessing is what the desynchronisation rule exists to prevent.
    let is_batch = flags.contains(FrameFlags::BATCH);
    if is_batch != (kind == FrameKind::Batch) {
        return Err(framing_error(ErrorKind::Protocol {
            reason: format!(
                "frame kind {kind} and BATCH flag {is_batch} disagree; a batch must be both"
            ),
        }));
    }
    Ok(FrameHeader::new(
        kind,
        flags,
        reader.get_u32().map_err(protocol)?,
    ))
}

/// Parses the buffered header and enforces the payload limit. Performs no allocation.
fn validate_header_bytes(raw: &[u8], max_payload_bytes: u32) -> Result<FrameHeader> {
    let header = parse_header_bytes(raw)?;
    if header.payload_len > max_payload_bytes {
        return Err(framing_error(ErrorKind::LimitExceeded {
            resource: "ipc.frame.payload_bytes".to_owned(),
            limit: u64::from(max_payload_bytes),
            observed: u64::from(header.payload_len),
        }));
    }
    Ok(header)
}

/// Encodes a frame to bytes.
///
/// Infallible: the payload length is carried in the header the caller supplies, so a
/// well-formed `Frame` always produces a well-formed encoding. The size limit is the
/// *decoder's* concern — an encoder that produces an oversized frame produces one its peer
/// will reject, which is the correct place for that error to surface.
#[must_use]
pub fn encode_frame(header: &FrameHeader, payload: &[u8]) -> Vec<u8> {
    let mut writer = BufWriter::with_capacity(FRAME_HEADER_BYTES + payload.len());
    writer.put_u32(FRAME_MAGIC);
    writer.put_u8(PROTOCOL_VERSION);
    writer.put_u8(header.kind.as_u8());
    writer.put_u16(header.flags.bits());
    writer.put_u32(header.payload_len);
    writer.put_slice(payload);
    writer.finish()
}

/// Builds a codec error attributed to a framing operation.
fn framing_error(kind: ErrorKind) -> NcError {
    NcError::new(kind).operation("ipc::frame")
}

/// A streaming decoder that turns an arbitrary byte stream into frames.
///
/// Feed whatever the transport read; call [`FrameDecoder::next_frame`] until it returns
/// `None`. The decoder holds at most `FRAME_HEADER_BYTES + max_payload_bytes` bytes regardless
/// of how much is fed or how long the peer stalls, which is what makes it safe to point at a
/// hostile socket.
#[derive(Debug)]
pub struct FrameDecoder {
    max_payload_bytes: u32,
    buffer: Vec<u8>,
    /// Frames already extracted but not yet handed to the caller.
    ///
    /// Needed because `feed` extracts eagerly: a transport reads a chunk that may hold several
    /// frames, and the frames that were complete at feed time must not be discarded just
    /// because the caller has not asked for them yet. Bounded by the same maximum as the
    /// input the caller chose to feed, so it adds no unbounded growth of its own.
    ready: VecDeque<Frame>,
    /// Set once framing is violated. A desynchronised stream has no recoverable position, so
    /// every later call fails instead of guessing.
    desynchronised: Option<NcError>,
}

impl FrameDecoder {
    /// Creates a decoder that accepts frames of at most `max_frame_bytes` including the
    /// header, matching the unit of
    /// [`Limits::max_ipc_frame_bytes`](ncui_core::Limits::max_ipc_frame_bytes).
    ///
    /// Rejects a limit that cannot even hold a header, and one above what the protocol's
    /// length field can represent.
    pub fn new(max_frame_bytes: u32) -> Result<Self> {
        if max_frame_bytes < FRAME_HEADER_BYTES as u32 {
            return Err(ncui_core::error::validation(
                "max_frame_bytes",
                format!("must be at least {FRAME_HEADER_BYTES}, the frame header size"),
            ));
        }
        if max_frame_bytes > PROTOCOL_MAX_FRAME_BYTES {
            return Err(ncui_core::error::validation(
                "max_frame_bytes",
                format!("must not exceed the protocol maximum of {PROTOCOL_MAX_FRAME_BYTES}"),
            ));
        }
        Ok(Self {
            max_payload_bytes: max_frame_bytes - FRAME_HEADER_BYTES as u32,
            buffer: Vec::with_capacity(FRAME_HEADER_BYTES),
            ready: VecDeque::new(),
            desynchronised: None,
        })
    }

    /// The largest payload this decoder accepts.
    #[must_use]
    pub const fn max_payload_bytes(&self) -> u32 {
        self.max_payload_bytes
    }

    /// How many bytes are buffered but not yet part of a returned frame.
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// How many complete frames are waiting to be returned.
    #[must_use]
    pub fn ready_count(&self) -> usize {
        self.ready.len()
    }

    /// Whether the decoder holds neither a partial frame nor an unread complete one.
    #[must_use]
    pub fn is_at_boundary(&self) -> bool {
        self.buffer.is_empty() && self.ready.is_empty()
    }

    /// Whether framing has been violated and the decoder has stopped.
    #[must_use]
    pub const fn is_desynchronised(&self) -> bool {
        self.desynchronised.is_some()
    }

    /// Appends bytes from the transport.
    ///
    /// Never allocates more than the configured maximum: bytes are copied in only as far as the
    /// current partial frame needs. A peer that sends megabytes of a frame it never finishes
    /// costs the decoder one bounded buffer and an eventual timeout in the transport above.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(err) = &self.desynchronised {
            return Err(err.clone());
        }
        let mut rest = bytes;
        // Fill, validate, extract, repeat. The buffer therefore never holds more than one
        // partial frame, and a caller may feed a chunk holding many frames without them
        // accumulating in the decoder.
        while !rest.is_empty() {
            if self.buffer.len() < FRAME_HEADER_BYTES {
                let want = (FRAME_HEADER_BYTES - self.buffer.len()).min(rest.len());
                self.buffer.extend_from_slice(&rest[..want]);
                rest = &rest[want..];
                if self.buffer.len() < FRAME_HEADER_BYTES {
                    return Ok(());
                }
            }
            // The header is complete, so the length can be validated before a single payload
            // byte is reserved. This is the allocation bound.
            let header = match self.parse_header() {
                Ok(header) => header,
                Err(err) => return Err(self.desynchronise(err)),
            };
            let total = FRAME_HEADER_BYTES + header.payload_len as usize;
            if self.buffer.len() < total {
                let want = (total - self.buffer.len()).min(rest.len());
                self.buffer.extend_from_slice(&rest[..want]);
                rest = &rest[want..];
            }
            if self.buffer.len() == total {
                let frame = self.extract_frame(header);
                self.ready.push_back(frame);
            }
        }
        Ok(())
    }

    /// Returns the next complete frame, or `None` if more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        if let Some(err) = &self.desynchronised {
            return Err(err.clone());
        }
        if let Some(frame) = self.ready.pop_front() {
            return Ok(Some(frame));
        }
        if self.buffer.len() < FRAME_HEADER_BYTES {
            return Ok(None);
        }
        let header = match self.parse_header() {
            Ok(header) => header,
            Err(err) => return Err(self.desynchronise(err)),
        };
        // The header can be complete while the payload is still arriving. Returning a frame
        // here would hand the caller a silently truncated payload, so wait for the rest.
        if self.buffer.len() < FRAME_HEADER_BYTES + header.payload_len as usize {
            return Ok(None);
        }
        let frame = self.extract_frame(header);
        Ok(Some(frame))
    }

    /// Errors unless the decoder is positioned exactly on a frame boundary.
    ///
    /// A transport calls this at end of stream: bytes left over mean the peer sent a frame it
    /// never finished, which is a truncated message and must not be mistaken for a clean close.
    pub fn check_complete(&self) -> Result<()> {
        if let Some(err) = &self.desynchronised {
            return Err(err.clone());
        }
        if self.buffer.is_empty() {
            return Ok(());
        }
        Err(framing_error(ErrorKind::Protocol {
            reason: format!(
                "stream ended mid-frame: {} byte(s) buffered, incomplete",
                self.buffer.len()
            ),
        }))
    }

    /// Records a framing violation and hands it back for the caller to return.
    ///
    /// Split this way rather than returning `Result<()>` because the two callers return
    /// different types, and a helper that returned `Result` would force each of them to
    /// discard the frame it was in the middle of building.
    fn desynchronise(&mut self, err: NcError) -> NcError {
        let err = if err.operation_name() == "unspecified" {
            err.operation("ipc::frame")
        } else {
            err
        };
        self.desynchronised = Some(err.clone());
        err
    }

    /// Validates the buffered header, enforcing the payload limit. Performs no allocation.
    fn parse_header(&self) -> Result<FrameHeader> {
        validate_header_bytes(&self.buffer[..FRAME_HEADER_BYTES], self.max_payload_bytes)
    }

    /// Removes a complete frame from the front of the buffer.
    ///
    /// Takes the already-validated header rather than re-parsing it: re-parsing would mean
    /// handling a second failure mode that the caller has already excluded, and a fallback
    /// value there would be exactly the silent misinterpretation this crate promises not to
    /// do. `payload_len` was checked against the configured maximum, so the allocation below
    /// is bounded by configuration and never by the peer.
    fn extract_frame(&mut self, header: FrameHeader) -> Frame {
        let total = FRAME_HEADER_BYTES + header.payload_len as usize;
        let payload = self.buffer[FRAME_HEADER_BYTES..total].to_vec();
        self.buffer.drain(..total);
        Frame::new(header, payload)
    }
}

/// Decodes the frames of a complete byte stream.
///
/// Convenience for tests and for transports that read a whole message at a time. It enforces
/// the same rules as [`FrameDecoder`], including the end-of-stream check.
pub fn decode_all(bytes: &[u8], max_frame_bytes: u32) -> Result<Vec<Frame>> {
    let mut decoder = FrameDecoder::new(max_frame_bytes)?;
    decoder.feed(bytes)?;
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame()? {
        frames.push(frame);
    }
    decoder.check_complete()?;
    Ok(frames)
}

/// Reads the payload of a batch frame as a sequence of frames.
///
/// A batch is a sequence of complete frames, so this is an ordinary [`FrameDecoder`] pointed at
/// the batch payload. It rejects a nested batch, which is what makes the nesting depth a
/// constant one rather than a function of the peer's goodwill.
pub fn decode_batch(payload: &[u8], max_frame_bytes: u32) -> Result<Vec<Frame>> {
    let frames = decode_all(payload, max_frame_bytes)?;
    if frames.iter().any(|frame| frame.header().is_batch()) {
        return Err(framing_error(ErrorKind::Protocol {
            reason: "a batch may not contain another batch; nesting must stay one level deep"
                .to_owned(),
        }));
    }
    Ok(frames)
}

/// Builds the frame that carries a batch payload.
///
/// Separated from the caller so the batch kind and the batch flag can never be set
/// independently of one another.
pub(crate) fn encode_batch_frame(payload: &[u8], max_frame_bytes: u32) -> Result<Vec<u8>> {
    let total = FRAME_HEADER_BYTES + payload.len();
    let max = usize::try_from(max_frame_bytes).unwrap_or(usize::MAX);
    if total > max {
        return Err(framing_error(ErrorKind::LimitExceeded {
            resource: "ipc.batch.bytes".to_owned(),
            limit: u64::from(max_frame_bytes),
            observed: total as u64,
        }));
    }
    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        framing_error(ErrorKind::LimitExceeded {
            resource: "ipc.batch.bytes".to_owned(),
            limit: u64::from(max_frame_bytes),
            observed: payload.len() as u64,
        })
    })?;
    let header = FrameHeader::new(FrameKind::Batch, FrameFlags::BATCH, payload_len);
    Ok(encode_frame(&header, payload))
}

/// Rejects a frame whose payload version or discriminator disagrees with its header.
///
/// Exposed for [`crate::message`], which is the only other place a frame is interpreted. It
/// lives here because the rule is about the relationship between the two layers, not about
/// either one alone.
pub(crate) fn check_payload_header(
    payload: &[u8],
    expected_kind: FrameKind,
    expected_discriminator: u8,
) -> Result<BufReader<'_>> {
    let mut reader = BufReader::new(payload, "ipc::message");
    let version = reader.get_u8().map_err(|_| {
        framing_error(ErrorKind::Protocol {
            reason: "payload is too short to contain a payload version".to_owned(),
        })
    })?;
    if version != crate::message::PAYLOAD_VERSION {
        return Err(framing_error(ErrorKind::Protocol {
            reason: format!(
                "unsupported payload version {version}, this build speaks {}",
                crate::message::PAYLOAD_VERSION
            ),
        }));
    }
    let discriminator = reader.get_u8().map_err(|_| {
        framing_error(ErrorKind::Protocol {
            reason: "payload is too short to contain a discriminator".to_owned(),
        })
    })?;
    if discriminator != expected_discriminator {
        return Err(framing_error(ErrorKind::Protocol {
            reason: format!(
                "payload discriminator 0x{discriminator:02x} does not match frame kind {expected_kind}"
            ),
        }));
    }
    Ok(reader)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )]

    use super::*;

    /// A limit that is generous for the test payloads but still small enough to trip.
    const LIMIT: u32 = 1024;

    fn header(kind: FrameKind, len: u32) -> FrameHeader {
        FrameHeader::new(kind, FrameFlags::NONE, len)
    }

    #[test]
    fn a_frame_round_trips_through_the_decoder() {
        for kind in [
            FrameKind::Request,
            FrameKind::Response,
            FrameKind::Event,
            FrameKind::ErrorResponse,
        ] {
            let payload = vec![0xA5; 7];
            let bytes = encode_frame(&header(kind, payload.len() as u32), &payload);
            let frames = decode_all(&bytes, LIMIT).unwrap();
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].header().kind, kind);
            assert_eq!(frames[0].payload(), payload.as_slice());
        }
    }

    #[test]
    fn the_header_is_twelve_bytes_in_the_documented_layout() {
        let bytes = encode_frame(&header(FrameKind::Request, 3), b"abc");
        assert_eq!(bytes.len(), FRAME_HEADER_BYTES + 3);
        assert_eq!(&bytes[0..4], b"NCUI");
        assert_eq!(bytes[4], PROTOCOL_VERSION);
        assert_eq!(bytes[5], 0x01);
        assert_eq!(&bytes[6..8], &[0, 0]);
        assert_eq!(&bytes[8..12], &[0, 0, 0, 3]);
        assert_eq!(&bytes[12..], b"abc");
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut bytes = encode_frame(&header(FrameKind::Request, 0), b"");
        bytes[0] = b'X';
        let err = decode_all(&bytes, LIMIT).expect_err("bad magic must be rejected");
        assert!(matches!(err.kind(), ErrorKind::Protocol { .. }));
        assert!(err.to_string().contains("magic"), "got: {err}");
    }

    #[test]
    fn an_unsupported_version_is_rejected() {
        let mut bytes = encode_frame(&header(FrameKind::Request, 0), b"");
        bytes[4] = 99;
        let err = decode_all(&bytes, LIMIT).expect_err("bad version must be rejected");
        assert!(
            err.to_string().contains("unsupported frame version"),
            "got: {err}"
        );
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        let mut bytes = encode_frame(&header(FrameKind::Request, 0), b"");
        bytes[5] = 0x7F;
        let err = decode_all(&bytes, LIMIT).expect_err("unknown kind must be rejected");
        assert!(err.to_string().contains("unknown frame kind"), "got: {err}");
    }

    #[test]
    fn an_undefined_flag_bit_is_rejected_rather_than_ignored() {
        let mut bytes = encode_frame(&header(FrameKind::Request, 0), b"");
        bytes[6] = 0x80; // bit 7 is not defined by this version
        let err = decode_all(&bytes, LIMIT).expect_err("unknown flag must be rejected");
        assert!(err.to_string().contains("undefined flag bit"), "got: {err}");
    }

    #[test]
    fn the_batch_flag_is_honoured() {
        let inner = encode_frame(&header(FrameKind::Event, 2), b"hi");
        let bytes = encode_batch_frame(&inner, LIMIT).unwrap();
        let frames = decode_all(&bytes, LIMIT).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].header().is_batch());
        assert_eq!(frames[0].header().kind, FrameKind::Batch);
        let inner_frames = decode_batch(frames[0].payload(), LIMIT).unwrap();
        assert_eq!(inner_frames.len(), 1);
        assert_eq!(inner_frames[0].payload(), b"hi");
    }

    #[test]
    fn a_batch_flag_without_the_batch_kind_is_rejected() {
        let mut bytes = encode_frame(&header(FrameKind::Event, 0), b"");
        bytes[6] = 0x00;
        bytes[7] = 0x01; // BATCH flag set, kind left as Event
        let err = decode_all(&bytes, LIMIT).expect_err("the two markers must agree");
        assert!(err.to_string().contains("disagree"), "got: {err}");
    }

    #[test]
    fn the_batch_kind_without_the_flag_is_rejected() {
        let mut bytes = encode_frame(&header(FrameKind::Batch, 0), b"");
        bytes[5] = FrameKind::Batch.as_u8();
        bytes[6] = 0x00;
        bytes[7] = 0x00; // kind says batch, flag does not
        let err = decode_all(&bytes, LIMIT).expect_err("the two markers must agree");
        assert!(err.to_string().contains("disagree"), "got: {err}");
    }

    #[test]
    fn an_oversized_batch_is_rejected_before_it_is_produced() {
        let inner = encode_frame(&header(FrameKind::Event, 64), &[0u8; 64]);
        let err = encode_batch_frame(&inner, 32).expect_err("an oversized batch must be rejected");
        assert!(matches!(err.kind(), ErrorKind::LimitExceeded { .. }));
    }

    #[test]
    fn a_nested_batch_is_rejected() {
        let inner = encode_frame(&header(FrameKind::Event, 0), b"");
        let inner_batch = encode_batch_frame(&inner, LIMIT).unwrap();
        let bytes = encode_batch_frame(&inner_batch, LIMIT).unwrap();
        let frames = decode_all(&bytes, LIMIT).unwrap();
        let err = decode_batch(frames[0].payload(), LIMIT).expect_err("nesting must be rejected");
        assert!(err.to_string().contains("nesting"), "got: {err}");
    }

    #[test]
    fn an_oversized_length_is_rejected_and_costs_nothing() {
        // Four bytes of payload plus a header claiming the maximum possible length.
        let mut bytes = encode_frame(&header(FrameKind::Request, 0), b"");
        bytes[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        let err = decode_all(&bytes, LIMIT).expect_err("oversized length must be rejected");
        match err.kind() {
            ErrorKind::LimitExceeded {
                limit, observed, ..
            } => {
                assert_eq!(*limit, u64::from(LIMIT - FRAME_HEADER_BYTES as u32));
                assert_eq!(*observed, u64::from(u32::MAX));
            }
            other => panic!("expected LimitExceeded, got {other:?}"),
        }
    }

    #[test]
    fn a_truncated_frame_is_reported_at_end_of_stream() {
        let bytes = encode_frame(&header(FrameKind::Request, 8), b"12345678");
        let mut decoder = FrameDecoder::new(LIMIT).unwrap();
        let partial = bytes.len() - 3;
        decoder.feed(&bytes[..partial]).unwrap();
        assert!(decoder.next_frame().unwrap().is_none());
        let err = decoder
            .check_complete()
            .expect_err("truncation must be reported");
        assert!(err.to_string().contains("mid-frame"), "got: {err}");
    }

    #[test]
    fn a_truncated_header_alone_is_reported() {
        let bytes = encode_frame(&header(FrameKind::Request, 0), b"");
        let mut decoder = FrameDecoder::new(LIMIT).unwrap();
        decoder.feed(&bytes[..5]).unwrap();
        assert!(decoder.next_frame().unwrap().is_none());
        assert!(decoder.check_complete().is_err());
    }

    #[test]
    fn a_desynchronised_decoder_refuses_all_further_work() {
        let mut bytes = encode_frame(&header(FrameKind::Request, 0), b"");
        bytes[0] = b'Z';
        let mut decoder = FrameDecoder::new(LIMIT).unwrap();
        assert!(decoder.feed(&bytes).is_err());
        assert!(decoder.is_desynchronised());
        // A later well-formed frame must not be served: the stream position is unknown.
        let good = encode_frame(&header(FrameKind::Event, 1), b"x");
        assert!(decoder.feed(&good).is_err());
        assert!(decoder.next_frame().is_err());
        assert!(decoder.check_complete().is_err());
    }

    #[test]
    fn many_frames_in_one_feed_all_come_back_in_order() {
        let mut stream = Vec::new();
        for i in 0..50u8 {
            stream.extend_from_slice(&encode_frame(&header(FrameKind::Event, 1), &[i]));
        }
        let mut decoder = FrameDecoder::new(LIMIT).unwrap();
        decoder.feed(&stream).unwrap();
        let mut seen = 0u8;
        while let Some(frame) = decoder.next_frame().unwrap() {
            assert_eq!(frame.payload(), &[seen]);
            seen += 1;
        }
        assert_eq!(seen, 50);
        decoder.check_complete().unwrap();
    }

    #[test]
    fn a_limit_smaller_than_a_header_is_rejected() {
        assert!(FrameDecoder::new(FRAME_HEADER_BYTES as u32 - 1).is_err());
        assert!(FrameDecoder::new(FRAME_HEADER_BYTES as u32).is_ok());
    }

    #[test]
    fn a_limit_above_the_protocol_maximum_is_rejected() {
        assert!(FrameDecoder::new(PROTOCOL_MAX_FRAME_BYTES + 1).is_err());
        assert!(FrameDecoder::new(PROTOCOL_MAX_FRAME_BYTES).is_ok());
    }

    #[test]
    fn the_decoder_reports_its_partial_and_queued_state() {
        let bytes = encode_frame(&header(FrameKind::Event, 4), b"1234");
        let second = encode_frame(&header(FrameKind::Event, 4), b"5678");
        let mut stream = bytes.clone();
        stream.extend_from_slice(&second);

        let mut decoder = FrameDecoder::new(LIMIT).unwrap();
        assert_eq!(decoder.buffered_len(), 0);
        assert_eq!(decoder.ready_count(), 0);
        assert!(decoder.is_at_boundary(), "a fresh decoder is on a boundary");

        // Each frame is 12 + 4 = 16 bytes, so 20 bytes is one whole frame plus four bytes of
        // the next frame's header: one frame ready, one partial.
        decoder.feed(&stream[..20]).unwrap();
        assert_eq!(decoder.ready_count(), 1, "the complete frame is queued");
        assert_eq!(
            decoder.buffered_len(),
            4,
            "the partial frame is still buffered"
        );
        assert!(!decoder.is_at_boundary());

        assert_eq!(
            decoder
                .next_frame()
                .unwrap()
                .expect("ready frame")
                .payload(),
            b"1234"
        );
        assert_eq!(decoder.ready_count(), 0);

        decoder.feed(&stream[20..]).unwrap();
        assert_eq!(
            decoder
                .next_frame()
                .unwrap()
                .expect("ready frame")
                .payload(),
            b"5678"
        );
        assert!(decoder.is_at_boundary());
        assert!(decoder.check_complete().is_ok());
    }

    #[test]
    fn a_zero_length_payload_frame_is_legal() {
        let bytes = encode_frame(&header(FrameKind::Event, 0), b"");
        let frames = decode_all(&bytes, LIMIT).unwrap();
        assert_eq!(frames[0].payload(), b"");
    }
}
