#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests must abort loudly on failure"
)]

//! Integration tests for the protocol: round-trips through both representations, streaming
//! delivery across arbitrary chunk boundaries, and the adversarial invariant suite ADR-003
//! requires.
//!
//! These live outside the crate rather than in `#[cfg(test)]` modules because they test the
//! *public* surface exactly as a transport would use it. A test that could reach into private
//! helpers would happily pass while the API a caller sees was broken.

mod common;

use common::SplitMix64;

use ncui_core::{CorrelationId, ErrorKind, Limits, NcError, RequestId};
use ncui_ipc::{
    FRAME_HEADER_BYTES, FrameDecoder, FrameFlags, FrameHeader, FrameKind, Message, PAYLOAD_VERSION,
    decode_messages, encode_messages,
};

/// A frame limit large enough for the generated messages but small enough to trip on a
/// hostile length field.
const LIMIT: u32 = 64 * 1024;

/// One representative message of each variant, so a property test covers all four.
fn sample_messages() -> Vec<Message> {
    let correlation = CorrelationId::new();
    let request_id = RequestId::new();
    vec![
        Message::Request {
            method: "window.create".to_owned(),
            params: vec![1, 2, 3],
            correlation,
            request_id,
            deadline_ms: 5_000,
        },
        Message::Response {
            request_id,
            correlation,
            result: b"{\"ok\":true}".to_vec(),
        },
        Message::Event {
            topic: "window.closed".to_owned(),
            payload: Vec::new(),
            correlation,
        },
        Message::ErrorResponse {
            request_id,
            correlation,
            error: NcError::new(ErrorKind::LimitExceeded {
                resource: "ipc.request.in_flight".to_owned(),
                limit: 4096,
                observed: 4097,
            })
            .operation("ipc::request::register")
            .correlated(correlation),
        },
    ]
}

/// Builds a random but structurally valid message.
fn random_message(rng: &mut SplitMix64) -> Message {
    // Identifiers must be non-zero: zero is the reserved null identifier and a valid message
    // never carries one.
    let correlation = CorrelationId::from_raw(u128::from(rng.next_u64()) | 1).expect("non-zero");
    let request_id = RequestId::from_raw(u128::from(rng.next_u64()) | 1).expect("non-zero");
    let blob = |rng: &mut SplitMix64| {
        let len = rng.in_range(0, 64) as usize;
        rng.bytes(len)
    };
    match rng.below(4) {
        0 => Message::Request {
            method: rng.name(),
            params: blob(rng),
            correlation,
            request_id,
            deadline_ms: rng.next_u64(),
        },
        1 => Message::Response {
            request_id,
            correlation,
            result: blob(rng),
        },
        2 => Message::Event {
            topic: rng.name(),
            payload: blob(rng),
            correlation,
        },
        _ => Message::ErrorResponse {
            request_id,
            correlation,
            error: random_error(rng, correlation),
        },
    }
}

/// Builds a random error covering the kinds a peer can realistically report.
fn random_error(rng: &mut SplitMix64, correlation: CorrelationId) -> NcError {
    let kind = match rng.below(6) {
        0 => ErrorKind::Cancelled,
        1 => ErrorKind::Timeout {
            elapsed_ms: rng.next_u64(),
        },
        2 => ErrorKind::Protocol { reason: rng.name() },
        3 => ErrorKind::LimitExceeded {
            resource: rng.name(),
            limit: rng.next_u64(),
            observed: rng.next_u64(),
        },
        4 => ErrorKind::Io {
            operation: rng.name(),
            io_kind: std::io::ErrorKind::PermissionDenied,
            message: rng.name(),
        },
        _ => ErrorKind::NotFound {
            kind: rng.name(),
            id: rng.name(),
        },
    };
    NcError::new(kind)
        .operation(rng.name())
        .correlated(correlation)
}

#[test]
fn every_variant_round_trips_through_the_binary_wire_format() {
    for message in sample_messages() {
        let bytes = message.encode(LIMIT).expect("encode");
        let decoded = decode_messages(&bytes, LIMIT).expect("decode");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0], message);
    }
}

#[test]
fn every_variant_round_trips_through_the_json_control_plane() {
    for message in sample_messages() {
        let json = message.to_json_string().expect("to json");
        let decoded = Message::from_json_str(&json).expect("from json");
        assert_eq!(decoded, message);
    }
}

