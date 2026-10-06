// Each integration test binary compiles this module separately and exercises a different
// subset of it, so an unused item here means "unused by that binary", not dead code.
#![allow(
    dead_code,
    reason = "shared by several test binaries, each of which uses a different subset"
)]

//! A deterministic PRNG for the adversarial and round-trip tests.
//!
//! `SplitMix64` is implemented here rather than pulled in as a dependency for two reasons. ADR-012
//! makes dependency count a reviewable property, and a PRNG is about thirty lines. And a test
//! that fails must be reproducible: a fixed algorithm with an explicit seed means a failure
//! reported as "seed 0x5EED" replays exactly, with no version skew in some transitive crate.
//!
//! This is a general-purpose generator, not a cryptographic one. It is not used for anything
//! that needs unpredictability; identifiers come from `ncui-core`.

/// A deterministic `SplitMix64` source.
///
/// Cheap to clone-by-value and cheap to advance: every step is a handful of wrapping
/// multiplies. That matters because the adversarial test generates hundreds of thousands of
/// values and the generator must not be the slow part.
#[derive(Debug, Clone, Copy)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// The multiplier from the reference `SplitMix64`, used by `ncui-core`'s own id generator.
    const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

    /// Creates a generator from an explicit seed, so a failure can be replayed exactly.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Returns the next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(Self::GAMMA);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Returns the next value below `bound`.
    ///
    /// Uses rejection sampling rather than a modulo, because a modulo would bias the low
    /// values: `next % 3` produces 0 far more often than 1. Bias in an adversarial generator
    /// quietly removes exactly the byte patterns the test needs to generate.
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        // 2^64 mod bound is not needed here: rejecting the top partial zone is enough, and
        // the loop terminates almost always on the first draw for the bounds used below.
        let zone = u64::MAX - (u64::MAX % bound) - 1;
        loop {
            let value = self.next_u64();
            if value <= zone {
                return value % bound;
            }
        }
    }

    /// Returns a value in `low..=high`, or `low` if the range is empty.
    pub fn in_range(&mut self, low: u64, high: u64) -> u64 {
        if high <= low {
            return low;
        }
        low + self.below(high - low + 1)
    }

    /// Returns the next byte.
    pub fn byte(&mut self) -> u8 {
        (self.next_u64() >> 24) as u8
    }

    /// Returns the next boolean.
    pub fn boolean(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    /// Fills a buffer with random bytes.
    pub fn fill_bytes(&mut self, out: &mut [u8]) {
        for slot in out.iter_mut() {
            *slot = self.byte();
        }
    }

    /// Returns a vector of `len` random bytes.
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        self.fill_bytes(&mut out);
        out
    }

    /// Returns a random ASCII string suitable for a method or topic name.
    ///
    /// Bounded so the fuzzer cannot accidentally generate a string larger than the frame
    /// limit and have every case fail for the wrong reason.
    pub fn name(&mut self) -> String {
        let len = self.in_range(1, 24) as usize;
        let mut out = String::with_capacity(len);
        for _ in 0..len {
            out.push((b'a' + self.below(26) as u8) as char);
        }
        out
    }
}
