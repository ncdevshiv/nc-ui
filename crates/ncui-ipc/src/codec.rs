//! Big-endian primitive readers and writers shared by the framing and message codecs.
//!
//! Every length-prefixed read validates the declared length against the bytes actually
//! remaining *before* it copies anything. That ordering is the single reason this crate can
//! promise that hostile input costs bounded work: a `take(len)` on a hostile `len` returns an
//! error, it does not allocate.
//!
//! This module is private. It is a helper, not part of the crate's contract; the public
//! surface is [`crate::frame`] and [`crate::message`].

use ncui_core::{ErrorKind, NcError, Result};

/// Builds a codec error attributed to `operation`.
///
/// Codec failures are reported as [`ErrorKind::Codec`] rather than [`ErrorKind::Protocol`]
/// because the frame layer already reserves `Protocol` for violations of the *framing* rules;
/// a bad length inside an otherwise well-framed payload is a serialisation failure, and the
/// distinction matters when a peer sends a truncated frame (protocol) versus a corrupted one
/// (codec).
fn codec_err(operation: &'static str, reason: impl Into<String>) -> NcError {
    NcError::new(ErrorKind::Codec {
        operation: operation.to_owned(),
        reason: reason.into(),
    })
}

/// Appends big-endian primitives to a growable buffer.
#[derive(Debug, Default, Clone)]
pub(crate) struct BufWriter {
    bytes: Vec<u8>,
}

impl BufWriter {
    /// Creates an empty writer.
    pub(crate) const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Creates a writer with room reserved for `capacity` bytes.
    ///
    /// Reserving up front is worth it on the encode path: the encoded length is known, so the
    /// buffer grows once instead of log2(n) times.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
        }
    }

    /// Writes one byte.
    pub(crate) fn put_u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    /// Writes a big-endian `u16`.
    pub(crate) fn put_u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes a big-endian `u32`.
    pub(crate) fn put_u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes a big-endian `u64`.
    pub(crate) fn put_u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes a big-endian `u128`, which is how every NC-UI identifier travels.
    pub(crate) fn put_u128(&mut self, value: u128) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes raw bytes with no length prefix.
    pub(crate) fn put_slice(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    /// Writes a `u32` length prefix followed by the bytes.
    pub(crate) fn put_bytes(&mut self, value: &[u8]) -> Result<()> {
        let len = u32::try_from(value.len()).map_err(|_| {
            codec_err(
                "codec::write",
                format!(
                    "blob of {} bytes exceeds the u32 length prefix",
                    value.len()
                ),
            )
        })?;
        self.put_u32(len);
        self.put_slice(value);
        Ok(())
    }

    /// Writes a `u32` length prefix followed by the string's UTF-8 bytes.
    pub(crate) fn put_str(&mut self, value: &str) -> Result<()> {
        self.put_bytes(value.as_bytes())
    }

    /// Consumes the writer and returns the encoded bytes.
    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Reads big-endian primitives out of a borrowed slice.
///
/// The reader never panics and never allocates more than the input it was handed: every
/// accessor reports [`ErrorKind::Codec`] when the input is short, and every `take` is checked
/// against the remaining length first.
#[derive(Debug, Clone)]
pub(crate) struct BufReader<'a> {
    bytes: &'a [u8],
    pos: usize,
    operation: &'static str,
}

impl<'a> BufReader<'a> {
    /// Creates a reader over `bytes`, attributing failures to `operation`.
    pub(crate) const fn new(bytes: &'a [u8], operation: &'static str) -> Self {
        Self {
            bytes,
            pos: 0,
            operation,
        }
    }

    /// Builds a codec error for this reader.
    fn err(&self, reason: impl Into<String>) -> NcError {
        codec_err(self.operation, reason)
    }