#[test]
fn the_two_representations_agree_on_every_variant() {
    // ADR-003: the JSON control plane and the binary hot path must not drift. Round-tripping
    // each independently would not catch a message that encodes one way and reads back the
    // same way but means something else; comparing the JSON of a decoded binary message to the
    // JSON of the original does.
    for message in sample_messages() {
        let bytes = message.encode(LIMIT).expect("encode");
        let from_binary = decode_messages(&bytes, LIMIT).expect("decode")[0].clone();
        let original_json = message.to_json_value().expect("to json");
        let binary_json = from_binary.to_json_value().expect("to json");
        assert_eq!(original_json, binary_json);
    }
}

#[test]
fn the_wire_layout_matches_the_documented_specification() {
    let correlation =
        CorrelationId::from_raw(0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10).expect("non-zero");
    let request_id =
        RequestId::from_raw(0x1112_1314_1516_1718_191A_1B1C_1D1E_1F20).expect("non-zero");
    let message = Message::Request {
        method: "ab".to_owned(),
        params: vec![9, 9],
        correlation,
        request_id,
        deadline_ms: 0x0102_0304_0506_0708,
    };
    let bytes = message.encode(LIMIT).expect("encode");

    assert_eq!(&bytes[0..4], b"NCUI", "magic");
    assert_eq!(bytes[4], 1, "frame version");
    assert_eq!(bytes[5], 0x01, "frame kind = request");
    assert_eq!(&bytes[6..8], &[0, 0], "flags");
    let expected_len = u32::try_from(bytes.len() - FRAME_HEADER_BYTES).expect("length fits a u32");
    assert_eq!(&bytes[8..12], &expected_len.to_be_bytes(), "payload length");

    let payload = &bytes[FRAME_HEADER_BYTES..];
    assert_eq!(payload[0], PAYLOAD_VERSION, "payload version");
    assert_eq!(payload[1], 0x01, "discriminator = request");
    assert_eq!(
        &payload[2..18],
        &correlation.as_raw().to_be_bytes(),
        "correlation is a big-endian u128"
    );
    assert_eq!(
        &payload[18..34],
        &request_id.as_raw().to_be_bytes(),
        "request id is a big-endian u128"
    );
    assert_eq!(
        &payload[34..42],
        &0x0102_0304_0506_0708u64.to_be_bytes(),
        "deadline is a big-endian u64"
    );
    assert_eq!(
        &payload[42..46],
        &2u32.to_be_bytes(),
        "method length prefix"
    );
    assert_eq!(&payload[46..48], b"ab", "method bytes");
    assert_eq!(
        &payload[48..52],
        &2u32.to_be_bytes(),
        "params length prefix"
    );
    assert_eq!(&payload[52..], &[9, 9], "params bytes");
}

#[test]
fn several_messages_batch_into_one_frame_and_decode_back_identically() {
    let messages = sample_messages();
    let bytes = encode_messages(&messages, LIMIT).expect("encode batch");
    // One outer batch frame, not four frames: batching is the point.
    let mut decoder = FrameDecoder::new(LIMIT).expect("decoder");
    decoder.feed(&bytes).expect("feed");
    let outer = decoder.next_frame().expect("frame").expect("a frame");
    assert!(outer.header().is_batch());
    decoder.check_complete().expect("no trailing bytes");

    let decoded = decode_messages(&bytes, LIMIT).expect("decode");
    assert_eq!(decoded, messages);
}

#[test]
fn a_single_message_is_not_wrapped_in_a_batch() {
    let messages = sample_messages();
    for message in &messages {
        let bytes = encode_messages(std::slice::from_ref(message), LIMIT).expect("encode");
        let mut decoder = FrameDecoder::new(LIMIT).expect("decoder");
        decoder.feed(&bytes).expect("feed");
        let frame = decoder.next_frame().expect("frame").expect("a frame");
        assert!(!frame.header().is_batch());
        assert_eq!(frame.header().kind, message.frame_kind());
    }
}

#[test]
fn an_empty_batch_encodes_to_nothing() {
    assert!(
        encode_messages(&[], LIMIT).expect("encode").is_empty(),
        "there is nothing to frame, and an empty batch would be a special case for every peer"
    );
    assert!(decode_messages(&[], LIMIT).expect("decode").is_empty());
}

