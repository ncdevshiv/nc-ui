//! Fuzzes the binary frame decoder.
//!
//! The decoder's contract with an untrusted peer is that arbitrary bytes produce a `Result`
//! and never a panic, a hang, or an allocation driven by a declared length rather than a
//! received one. Generated input replaces the fixed corpus of malformed frames.

#![no_main]

use ncui_fuzz_target::fuzz_target;

/// The frame limit a configured runtime would apply.
const MAX_FRAME_BYTES: u32 = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    // Both entry points, because they are different code: `decode_all` walks a stream and
    // latches desynchronisation, `decode_batch` reads a counted array.
    let _ = ncui_ipc::frame::decode_all(data, MAX_FRAME_BYTES);
    let _ = ncui_ipc::frame::decode_batch(data, MAX_FRAME_BYTES);
});
