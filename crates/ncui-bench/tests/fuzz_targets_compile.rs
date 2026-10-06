//! Type-checks and exercises every fuzz target's body against the real crates.
//!
//! libFuzzer needs nightly *and* a sanitizer runtime, and a contributor without both would
//! otherwise ship a target that had never been compiled. This test compiles each target's
//! assertion body against the real API and runs it over generated input, so:
//!
//! * a target that no longer type-checks fails an ordinary CI run, and
//! * the invariants each target asserts are actually exercised, not merely written.
//!
//! `fuzz_target!` is stubbed below, because what is verified here is the code inside it
//! rather than the fuzzing engine around it. Running the engine is `cargo fuzz run`, which is
//! recorded separately as unverified on a machine with no x64 sanitizer runtime.
//!
//! This crate is the right home for it: it is the only one that already depends on every
//! crate a target names.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
// The targets carry `#![no_main]`, which only means something at a crate root, and each
// imports the stubbed macro from this crate. Included as modules, both look unused.
#![allow(
    unused_attributes,
    unused_imports,
    unused_macros,
    reason = "the targets are included as modules rather than built as binaries, and
              import the stubbed macro through an alias at this crate root"
)]
use proptest::prelude::*;

/// Stands in for `libfuzzer_sys::fuzz_target!`.
///
/// The real macro expands to a `#[no_mangle] extern "C"` function, which can only be defined
/// once per crate. The stand-in instead turns each body into an ordinary function, which is
/// what lets this test call it with generated input.
#[macro_export]
macro_rules! fuzz_target {
    (|$data:ident: &[u8]| $body:expr) => {
        /// The target body, exposed so this test can call it with generated input.
        ///
        /// The name is fixed because the real macro takes no name argument: it derives one
        /// from `LLVMFuzzerTestOneInput` and `#[export_name]`, neither of which can be called
        /// directly. One name per *module* is enough here, since each target is its own
        /// module, and the call sites read `<module>::fuzz_target_body`.
        pub fn fuzz_target_body(input: &[u8]) {
            let $data = input;
            $body
        }
    };
}

// The stubbed targets `use ncui_fuzz_target::fuzz_target;`. They are included as sibling
// modules with their own crate-relative resolution, so the alias is bound at the *crate*
// root via `extern crate` rather than as a sibling module — a plain `mod` would not be
// visible from inside an included file that resolves paths from its own root.
extern crate self as ncui_fuzz_target;

#[path = "../../../fuzz/fuzz_targets/fuzz_frame_decode.rs"]
mod frame_decode;
#[path = "../../../fuzz/fuzz_targets/fuzz_message_decode.rs"]
mod message_decode;
#[path = "../../../fuzz/fuzz_targets/fuzz_path_resolve.rs"]
mod path_resolve;
#[path = "../../../fuzz/fuzz_targets/fuzz_registry_parse.rs"]
mod registry_parse;
#[path = "../../../fuzz/fuzz_targets/fuzz_semver_parse.rs"]
mod semver_parse;
#[path = "../../../fuzz/fuzz_targets/fuzz_token_hex.rs"]
mod token_hex;

/// Every target body is checked in.
#[test]
fn every_fuzz_target_exists_and_is_callable() {
    // A body that no longer type-checks fails here at compile time; this test asserts the
    // whole set is present, so a deleted or renamed target is also caught.
    let _ = (
        frame_decode::fuzz_target_body as fn(&[u8]),
        message_decode::fuzz_target_body as fn(&[u8]),
        semver_parse::fuzz_target_body as fn(&[u8]),
        path_resolve::fuzz_target_body as fn(&[u8]),
        token_hex::fuzz_target_body as fn(&[u8]),
        registry_parse::fuzz_target_body as fn(&[u8]),
    );
}

/// Each target body is run in isolation over generated input.
///
/// Run one at a time so a failure names the target rather than the group.
#[test]
fn each_fuzz_target_body_survives_generated_input() {
    proptest!(|(data: Vec<u8>)| {
        frame_decode::fuzz_target_body(&data);
        message_decode::fuzz_target_body(&data);
        semver_parse::fuzz_target_body(&data);
        path_resolve::fuzz_target_body(&data);
        token_hex::fuzz_target_body(&data);
        registry_parse::fuzz_target_body(&data);
    });
}

/// Input shaped like what a hostile peer actually sends: a valid magic prefix and a plausible
/// header, so the post-header paths are reached rather than rejected at the first byte.
#[test]
fn each_fuzz_target_body_survives_prefixed_input() {
    proptest!(|(body: Vec<u8>, version: u8, kind: u8)| {
        let mut data = Vec::with_capacity(body.len() + 8);
        data.extend_from_slice(b"NCUI");
        data.push(version);
        data.push(kind);
        data.extend_from_slice(&body);

        frame_decode::fuzz_target_body(&data);
        message_decode::fuzz_target_body(&data);
        semver_parse::fuzz_target_body(&data);
        path_resolve::fuzz_target_body(&data);
        token_hex::fuzz_target_body(&data);
        registry_parse::fuzz_target_body(&data);
    });
}

/// Traversal-shaped input, which is what a containment escape or a frame desync needs.
#[test]
fn the_path_target_rejects_generated_traversal() {
    proptest!(|(depth: u8, filler: String, tail: String)| {
        let mut traversal = String::new();
        for _ in 0..(depth % 12) {
            traversal.push_str("../");
        }
        traversal.push_str(&filler);
        traversal.push('/');
        traversal.push_str(&tail);
        path_resolve::fuzz_target_body(traversal.as_bytes());
    });
}

/// A run of separators drives the frame decoders through their desynchronisation latch.
#[test]
fn the_frame_targets_survive_a_desynchronising_run() {
    proptest!(|(length: u8, filler: u8)| {
        let mut data = vec![filler; (length as usize) % 40 + 1];
        data.extend_from_slice(b"NCUI");
        frame_decode::fuzz_target_body(&data);
        message_decode::fuzz_target_body(&data);
    });
}
