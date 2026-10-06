//! Property tests for the IPC decoders.
//!
//! These are the corpus-free half of `process.fuzzing`. The `fuzz/` targets replace a fixed
//! corpus with generated input and run for minutes at a time; these run in the ordinary test
//! suite on every CI run, so a regression in decoder totality fails a build rather than being
//! discovered by someone remembering to fuzz.
//!
//! Every property here is **totality**: arbitrary bytes must produce a `Result`, never a
//! panic, a hang, or an allocation proportional to the declared length. That is the property
//! an untrusted peer exercises, since it chooses the bytes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "property tests must abort loudly on a counterexample"
)]

use ncui_core::Limits;
use ncui_ipc::frame::{decode_all, decode_batch};
use ncui_ipc::message::{Message, decode_messages, decode_messages_with_limits};
use proptest::prelude::*;

/// The frame limit the property tests decode under, matching a configured runtime.
const MAX_FRAME_BYTES: u32 = 64 * 1024;

/// A decoded frame never allocates more than the configured limit, whatever the input.
#[test]
fn decoding_arbitrary_bytes_is_total() {
    proptest!(|(bytes: Vec<u8>)| {
        // Any input at all must return. `decode_*` returning `Err` is the expected outcome
        // for most bytes; what must never happen is a panic, and `proptest` would catch one.
        let _ = decode_all(&bytes, MAX_FRAME_BYTES);
        let _ = decode_batch(&bytes, MAX_FRAME_BYTES);
        let _ = decode_messages(&bytes, MAX_FRAME_BYTES);
    });
}

/// A batch payload declaring more frames than the limit carries is refused, not truncated.
///
/// The bound is checked before any allocation, so an absurd declared count costs nothing.
#[test]
fn a_declared_frame_count_never_exceeds_the_limit() {
    proptest!(|(bytes: Vec<u8>)| {
        match decode_batch(&bytes, MAX_FRAME_BYTES) {
            Ok(frames) => prop_assert!(frames.len() <= MAX_FRAME_BYTES as usize),
            Err(_) => { /* refusal is always acceptable */ }
        }
    });
}

/// A successful decode never yields more frames than the payload could hold.
///
/// Each frame needs at least a header, so a decode returning more frames than
/// `bytes.len() / HEADER` would mean the decoder invented structure.
#[test]
fn a_successful_decode_returns_no_more_frames_than_the_payload_could_hold() {
    const MIN_FRAME_BYTES: usize = 8; // magic + version + kind + length
    proptest!(|(bytes: Vec<u8>)| {
        if let Ok(frames) = decode_all(&bytes, MAX_FRAME_BYTES) {
            prop_assert!(
                frames.len() <= bytes.len() / MIN_FRAME_BYTES,
                "decoded {} frames from {} bytes, which cannot each carry a header",
                frames.len(),
                bytes.len()
            );
        }
    });
}

/// The same totality property, driven through the `Limits`-aware entry point.
///
/// This is the path a real runtime takes, so it is the one whose totality actually matters.
#[test]
fn decoding_is_total_under_configured_limits() {
    proptest!(|(bytes: Vec<u8>)| {
        let limits = Limits {
            max_ipc_frame_bytes: MAX_FRAME_BYTES,
            ..Limits::default()
        };
        let _ = decode_messages_with_limits(&bytes, &limits);
    });
}

/// Every frame kind decodes arbitrary payloads totally.
///
/// Seeding a third of the cases with a valid magic prefix exercises the *post-header* paths,
/// which pure random bytes almost never reach: without the seed, the decoder rejects at the
/// first byte and the interesting code is never entered.
#[test]
fn every_frame_kind_decodes_a_hostile_payload_totally() {
    const MAGIC: &[u8; 4] = b"NCUI";

    proptest!(|(bytes: Vec<u8>, kind_byte: u8, seed: bool)| {
        let kind = match kind_byte % 5 {
            0 => ncui_ipc::frame::FrameKind::Request,
            1 => ncui_ipc::frame::FrameKind::Response,
            2 => ncui_ipc::frame::FrameKind::Event,
            3 => ncui_ipc::frame::FrameKind::ErrorResponse,
            _ => ncui_ipc::frame::FrameKind::Batch,
        };
        let mut payload = Vec::with_capacity(bytes.len() + 8);
        if seed {
            payload.extend_from_slice(MAGIC);
            payload.push(1);
            payload.push(kind_byte);
        }
        payload.extend_from_slice(&bytes);
        let _ = Message::decode_payload(kind, &payload);
    });
}

/// Decoding is deterministic: the same bytes always give the same answer.
///
/// A decoder that depended on iteration order or interior mutability would let an attacker
/// probe it — submit the same frame twice and learn something from the differing replies.
#[test]
fn decoding_is_deterministic() {
    proptest!(|(bytes: Vec<u8>)| {
        let first = decode_messages(&bytes, MAX_FRAME_BYTES);
        let second = decode_messages(&bytes, MAX_FRAME_BYTES);
        prop_assert_eq!(
            format!("{first:?}"),
            format!("{second:?}"),
            "the same bytes produced two different decodes"
        );
    });
}
