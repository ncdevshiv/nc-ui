//! Fuzzes the message decoder for every frame kind.
//!
//! Seeding a third of the inputs with a valid header matters: pure random bytes are rejected
//! at the first byte and the post-header paths are never reached.

#![no_main]

use ncui_fuzz_target::fuzz_target;
use ncui_ipc::frame::FrameKind;

const MAGIC: &[u8; 4] = b"NCUI";

fuzz_target!(|data: &[u8]| {
    let kinds = [
        FrameKind::Request,
        FrameKind::Response,
        FrameKind::Event,
        FrameKind::ErrorResponse,
        FrameKind::Batch,
    ];
    let index = data.first().map_or(0, |b| usize::from(*b) % kinds.len());
    let kind = kinds[index];

    let mut payload = Vec::with_capacity(data.len() + 8);
    // `data[0]` selects the kind, so every kind is exercised across the corpus.
    // `data[1..]` would panic on an empty input, which is the most ordinary input a fuzzer
    // starts from — a target that dies on its own corpus never fuzzes anything.
    payload.extend_from_slice(data.get(1..).unwrap_or(&[]));
    if data.len().is_multiple_of(3) {
        payload.extend_from_slice(MAGIC);
        payload.push(1);
    }
    let _ = ncui_ipc::message::Message::decode_payload(kind, &payload);
    let _ = ncui_ipc::message::decode_messages(data, 64 * 1024);
});
