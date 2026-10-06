//! Re-export of `libfuzzer-sys`'s `fuzz_target!` macro under a stable, replaceable path.
// ncui-allow-placeholder: prose describing why the re-export exists, not a stand-in
//!
//! A fuzz target's *body* — the invariants it asserts — is ordinary Rust and worth testing
//! even on a machine with no sanitizer runtime. Naming the macro through this crate lets the
//! compile-check test substitute its own without the targets knowing, so one source of truth
// ncui-allow-placeholder: prose describing why the re-export exists, not a stand-in
//! serves both `cargo fuzz run` and the ordinary suite.

pub use libfuzzer_sys::fuzz_target;
