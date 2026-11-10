//! The boundary a component must be built against.
//!
//! This is the wire format in its executable half: the exports a module must provide, the
//! imports it may provide, how a JSON document crosses the boundary in each direction, and
//! what the integers mean when a call does not succeed.
//!
//! # Why the convention is this shape
//!
//! A WebAssembly module has one address space, the guest's, and the host cannot allocate in it
//! without the guest's cooperation: calling the guest's own allocator from a host function is
//! not possible without re-entering the instance, which the engine does not offer. So the
//! component publishes a region instead — `ncui_scratch` — and the host writes into space the
//! component chose. The alternative, a host-side copy for every argument, would mean the host
//! deciding where a component's data lives, which is exactly the kind of arrangement that
//! breaks the first time a component needs its input at a stable address.
//!
//! # The packed return value
//!
//! A function that returns a document returns one `i64` holding `(pointer << 32) | length`.
//! Two `i32` results would need multi-value support enabled in the engine; one `i64` needs
//! nothing, and it cannot be misread as a count of anything else.
//!
//! # Refusal codes
//!
//! `ncui::host_call` returns a byte count, which is non-negative, or one of the negative codes
//! below. They are negative because a length of zero is a legitimate response and a component
//! that has to guess whether `0` meant "empty" or "refused" will eventually guess wrong.

pub use ncui_abi::wire::HOST_MODULE;

/// The calling-convention revision a component must declare numerically.
///
/// This is the *transport*, not the manifest's ABI string: a component whose export says `2`
/// was built against a convention this host does not implement, whatever its manifest claims.
/// Both are checked, and they must agree.
pub const ABI_MAJOR: i32 = 1;

/// The exported linear memory every document is read from and written to.
pub const MEMORY_EXPORT: &str = "memory";

/// `() -> i32` — the convention revision the component was built against.
pub const ABI_VERSION_EXPORT: &str = "ncui_abi_version";

/// `() -> i64` — packed location of the manifest JSON, in UTF-8.
pub const MANIFEST_EXPORT: &str = "ncui_manifest";

/// `() -> i64` — packed location of the host-writable scratch region.
pub const SCRATCH_EXPORT: &str = "ncui_scratch";

/// `(name_ptr, name_len, input_ptr, input_len) -> i64` — invoke a method, return a JSON
/// document as a packed location inside the component's own memory.
pub const INVOKE_EXPORT: &str = "ncui_invoke";

/// `(name_ptr, name_len) -> i32` — may this component use that capability right now?
pub const CAPABILITY_CHECK_IMPORT: &str = "capability_check";

/// `(name_ptr, name_len, input_ptr, input_len, output_ptr, output_len) -> i32` — ask the host
/// to do something, and read its response out of `output`.
pub const HOST_CALL_IMPORT: &str = "host_call";

/// The alignment the input document starts at inside the scratch region.
///
/// Eight bytes, so a component may load 64-bit values from the payload without trapping on a
/// misaligned access. The name length is not a multiple of eight, so the host pads.
pub const SCRATCH_ALIGNMENT: u32 = 8;

/// The response does not fit in `output_len`. Nothing was written.
pub const REFUSAL_TOO_SMALL: i32 = -1;
/// The component did not declare that service in its manifest, or the negotiation did not
/// grant the capability it needs. Nothing was written.
pub const REFUSAL_UNDECLARED: i32 = -2;
/// The host was asked to do the work and refused or failed. Nothing was written.
pub const REFUSAL_HOST_ERROR: i32 = -3;
/// The request bytes were not valid UTF-8 JSON. Nothing was written.
pub const REFUSAL_BAD_INPUT: i32 = -4;

/// Packs a guest pointer and length into the single `i64` the convention returns.
#[must_use]
pub fn pack(pointer: u32, length: u32) -> u64 {
    // `u64::from` is not a const fn on this toolchain, and nothing here needs it to be.
    (u64::from(pointer) << 32) | u64::from(length)
}

/// Reads a packed guest pointer and length back out of a value the component returned.
#[must_use]
pub const fn unpack(packed: i64) -> (u32, u32) {
    let bits = packed as u64;
    ((bits >> 32) as u32, (bits & 0xFFFF_FFFF) as u32)
}

/// Where the input document starts inside a scratch region, given the method name's length.
///
/// The name goes at the start of the region and the payload follows, padded up to
/// [`SCRATCH_ALIGNMENT`]. The length must already have been bounded by the host's own name
/// limit: at `u32::MAX` this saturates and rounds down, which would place the payload before
/// the end of the name, and only a caller that skipped its bound can get there.
#[must_use]
pub const fn input_offset(name_length: u32) -> u32 {
    name_length.saturating_add(SCRATCH_ALIGNMENT - 1) & !(SCRATCH_ALIGNMENT - 1)
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_packed_location_survives_a_round_trip() {
        for (pointer, length) in [
            (0, 0),
            (1, 1),
            (64, 73),
            (u32::MAX, u32::MAX),
            (0x8000_0000, 0x0000_0001),
        ] {
            let packed = i64::from_ne_bytes(pack(pointer, length).to_ne_bytes());
            assert_eq!(unpack(packed), (pointer, length), "{pointer}+{length}");
        }
    }

    #[test]
    fn the_low_half_carries_the_length_and_the_high_half_the_pointer() {
        // A component author reading this convention needs the halves named, not guessed.
        let packed = pack(0x1122_3344, 0x5566_7788);
        assert_eq!(packed >> 32, 0x1122_3344);
        assert_eq!(packed & 0xFFFF_FFFF, 0x5566_7788);
    }

    #[test]
    fn the_input_payload_starts_aligned_past_the_name() {
        assert_eq!(input_offset(0), 0);
        assert_eq!(input_offset(1), 8);
        assert_eq!(input_offset(8), 8);
        assert_eq!(input_offset(9), 16);
        assert_eq!(input_offset(128), 128);
        // Saturating, never wrapping: a wrapped offset would place the payload before the
        // name, which is the failure a caller that skipped its length bound would hit.
        assert_eq!(input_offset(u32::MAX), u32::MAX - SCRATCH_ALIGNMENT + 1);
        for length in 0..=256 {
            let offset = input_offset(length);
            assert!(offset >= length, "{length} -> {offset}");
            assert_eq!(offset % SCRATCH_ALIGNMENT, 0, "{length} -> {offset}");
        }
    }

    #[test]
    fn the_refusal_codes_are_negative_and_distinct() {
        // A non-negative refusal would be indistinguishable from a byte count.
        let codes = [
            REFUSAL_TOO_SMALL,
            REFUSAL_UNDECLARED,
            REFUSAL_HOST_ERROR,
            REFUSAL_BAD_INPUT,
        ];
        assert!(codes.iter().all(|code| *code < 0), "{codes:?}");
        for (index, code) in codes.iter().enumerate() {
            assert!(
                codes[..index].iter().all(|other| other != code),
                "code {code} appears twice"
            );
        }
    }

    #[test]
    fn the_host_module_name_is_the_one_the_abi_declares() {
        // The linker keys imports by (module, name). A second spelling of the module is a
        // component that links against nothing and a host that refuses everything.
        assert_eq!(HOST_MODULE, "ncui");
    }
}