#[test]
fn a_stream_survives_delivery_in_any_chunking() {
    // The transport does not choose how bytes arrive. Feeding one byte at a time, all at
    // once, and at every intermediate split must all produce the same messages: a decoder
    // that only works on aligned reads is not a streaming decoder.
    let messages = sample_messages();
    let bytes = encode_messages(&messages, LIMIT).expect("encode");

    for chunk_size in 1..=bytes.len() {
        let mut decoder = FrameDecoder::new(LIMIT).expect("decoder");
        let mut decoded = Vec::new();
        for chunk in bytes.chunks(chunk_size) {
            decoder.feed(chunk).expect("feed a chunk");
            while let Some(frame) = decoder.next_frame().expect("next frame") {
                decode_frame(&frame, &mut decoded);
            }
        }
        decoder.check_complete().expect("stream ends on a boundary");
        assert_eq!(
            decoded, messages,
            "chunk size {chunk_size} produced a different result"
        );
    }
}

/// Decodes the messages a frame carries, unwrapping a batch, appending to `out`.
///
/// Deliberately the crate's own `decode_frame_messages` rather than a hand-rolled version: a
/// test helper that re-implemented the unwrapping would pass even if the library's version were
/// broken.
fn decode_frame(frame: &ncui_ipc::Frame, out: &mut Vec<Message>) {
    ncui_ipc::message::decode_frame_messages(frame, LIMIT, out).expect("decode a frame");
}

#[test]
fn random_valid_messages_round_trip_through_both_representations() {
    // The property ADR-003 asks for: `decode(encode(message)) == message`, over messages the
    // generator produces rather than ones a human wrote.
    let mut rng = SplitMix64::new(0x5EED_0001);
    for iteration in 0..2_000u64 {
        let message = random_message(&mut rng);
        let bytes = message
            .encode(LIMIT)
            .unwrap_or_else(|e| panic!("iteration {iteration}: encode failed: {e}"));
        let decoded = decode_messages(&bytes, LIMIT)
            .unwrap_or_else(|e| panic!("iteration {iteration}: decode failed: {e}"));
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0], message, "iteration {iteration}");

        let json = message
            .to_json_string()
            .unwrap_or_else(|e| panic!("iteration {iteration}: to_json failed: {e}"));
        let from_json = Message::from_json_str(&json)
            .unwrap_or_else(|e| panic!("iteration {iteration}: from_json failed: {e}"));
        assert_eq!(from_json, message, "iteration {iteration}");
    }
}

#[test]
fn random_batches_round_trip() {
    let mut rng = SplitMix64::new(0x5EED_0002);
    for iteration in 0..200u64 {
        let messages: Vec<Message> = (0..rng.in_range(1, 8))
            .map(|_| random_message(&mut rng))
            .collect();
        let bytes = encode_messages(&messages, LIMIT)
            .unwrap_or_else(|e| panic!("iteration {iteration}: encode failed: {e}"));
        let decoded = decode_messages(&bytes, LIMIT)
            .unwrap_or_else(|e| panic!("iteration {iteration}: decode failed: {e}"));
        assert_eq!(decoded, messages, "iteration {iteration}");
    }
}

#[test]
fn a_hostile_length_field_costs_nothing_and_is_always_rejected() {
    // The single most important property of the decoder (ADR-003). Four bytes of payload must
    // not be able to induce a large allocation, so the bound is asserted directly on the
    // decoder's buffer rather than only inferred from the error it returns.
    let mut rng = SplitMix64::new(0x5EED_0003);
    for _ in 0..1_000 {
        let mut bytes = vec![b'N', b'C', b'U', b'I', 1u8, 0x01, 0, 0];
        bytes.extend_from_slice(&u32::MAX.to_be_bytes());
        // A few payload bytes follow, so the header is not obviously the only thing present.
        bytes.extend_from_slice(&rng.bytes(16));

        let mut decoder = FrameDecoder::new(LIMIT).expect("decoder");
        let err = decoder
            .feed(&bytes)
            .expect_err("a hostile length field must be rejected");
        assert!(
            matches!(err.kind(), ErrorKind::LimitExceeded { .. }),
            "got: {err}"
        );

        // The decoder must not have reserved anything on the peer's say-so. The header was
        // read to find the length and then rejected; that is the whole cost.
        assert!(
            decoder.buffered_len() <= FRAME_HEADER_BYTES,
            "a rejected length must not have been buffered: {} byte(s) held",
            decoder.buffered_len()
        );
        assert!(
            decoder.next_frame().is_err(),
            "a desynchronised stream stays stopped"
        );
    }
}

