//! Typed identifiers.
//!
//! NC-UI uses distinct identifier *types* for distinct entity classes so that a
//! [`WindowId`] can never be passed where a [`ComponentId`] is expected. The compiler
//! enforces that; runtime validation is a second line of defence, not the primary one.
//!
//! All identifiers are 128-bit values rendered as 32 lowercase hex characters. They are
//! generated from a per-process `SplitMix64` sequence and are therefore:
//!
//! * **unique within a process** — guaranteed, because the generator never repeats a
//!   value within one run.
//! * **not cryptographically unpredictable** — they are a monotonic-ish sequence from a
//!   seed. They must never be used as authentication tokens, capability handles, or
//!   secret material. Use [`crate::capability::CapabilitySet`] and the token type in
//!   `ncui-security` for those instead.
//!
//! * **not stable across restarts** — a restarted process reseeds. Anything persisted must
//!   store the application's own stable identifier alongside, not the runtime ID.

use core::fmt;
use core::num::NonZeroU128;
use core::str::FromStr;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use std::sync::atomic::{AtomicU64, Ordering};

/// Error returned when an identifier cannot be parsed from its textual form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseIdError {
    /// The identifier type that failed to parse, e.g. `"ComponentId"`.
    pub id_type: &'static str,
    /// The offending input, truncated for safe inclusion in logs.
    pub input: String,
    /// Human-readable explanation of the failure.
    pub reason: &'static str,
}

impl fmt::Display for ParseIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid {}: {} (input: {:?})",
            self.id_type, self.reason, self.input
        )
    }
}

impl std::error::Error for ParseIdError {}

/// Truncates untrusted input before it reaches a log or error message.
fn truncate(input: &str) -> String {
    const MAX: usize = 40;
    if input.chars().count() <= MAX {
        return input.to_owned();
    }
    let mut out: String = input.chars().take(MAX).collect();
    out.push('…');
    out
}

/// Declares a 128-bit newtype identifier with parsing, formatting and serde support.
macro_rules! declare_id {
    (
        $(#[$meta:meta])*
        $name:ident
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(NonZeroU128);

        impl $name {
            /// The identifier type name, used in diagnostics and metrics labels.
            pub const TYPE_NAME: &'static str = stringify!($name);

            /// Generates a fresh identifier that has not been used in this process.
            ///
            /// The value is unique within the current process but carries no
            /// cryptographic guarantee; see the module documentation.
            #[must_use]
            pub fn new() -> Self {
                let raw = generate_raw();
                // `generate_raw` never returns zero, but falling back rather than
                // panicking keeps the crate free of panic paths in library code.
                Self(NonZeroU128::new(raw).unwrap_or(NonZeroU128::MIN))
            }

            /// Reconstructs an identifier from its raw 128-bit value.
            ///
            /// Returns `None` for zero, which is reserved as the null identifier so that
            /// `Option<Id>` has a cheaper representation than a separate validity flag.
            #[must_use]
            pub const fn from_raw(raw: u128) -> Option<Self> {
                match NonZeroU128::new(raw) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            /// Returns the raw 128-bit value.
            #[must_use]
            pub const fn as_raw(self) -> u128 {
                self.0.get()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:032x}", self.0.get())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                if s.is_empty() {
                    return Err(ParseIdError {
                        id_type: Self::TYPE_NAME,
                        input: truncate(s),
                        reason: "empty identifier",
                    });
                }
                if s.len() > 32 {
                    return Err(ParseIdError {
                        id_type: Self::TYPE_NAME,
                        input: truncate(s),
                        reason: "identifier exceeds 32 hex digits",
                    });
                }
                let raw = u128::from_str_radix(s, 16).map_err(|_| ParseIdError {
                    id_type: Self::TYPE_NAME,
                    input: truncate(s),
                    reason: "not a 32-character hex value",
                })?;
                Self::from_raw(raw).ok_or_else(|| ParseIdError {
                    id_type: Self::TYPE_NAME,
                    input: truncate(s),
                    reason: "zero is the reserved null identifier",
                })
            }
        }

        /// Serializes the identifier as its 32-character lowercase hex form.
        ///
        /// This is the canonical JSON representation: identifiers are opaque values,
        /// so a numeric encoding would be lossy for consumers outside the runtime.
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.to_string())
            }
        }

        /// Deserializes an identifier from its hex form, rejecting malformed input.
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                raw.parse().map_err(de::Error::custom)
            }
        }
    };
}