    /// How many bytes are still unread.
    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    /// Whether every byte has been consumed.
    pub(crate) fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Rejects trailing bytes that a well-formed payload must not contain.
    ///
    /// A payload with trailing garbage is rejected rather than tolerated: tolerating it would
    /// let a peer append data that a future version might interpret, which is exactly the
    /// silent-drift failure the version byte exists to prevent.
    pub(crate) fn expect_end(&self) -> Result<()> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(self.err(format!(
                "{} trailing byte(s) after the payload",
                self.remaining()
            )))
        }
    }

    /// Borrows the next `len` bytes without copying.
    ///
    /// This is the guard that makes the crate's allocation bound hold. `len` is attacker
    /// controlled, so it is checked against `remaining()` and the addition is written so that
    /// even a `len` near `usize::MAX` cannot overflow the cursor.
    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let remaining = self.remaining();
        if len > remaining {
            return Err(self.err(format!(
                "declared length {len} exceeds {remaining} remaining byte(s)"
            )));
        }
        let start = self.pos;
        // `len <= remaining <= bytes.len() - pos`, so this cannot overflow.
        self.pos = start + len;
        Ok(&self.bytes[start..self.pos])
    }

    /// Reads one byte.
    pub(crate) fn get_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Reads a big-endian `u16`.
    pub(crate) fn get_u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// Reads a big-endian `u32`.
    pub(crate) fn get_u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads a big-endian `u64`.
    pub(crate) fn get_u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        let mut buffer = [0u8; 8];
        buffer.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(buffer))
    }

    /// Reads a big-endian `u128`.
    pub(crate) fn get_u128(&mut self) -> Result<u128> {
        let bytes = self.take(16)?;
        let mut buffer = [0u8; 16];
        buffer.copy_from_slice(bytes);
        Ok(u128::from_be_bytes(buffer))
    }

    /// Reads a `u32` length prefix and returns the bytes it describes.
    pub(crate) fn get_bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.get_u32()? as usize;
        self.take(len)
    }

    /// Reads a `u32` length prefix and validates the payload as UTF-8.
    ///
    /// UTF-8 validation is not cosmetic: method and topic names are compared and dispatched
    /// on, and a lossy conversion would turn two distinct byte sequences into one name.
    pub(crate) fn get_str(&mut self) -> Result<&'a str> {
        let raw = self.get_bytes()?;
        core::str::from_utf8(raw)
            .map_err(|e| self.err(format!("string field is not valid UTF-8: {e}")))
    }
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

    fn reader(bytes: &[u8]) -> BufReader<'_> {
        BufReader::new(bytes, "codec::test")
    }

    #[test]
    fn integers_round_trip_big_endian() {
        let mut writer = BufWriter::new();
        writer.put_u8(0xAB);
        writer.put_u16(0x1234);
        writer.put_u32(0xDEAD_BEEF);
        writer.put_u64(u64::MAX);
        writer.put_u128(1 << 100);
        let bytes = writer.finish();

        // The first byte after the u8 must be the high byte of the u16, proving big-endian.
        assert_eq!(bytes[1], 0x12);
        assert_eq!(bytes[2], 0x34);

        let mut r = reader(&bytes);
        assert_eq!(r.get_u8().unwrap(), 0xAB);
        assert_eq!(r.get_u16().unwrap(), 0x1234);
        assert_eq!(r.get_u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.get_u64().unwrap(), u64::MAX);
        assert_eq!(r.get_u128().unwrap(), 1 << 100);
        r.expect_end().unwrap();
    }

    #[test]
    fn length_prefixes_round_trip() {
        let mut writer = BufWriter::new();
        writer.put_str("topic/name").unwrap();
        writer.put_bytes(&[1, 2, 3]).unwrap();
        let bytes = writer.finish();

        let mut r = reader(&bytes);
        assert_eq!(r.get_str().unwrap(), "topic/name");
        assert_eq!(r.get_bytes().unwrap(), &[1, 2, 3]);
        r.expect_end().unwrap();
    }

    #[test]
    fn an_overlong_length_prefix_is_rejected_without_reading() {
        // A blob claiming 0xFFFF_FFFF bytes in a 4-byte input.
        let bytes = [0xFF, 0xFF, 0xFF, 0xFF, 0x00];
        let mut r = reader(&bytes);
        let err = r.get_bytes().expect_err("must reject the oversized length");
        assert!(matches!(err.kind(), ErrorKind::Codec { .. }));
        assert!(err.to_string().contains("exceeds"), "got: {err}");
    }

    #[test]
    fn a_truncated_integer_is_rejected_rather_than_read_past_the_end() {
        let bytes = [0x01, 0x02];
        let mut r = reader(&bytes);
        assert!(r.get_u32().is_err());
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let mut writer = BufWriter::new();
        writer.put_slice(&[0xFF, 0xFE, 0xFD]); // never valid UTF-8
        let mut body = writer.finish();
        // Prepend a length that matches.
        let mut framed = BufWriter::new();
        framed.put_u32(3);
        framed.put_slice(&body);
        body = framed.finish();

        let mut r = reader(&body);
        let err = r.get_str().expect_err("invalid utf-8 must be rejected");
        assert!(err.to_string().contains("UTF-8"), "got: {err}");
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let bytes = [1, 2, 3];
        let r = reader(&bytes);
        assert!(r.expect_end().is_err());
    }

    #[test]
    fn a_blob_length_is_written_big_endian_and_never_truncated() {
        // The length prefix must be the true length, never a narrowed one: a 32-bit length
        // field is what makes the reader's bounds check meaningful.
        let mut writer = BufWriter::new();
        writer.put_bytes(&[7u8; 16]).unwrap();
        writer.put_bytes(&[9u8; 300]).unwrap();
        let bytes = writer.finish();
        assert_eq!(&bytes[..4], &[0, 0, 0, 16]);
        assert_eq!(&bytes[20..24], &[0, 0, 0x01, 0x2C]);

        let mut r = reader(&bytes);
        assert_eq!(r.get_bytes().unwrap().len(), 16);
        assert_eq!(r.get_bytes().unwrap().len(), 300);
        r.expect_end().unwrap();
    }
}