#[test]
fn a_peer_that_stops_mid_frame_cannot_grow_the_decoder_without_bound() {
    // A hostile peer that announces a legal but large frame and then stalls must not be able to
    // make the decoder grow beyond one frame. The transport's read timeout is what ends this;
    // the decoder's job is only to stay bounded.
    let announced = 32 * 1024usize; // well under the 64 KiB limit, so the header is accepted
    let mut header = vec![b'N', b'C', b'U', b'I', 1u8, 0x01, 0, 0];
    header.extend_from_slice(&u32::try_from(announced).expect("fits").to_be_bytes());

    // Deliver one byte short of the promised payload, in small dribbles, so the frame never
    // completes and the decoder is always holding a partial frame.
    let dribble = 64usize;
    let mut delivered = 0usize;
    let mut decoder = FrameDecoder::new(LIMIT).expect("decoder");
    decoder
        .feed(&header)
        .expect("a bare header is not an error");
    assert!(decoder.next_frame().expect("partial frame").is_none());

    while delivered + dribble < announced {
        decoder
            .feed(&vec![0u8; dribble])
            .expect("a partial frame is not an error");
        delivered += dribble;
        assert!(
            decoder.buffered_len() <= FRAME_HEADER_BYTES + announced,
            "the buffer must never exceed one frame"
        );
        assert!(
            decoder.next_frame().expect("partial frame").is_none(),
            "an incomplete frame must not be reported"
        );
    }

    // Ten thousand dribbled writes later the decoder is still bounded, and still waiting.
    assert_eq!(decoder.buffered_len(), FRAME_HEADER_BYTES + delivered);
    assert!(decoder.buffered_len() < FRAME_HEADER_BYTES + announced);
    let err = decoder
        .check_complete()
        .expect_err("a stream that ends mid-frame is a truncated message");
    assert!(err.to_string().contains("mid-frame"), "got: {err}");
}

#[test]
fn arbitrary_adversarial_bytes_never_panic_and_always_return_err() {
    // Random noise has no reason to be a valid message, so decoding must always fail — and
    // must fail *safely*. A panic here would be the defect the whole test exists to catch.
    let mut rng = SplitMix64::new(0x5EED_0004);
    for iteration in 0..20_000u64 {
        let len = rng.in_range(0, 96) as usize;
        let mut bytes = rng.bytes(len);

        // Half the time, prefix the stream with a real magic so the noise gets past the first
        // check and exercises the deeper length handling rather than always failing at byte 0.
        if rng.boolean() && bytes.len() >= 4 {
            bytes[..4].copy_from_slice(b"NCUI");
        }

        let outcome = decode_messages(&bytes, LIMIT);
        if len == 0 {
            assert!(outcome.is_ok(), "an empty stream is not malformed");
            continue;
        }
        assert!(
            outcome.is_err(),
            "iteration {iteration}: random noise decoded successfully as {outcome:?}"
        );
    }
}

#[test]
fn mutated_valid_streams_never_panic_and_are_overwhelmingly_rejected() {
    // Random noise almost never trips the deep length checks. Flipping single bytes in a valid
    // stream is what actually reaches them: a corrupted length prefix, a corrupted
    // discriminator, a corrupted version.
    let mut rng = SplitMix64::new(0x5EED_0005);
    let messages = sample_messages();
    let good = encode_messages(&messages, LIMIT).expect("encode");

    let mut rejected = 0usize;
    let mut total = 0usize;
    for _ in 0..20_000 {
        let mut bytes = good.clone();
        // Between one and three byte flips.
        for _ in 0..rng.in_range(1, 4) {
            let index = rng.below(bytes.len() as u64) as usize;
            bytes[index] = rng.byte();
        }
        total += 1;
        // A mutation can land somewhere harmless (the magic is four bytes, an empty blob has
        // all of its bytes spare) or on a payload byte, which legitimately changes the
        // message. What must always hold for a *successful* decode is canonicality: the
        // decoded messages re-encode to something that decodes back to exactly themselves.
        // That is what proves nothing was silently lost or normalised on the way through.
        match decode_messages(&bytes, LIMIT) {
            Ok(decoded) => {
                let re_encoded = encode_messages(&decoded, LIMIT).expect("re-encode");
                let again = decode_messages(&re_encoded, LIMIT).expect("re-decode");
                assert_eq!(
                    again, decoded,
                    "a decoded message must re-encode to the same message"
                );
            }
            Err(_) => rejected += 1,
        }
    }
    assert!(
        rejected > total / 2,
        "mutations should mostly be rejected; only {rejected} of {total} were"
    );
}

