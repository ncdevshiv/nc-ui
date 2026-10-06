//! Binary wire protocol, typed envelopes, correlation, cancellation and backpressure for
//! NC-UI inter-process messaging (ADR-003).
//!
//! This crate owns the *transport vocabulary*: how a message is put on a byte stream, how a
//! byte stream becomes a message, how an in-flight request is tracked, how work is cancelled,
//! and what happens when a bounded queue fills. It owns no sockets. A transport (the `WebView`
//! bridge, a worker pipe, the agent socket) supplies bytes and dispatches the messages this
//! crate produces; that binding is deliberately outside this crate so the protocol can be
//! tested exhaustively without a platform.
//!
//! # Layering
//!
//! Layer 1 (ADR-011). Depends on `ncui-core` and nothing else inside the workspace.
//!
//! # Wire format
//!
//! Every integer on the wire is **big-endian**. A stream is a concatenation of frames; there
//! is no other framing, no length prefix outside the header, and no padding.
//!
//! ## Frame header (12 bytes, fixed)
//!
//! ```text
//! offset  size  field         value
//! 0       4     magic         0x4E43_5549  (ASCII "NCUI", big-endian)
//! 4       1     version       1
//! 5       1     kind          FrameKind discriminant
//! 6       2     flags         u16 bitfield; bit 0 = BATCH, every other bit reserved
//! 8       4     payload_len   u32, count of payload bytes that follow
//! ```
//!
//! The payload is exactly `payload_len` bytes. No field is optional and there is no
//! alignment padding, so a frame is trivially portable across architectures.
//!
//! The **magic** is the ASCII bytes `NCUI` read as a big-endian `u32`. It is not an arbitrary
//! constant: it makes a mistyped or desynchronised stream legible in a hex dump, and it is
//! specific enough that a random 4-byte prefix will not accidentally validate.
//!
//! ## Frame flags
//!
//! * `BATCH` (bit 0) — the payload is a sequence of complete frames rather than a message
//!   payload. Batching coalesces several small messages into one transport write, which
//!   matters because a write syscall per message is the dominant cost at high message rates.
//!   Batches are **one level deep**: a frame inside a batch must not itself set `BATCH`.
//!
//! Every other bit is reserved and must be zero. A peer that sets a bit this version does not
//! understand is rejected with [`ncui_core::ErrorKind::Protocol`] rather than ignored, because silently
//! ignoring a flag means silently misinterpreting a payload the sender believed was marked.
//!
//! ## Message payload layout (version 1)
//!
//! Every message payload begins with two bytes:
//!
//! ```text
//! offset 0  payload_version  u8, currently 1
//! offset 1  discriminator    u8, selects the variant
//! ```
//!
//! The frame header already carries a version and a kind; the payload repeats them so that a
//! payload is self-describing and can be validated (and unit-tested) without its frame. The
//! two must agree — a payload whose `payload_version` or discriminator disagrees with its frame
//! is a [`ncui_core::ErrorKind::Protocol`] violation, never a silent reinterpretation.
//!
//! Discriminators: `0x01` request, `0x02` response, `0x03` event, `0x04` error response.
//!
//! ```text
//! request  (0x01), 46-byte fixed prefix
//!   0   u8  payload_version
//!   1   u8  discriminator
//!   2   u128 correlation id
//!  18   u128 request id
//!  34   u64  deadline_ms, relative to send, 0 = no deadline
//!  42   u32  method_len
//!  46   ..  method bytes (UTF-8), then params_len u32, then params bytes
//!
//! response (0x02), 38-byte fixed prefix
//!   0   u8  payload_version
//!   1   u8  discriminator
//!   2   u128 request id
//!  18   u128 correlation id
//!  34   u32  result_len, then result bytes
//!
//! event    (0x03), 22-byte fixed prefix
//!   0   u8  payload_version
//!   1   u8  discriminator
//!   2   u128 correlation id
//!  18   u32  topic_len
//!  22   ..  topic bytes (UTF-8), then payload_len u32, then payload bytes
//!
//! error response (0x04), 34-byte fixed prefix, then the error record
//!   0   u8  payload_version
//!   1   u8  discriminator
//!   2   u128 request id
//!  18   u128 correlation id
//!  34   ..  error record (see `error_codec`)
//! ```
//!
//! Identifiers travel as raw 128-bit big-endian integers, not as their 32-character hex
//! rendering. The hex form exists for logs and JSON; carrying it on the hot path would cost
//! eight bytes and a parse per identifier for no benefit. Zero is the reserved null
//! identifier, so a decoded id of zero is a protocol violation rather than a valid id.
//!
//! Strings and byte blobs are length-prefixed with a `u32`. Lengths are validated against the
//! bytes actually remaining in the payload *before* any allocation, so a crafted length can
//! never cause a large read or a large copy.
//!
//! `params`, `result`, `payload` and the event `payload` are **opaque bytes**. The transport
//! does not interpret them; the method or topic name says what schema they hold. That is what
//! keeps the hot path free of a value parser.
//!
//! # Invariants
//!
//! These are the properties ADR-003 calls load-bearing. Each is enforced by code and covered
//! by a named test.
//!
//! 1. **A hostile length never causes a large allocation.** The decoder checks `payload_len`
//!    against the configured maximum before it reserves anything, and the buffer never grows
//!    past `header + maximum`. A four-byte stream claiming a 4 GiB payload costs four bytes.
//! 2. **Malformed input never panics and always yields `Err`.** Magic, version, kind, flags,
//!    discriminator, payload version and every length prefix are validated. There is no
//!    infallible entry point into the decoder.
//! 3. **Request identifiers are unique.** Registering a live id twice is a
//!    [`ncui_core::ErrorKind::Conflict`]; it is never a silent overwrite.
//! 4. **A response maps to exactly one request.** Completing, failing or cancelling an unknown
//!    request is a [`ncui_core::ErrorKind::NotFound`], never a silent drop.
//! 5. **Nothing is silently dropped.** A queue that discards work does so only under an
//!    explicit [`BackpressurePolicy::DropOldest`], and it counts what it dropped.
//! 6. **No unbounded queue exists.** Every buffer in this crate is bounded by a configured
//!    maximum, and the channel cannot be constructed with a zero capacity.
//!
//! # Relationship to the JSON control plane
//!
//! JSON is the agent-facing control plane (ADR-008), not the hot path. [`Message::to_json_value`]
//! and [`Message::from_json_value`] convert to and from a tagged JSON object that carries the same
//! logical fields, so the two representations cannot drift: the tests round-trip the same
//! messages through both. Opaque byte fields are JSON arrays of numbers there, because an
//! agent needs to read them and a base64 alphabet would just move the decoding problem.
//!
//! [`ErrorKind`]: ncui_core::ErrorKind
//! [`BackpressurePolicy::DropOldest`]: crate::BackpressurePolicy::DropOldest

// Test code deliberately exercises panicking paths: a failed assertion or an `unwrap` in a
// test must abort so the failure is visible rather than being swallowed by a `Result`.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

pub mod cancel;
pub mod channel;
pub mod frame;
pub mod message;
pub mod request;

mod codec;
mod error_codec;

pub use cancel::{CancellationToken, Cancelled};
pub use channel::{BackpressurePolicy, BoundedChannel};
pub use frame::{
    FRAME_HEADER_BYTES, FRAME_MAGIC, Frame, FrameDecoder, FrameFlags, FrameHeader, FrameKind,
    PROTOCOL_VERSION, decode_all, decode_batch, encode_frame,
};
pub use message::{
    Message, PAYLOAD_VERSION, decode_messages, decode_messages_with_limits, encode_messages,
};
pub use request::{CompletedRequest, RequestOutcome, RequestRegistry};