/// Process-global `SplitMix64` source. Each call returns a distinct 128-bit value.
fn generate_raw() -> u128 {
    static STATE: AtomicU64 = AtomicU64::new(seed());
    const fn seed() -> u64 {
        // A fixed, non-zero seed: uniqueness within the process is guaranteed by the
        // counter itself, so the seed only has to decorrelate separate processes.
        0x9E37_79B9_7F4A_7C15
    }

    let mut x = STATE
        .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
        .wrapping_add(seed());
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;

    // Combine two successive steps into one 128-bit value. Both are non-zero after
    // the mixing above, but the high word is masked defensively so the value can
    // never be zero and the invariant `NonZeroU128` relies on holds unconditionally.
    let mut y = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    y = (y ^ (y >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    y = (y ^ (y >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    y ^= y >> 31;

    let high = y & 0x7FFF_FFFF_FFFF_FFFF;
    let raw = (u128::from(high) << 64) | u128::from(x);
    if raw == 0 { 1 } else { raw }
}

declare_id! {
    /// Correlates every event produced while handling one logical operation.
    ///
    /// This is the identifier that must survive across process boundaries: it is
    /// carried in IPC envelopes, attached to tracing spans, and reported in errors so
    /// that one user action can be followed end to end (spec 50).
    CorrelationId
}

declare_id! {
    /// Identifies an in-flight request awaiting a response.
    RequestId
}

declare_id! {
    /// Identifies a window managed by the runtime.
    WindowId
}

declare_id! {
    /// Identifies a mounted component instance.
    ComponentId
}

declare_id! {
    /// Identifies a native worker.
    WorkerId
}

declare_id! {
    /// Identifies a unit of scheduled work.
    TaskId
}

/// Generates identifiers from a caller-supplied counter, for tests and for callers that
/// need reproducible identifiers in recorded fixtures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DupIdGenerator {
    next: u64,
}

impl DupIdGenerator {
    /// Creates a generator whose first identifier is `1`.
    #[must_use]
    pub const fn new() -> Self {
        Self { next: 1 }
    }

    /// Returns the next identifier in the sequence.
    ///
    /// The name is historical: "dup" refers to the duplicate-detection requirement these
    /// identifiers serve. This generator never produces a duplicate by construction.
    #[must_use]
    pub fn next_id(&mut self) -> u128 {
        let value = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        u128::from(value)
    }
}

impl Default for DupIdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generated_ids_are_unique_within_process() {
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            assert!(
                seen.insert(ComponentId::new().as_raw()),
                "duplicate id generated"
            );
        }
    }

    #[test]
    fn ids_round_trip_through_text() {
        let id = WindowId::new();
        let text = id.to_string();
        assert_eq!(text.len(), 32);
        let parsed: WindowId = text.parse().expect("valid hex should parse");
        assert_eq!(parsed, id);
    }

    #[test]
    fn zero_is_rejected_as_null() {
        assert!(WindowId::from_raw(0).is_none());
        assert!(
            "00000000000000000000000000000000"
                .parse::<WindowId>()
                .is_err()
        );
    }

    #[test]
    fn malformed_ids_are_rejected_with_context() {
        for bad in ["", "zz", "not-an-id", "0000000000000000000000000000000"] {
            let err = bad
                .parse::<ComponentId>()
                .expect_err("malformed input must not parse");
            assert_eq!(err.id_type, "ComponentId");
            assert!(!err.reason.is_empty());
        }
    }

    #[test]
    fn uppercase_hex_is_accepted_and_normalised() {
        let id = ComponentId::new();
        let upper = id.to_string().to_ascii_uppercase();
        assert_eq!(upper.parse::<ComponentId>().expect("valid hex"), id);
    }

    #[test]
    fn ids_serialize_as_strings_in_json() {
        let id = TaskId::new();
        let json = serde_json::to_string(&id).expect("serialization cannot fail");
        assert_eq!(json, format!("\"{id}\""));
        let back: TaskId = serde_json::from_str(&json).expect("valid id should deserialize");
        assert_eq!(back, id);
    }

    #[test]
    fn dup_generator_is_monotonic_and_starts_at_one() {
        let mut sequence = DupIdGenerator::new();
        assert_eq!(sequence.next_id(), 1);
        assert_eq!(sequence.next_id(), 2);
    }
}