#[test]
fn truncation_at_every_offset_is_rejected_or_decodes_a_prefix() {
    let messages = sample_messages();
    let bytes = encode_messages(&messages, LIMIT).expect("encode");
    for cut in 1..bytes.len() {
        let truncated = &bytes[..cut];
        let outcome = decode_messages(truncated, LIMIT);
        if let Ok(decoded) = outcome {
            // A truncation can land on a frame boundary, which is a legitimate prefix.
            assert!(
                decoded.len() < messages.len(),
                "a truncated stream decoded to the full message list"
            );
            assert_eq!(&decoded, &messages[..decoded.len()]);
        }
    }
}

#[test]
fn a_valid_stream_delivered_one_byte_at_a_time_still_decodes() {
    let messages = sample_messages();
    let bytes = encode_messages(&messages, LIMIT).expect("encode");
    let mut decoder = FrameDecoder::new(LIMIT).expect("decoder");
    let mut count = 0usize;
    for byte in &bytes {
        decoder.feed(std::slice::from_ref(byte)).expect("feed");
        while decoder.next_frame().expect("frame").is_some() {
            count += 1;
        }
    }
    decoder.check_complete().expect("complete stream");
    assert_eq!(count, 1, "one outer batch frame");
}

#[test]
fn decoding_with_limits_from_configuration_agrees_with_the_explicit_limit() {
    let limits = Limits {
        max_ipc_frame_bytes: LIMIT,
        ..Limits::default()
    };
    let messages = sample_messages();
    let bytes = encode_messages(&messages, limits.max_ipc_frame_bytes).expect("encode");
    let decoded = ncui_ipc::message::decode_messages_with_limits(&bytes, &limits).expect("decode");
    assert_eq!(decoded, messages);
}

#[test]
fn a_message_larger_than_the_frame_limit_is_refused_by_the_encoder() {
    let limits = Limits {
        max_ipc_frame_bytes: 256,
        ..Limits::default()
    };
    let big = Message::Request {
        method: "window.create".to_owned(),
        params: vec![0u8; 4096],
        correlation: CorrelationId::new(),
        request_id: RequestId::new(),
        deadline_ms: 0,
    };
    let err = big
        .encode(limits.max_ipc_frame_bytes)
        .expect_err("must be refused");
    assert!(matches!(err.kind(), ErrorKind::LimitExceeded { .. }));
}

#[test]
fn a_null_identifier_on_the_wire_is_a_protocol_violation() {
    // Zero is reserved, so a payload carrying it cannot become a valid message.
    let mut bytes = Vec::new();
    let payload = vec![PAYLOAD_VERSION, 0x01];
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&0u128.to_be_bytes()); // correlation
    bytes.extend_from_slice(&1u128.to_be_bytes()); // request id
    bytes.extend_from_slice(&0u64.to_be_bytes()); // deadline
    bytes.extend_from_slice(&0u32.to_be_bytes()); // method length
    bytes.extend_from_slice(&0u32.to_be_bytes()); // params length

    let header = FrameHeader::new(
        FrameKind::Request,
        FrameFlags::NONE,
        u32::try_from(bytes.len()).expect("length fits"),
    );
    let framed = ncui_ipc::frame::encode_frame(&header, &bytes);
    let err = decode_messages(&framed, LIMIT).expect_err("a null id must be rejected");
    assert!(
        matches!(err.kind(), ErrorKind::InvalidId { .. }),
        "got: {err}"
    );
}

#[test]
fn a_payload_whose_discriminator_disagrees_with_its_frame_is_rejected() {
    let message = sample_messages().remove(0);
    let payload = message.encode_payload().expect("payload");
    // Claim the frame is an event while the payload says request.
    let header = FrameHeader::new(
        FrameKind::Event,
        FrameFlags::NONE,
        u32::try_from(payload.len()).expect("length fits"),
    );
    let framed = ncui_ipc::frame::encode_frame(&header, &payload);
    let err = decode_messages(&framed, LIMIT).expect_err("the disagreement must be caught");
    assert!(
        matches!(err.kind(), ErrorKind::Protocol { .. }),
        "got: {err}"
    );
}

#[test]
fn a_payload_with_trailing_bytes_is_rejected() {
    let message = sample_messages().remove(0);
    let mut payload = message.encode_payload().expect("payload");
    payload.extend_from_slice(b"trailing");
    let header = FrameHeader::new(
        FrameKind::Request,
        FrameFlags::NONE,
        u32::try_from(payload.len()).expect("length fits"),
    );
    let framed = ncui_ipc::frame::encode_frame(&header, &payload);
    let err = decode_messages(&framed, LIMIT).expect_err("trailing bytes must be rejected");
    assert!(matches!(err.kind(), ErrorKind::Codec { .. }), "got: {err}");
}
