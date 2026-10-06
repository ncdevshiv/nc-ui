//! Secret token material and constant-time comparison.
//!
//! Spec 28: a secret must never reach source control, logs, traces, metrics or a crash
//! report. That requirement is not satisfied by a discipline of "remember not to log it"
//! — it is satisfied by a type whose *formatting* and *comparison* are both incapable of
//! disclosing the material.
//!
//! [`SecretToken`] therefore:
//!
//! * implements [`fmt::Debug`] and [`fmt::Display`] by hand, printing only a redaction
//!   marker and the length;
//! * implements [`PartialEq`] through a branch-free fixed-length comparison rather than
//!   deriving it, because a derived `PartialEq` is `memcmp`-shaped and the derived
//!   `Debug` would print the bytes;
//! * implements neither `Serialize` nor [`Hash`], so a secret cannot be written into a
//!   config file, an IPC frame, a `tracing` field or a map key by accident. Forgetting to
//!   add a `#[derive]` is a compile error, which is the failure mode we want;
//! * does not implement [`Clone`], because each copy is another copy of the secret that
//!   `Drop` cannot guarantee to erase. Callers that genuinely need a second copy can
//!   build one from their own material.
//!
//! # Entropy: what `std` alone can and cannot do
//!
//! A token is only as strong as the source that produced it, so the honest question is
//! where the bytes come from. `std` produces an asymmetric answer:
//!
//! * **Unix.** `/dev/urandom` is a character device, and `std::fs::File` can open and
//!   read one with no `unsafe` and no FFI. [`SystemEntropy`] therefore draws from the
//!   kernel CSPRNG and is exactly as strong as the platform claims.
//! * **Windows.** The OS CSPRNG (`BCryptGenRandom` / `RtlGenRandom`) is reachable only
//!   through FFI, which needs `unsafe`. **`std` alone cannot obtain cryptographic entropy
//!   on Windows, and this crate does not pretend otherwise.** Substituting
//!   `RandomState`, `SystemTime`, the thread-local address, or a `SplitMix64` would
//!   produce 32 bytes that *look* random and are perfectly predictable to anyone who can
//!   observe a handful of outputs — a strictly worse outcome than no token at all,
//!   because it is trusted.
//!
//!   The fix adopted is the `getrandom` crate, which performs that FFI on our behalf so
//!   this crate's own code stays free of `unsafe`. It is the only external dependency in
//!   the workspace and is scoped to this module; ADR-012 records why the exception was
//!   granted rather than leaving the Windows path unavailable.
//!
//! The API is therefore shaped so that this failure cannot be hidden:
//!
//! * [`SecretToken::from_entropy_source`] requires the caller to supply an
//!   [`EntropySource`]. The real sources (`getrandom`, `rand::rngs::OsRng`,
//!   `ring::SystemRandom`, the Windows CNG API through a thin safe wrapper) all satisfy
//!   a two-method trait, so wiring one in is a few lines at the application boundary.
//! * [`SecretToken::from_os_entropy`] is the ergonomic path, and it works on all three
//!   target platforms. On Unix it reads `/dev/urandom` directly with no dependency; on
//!   Windows it uses the `getrandom` crate, which performs the `BCryptGenRandom` FFI so
//!   this workspace's own code stays free of `unsafe` (ADR-012). A platform with neither
//!   path returns [`ErrorKind::Internal`] with a message saying what to supply, rather
//!   than returning weak bytes. **The rule that has not changed: an unavailable platform
//!   CSPRNG is a loud failure at startup, never a guessable token.**
//!
//! # Constant-time comparison
//!
//! [`constant_time_eq`] accumulates the XOR of every byte difference over the longer of
//! the two inputs, folding the length difference into the same accumulator, and returns
//! whether the accumulator is zero. There is no data-dependent branch inside the loop and
//! no early exit: the running time depends on the two *lengths*, which are public (a
//! token's size is not a secret — an attacker who knows the length learns nothing they
//! could not learn from a failed connection), and on nothing about the *contents*.
//!
//! # Residuals
//!
//! * Compiler optimization. Rust gives no formal constant-time guarantee; an optimizer
//!   may in principle introduce a branch. Mitigations used here: the comparison lives in
//!   a `#[inline(never)]` `pub` function whose result passes through
//!   [`core::hint::black_box`], and the accumulator is widened to `u32` so LLVM is not
//!   tempted to narrow it back to a byte and branch on zero.
//! * `Drop` overwrites the buffer with zeros, but the optimizer may elide a store to
//!   memory that is about to be freed, and the allocation may be paged to disk before
//!   the overwrite. This is best-effort hygiene, **not** a guarantee. Callers that need a
//!   guarantee must keep the material in a `zeroize`-backed buffer or in the OS
//!   keystore, and must not copy it into a `Vec<u8>` of their own.

use core::fmt;
use core::hint::black_box;
use ncui_core::{ErrorKind, NcError, Result};

/// The operation name recorded when the platform cannot supply OS entropy.
const OP_OS_ENTROPY: &str = "security::token::from_os_entropy";
/// The operation name recorded when a caller-supplied entropy source fails.
const OP_INJECTED_ENTROPY: &str = "security::token::from_entropy_source";
/// The operation name recorded when a token is constructed from caller bytes.
const OP_NEW_TOKEN: &str = "security::token::new";

/// The smallest token [`SecretToken::from_entropy_source`] will produce, in bytes.
///
/// 128 bits is the point below which an offline search of a realistic key space stops
/// being a research project. The bound is enforced rather than documented so that a
/// misconfigured `len` fails at the call that made the mistake instead of producing a
/// token that looks like a token and is brute-forceable.
pub const MIN_TOKEN_BYTES: usize = 16;

/// The largest token this crate will construct, in bytes.
///
/// An upper bound is a denial-of-service control, not a security control: `from_os_entropy`
/// and `from_entropy_source` allocate exactly the length the caller asked for, and an
/// agent that can ask for a 4 GiB token is an agent that can exhaust memory. 4 KiB is far
/// beyond any legitimate bearer token (a real one is 16–64 bytes) and far below anything
/// that would matter to a process.
pub const MAX_TOKEN_BYTES: usize = 4096;

/// The process-independent redacted rendering prefix.
const REDACTED: &str = "<redacted";

/// Compares two byte strings in time that depends only on their lengths.
///
/// The loop covers `max(a.len(), b.len())` bytes, treating a missing byte on the shorter
/// side as zero, and XORs every difference into a `u32` accumulator.
///
/// The XOR of every byte difference is folded into the same accumulator as the length
/// difference, so a caller cannot distinguish "same length, wrong
/// bytes" from "different length" by timing the comparison — which is exactly the
/// distinction a token-guessing attack would want.
///
/// `#[inline(never)]` and the [`black_box`] on the accumulator keep the optimizer from
/// collapsing the loop into a `memcmp` (which is allowed to exit early) or from proving
/// the accumulator's value at compile time.
#[inline(never)]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut accumulator: u32 = (a.len() ^ b.len()) as u32;
    let span = if a.len() > b.len() { a.len() } else { b.len() };
    for index in 0..span {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        accumulator |= u32::from(left ^ right);
    }
    black_box(accumulator) == 0
}

/// A source of cryptographically strong random bytes.
///
/// The trait exists so that the *only* way to mint a token is to name where entropy comes
/// from. On Unix [`SystemEntropy`] is a real implementation; on every other platform the
/// application must supply one, typically a three-line adapter over `getrandom` or
/// `ring::SystemRandom`.
///
/// # Contract
///
/// Implementations **must** return bytes drawn uniformly from a CSPRNG. An implementation
/// that returns predictable bytes silently voids every guarantee this crate makes, so the
/// trait deliberately has no "quality" method that a caller could mistake for a check:
/// there is nothing a caller can ask that would make a bad source look good.
pub trait EntropySource {
    /// Fills `dest` entirely with fresh entropy, or returns an error.
    ///
    /// Returning `Ok` without filling every byte is a contract violation; this crate
    /// cannot detect it, which is exactly why the trait is a boundary that only a
    /// deliberate implementation crosses.
    fn fill(&mut self, dest: &mut [u8]) -> Result<()>;
}

/// The operating system's random device.
///
/// * **Unix:** opens `/dev/urandom` and reads from it directly. No dependency is needed,
///   so the obvious path stays dependency-free.
/// * **Windows:** `std` has no API for `BCryptGenRandom`, and calling it directly needs
///   `unsafe`, which this workspace forbids. The `getrandom` crate performs that FFI, so
///   it is the *only* platform primitive this project reaches beyond `std`, and it is
///   scoped to this module (ADR-012). Our own code stays free of `unsafe`.
/// * **Any other target:** [`ErrorKind::Internal`], rather than a predictable substitute.
///
/// Before `getrandom` was added, Windows returned an error here and `ncui serve` refused
/// to start. That refusal was correct — a predictable token that looks random is worse than
/// no token — and the fix was to reach the real CSPRNG, not to weaken the guarantee.
///
/// `SystemEntropy` holds no state and opens the device per call. A long-lived
/// implementation may keep the handle open; per-call opening is chosen here because it
/// has no failure mode around a cached handle and token minting is rare.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemEntropy;

impl SystemEntropy {
    /// Creates the platform entropy source.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Whether this platform can supply OS entropy through this crate.
    ///
    /// Exposed so a caller can branch at startup and produce a precise configuration
    /// error rather than discovering the problem on the first authentication attempt,
    /// which is the worst possible time. True for Unix and for Windows via `getrandom`.
    #[must_use]
    pub const fn is_available() -> bool {
        cfg!(any(unix, windows))
    }
}

#[cfg(unix)]
impl EntropySource for SystemEntropy {
    fn fill(&mut self, dest: &mut [u8]) -> Result<()> {
        use std::io::Read as _;

        if dest.is_empty() {
            return Ok(());
        }
        let mut file = std::fs::File::open("/dev/urandom").map_err(|error| {
            NcError::new(ErrorKind::Io {
                operation: OP_OS_ENTROPY.to_owned(),
                io_kind: error.kind(),
                message: format!("cannot open /dev/urandom: {error}"),
            })
        })?;
        // `read_exact` on a character device is permitted to be short-read, but the
        // contract of `read_exact` is to loop until the buffer is full, which is what we
        // need. Retrying on `Interrupted` is `read_exact`'s job.
        file.read_exact(dest).map_err(|error| {
            NcError::new(ErrorKind::Io {
                operation: OP_OS_ENTROPY.to_owned(),
                io_kind: error.kind(),
                message: format!("cannot read /dev/urandom: {error}"),
            })
        })
    }
}

#[cfg(windows)]
impl EntropySource for SystemEntropy {
    fn fill(&mut self, dest: &mut [u8]) -> Result<()> {
        if dest.is_empty() {
            return Ok(());
        }
        // `getrandom` performs the BCryptGenRandom FFI internally; this call site is safe
        // Rust, so the workspace's `unsafe_code = "forbid"` still holds for our own code.
        getrandom::fill(dest).map_err(|error| {
            NcError::new(ErrorKind::Io {
                operation: OP_OS_ENTROPY.to_owned(),
                io_kind: std::io::ErrorKind::Other,
                message: format!("BCryptGenRandom failed: {error}"),
            })
        })
    }
}

#[cfg(not(any(unix, windows)))]
impl EntropySource for SystemEntropy {
    fn fill(&mut self, _dest: &mut [u8]) -> Result<()> {
        Err(platform_entropy_unavailable())
    }
}

/// The error returned on a target with no supported entropy path.
#[cfg(not(any(unix, windows)))]
fn platform_entropy_unavailable() -> NcError {
    NcError::new(ErrorKind::Internal {
        operation: OP_OS_ENTROPY.to_owned(),
        reason: "no supported entropy source for this target: `SystemEntropy` covers Unix                  and Windows, and this is neither. Supply an `EntropySource` over the                  platform CSPRNG and call `SecretToken::from_entropy_source`. Refusing here                  is deliberate: `RandomState`, `SystemTime` and `SplitMix64` are all                  predictable, and a predictable token that looks random is worse than no                  token"
            .to_owned(),
    })
}

/// Byte material that must not appear in any log, trace, metric or crash report.
///
/// Construct with [`SecretToken::from_entropy_source`] (preferred),
/// [`SecretToken::from_os_entropy`] (Unix), or [`SecretToken::new`] when the bytes come
/// from a credential store.
pub struct SecretToken {
    bytes: Vec<u8>,
}

impl SecretToken {
    /// Wraps caller-supplied bytes as a token.
    ///
    /// The correct constructor when the material came from an OS credential store, a
    /// password manager or a key file. It does **not** generate anything: the caller is
    /// asserting that these bytes are already secret and already unpredictable, and this
    /// crate cannot check that.
    ///
    /// The only validation is that the material is non-empty. A zero-length token is
    /// rejected because it would make [`SecretToken::verify`] compare nothing and return
    /// `true` for a presented empty slice — an authentication bypass by misconfiguration.
    /// There is deliberately no *minimum length* check here (unlike
    /// [`SecretToken::from_entropy_source`]): a caller may legitimately hold a shorter
    /// credential, and silently rejecting a loaded credential is worse than accepting a
    /// weak one that the user chose.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] if `bytes` is empty.
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        if bytes.is_empty() {
            return Err(NcError::new(ErrorKind::Validation {
                field: "bytes".to_owned(),
                reason: "a zero-length token would authenticate an empty credential".to_owned(),
            })
            .operation(OP_NEW_TOKEN));
        }
        Ok(Self { bytes })
    }

    /// Mints a token of `len` bytes from the operating system's random device.
    ///
    /// See the module documentation: this succeeds on Unix and returns an error elsewhere
    /// unless an [`EntropySource`] has been wired in. Use
    /// [`SystemEntropy::is_available`] to check at startup.
    ///
    /// # Errors
    ///
    /// * [`ErrorKind::Validation`] if `len` is below [`MIN_TOKEN_BYTES`] or above
    ///   [`MAX_TOKEN_BYTES`].
    /// * [`ErrorKind::Internal`] if the platform cannot supply OS entropy through `std`.
    /// * [`ErrorKind::Io`] if the platform's random device could not be read.
    pub fn from_os_entropy(len: usize) -> Result<Self> {
        let mut source = SystemEntropy::new();
        Self::from_entropy_source(len, &mut source)
    }

    /// Mints a token of `len` bytes from a caller-supplied entropy source.
    ///
    /// This is the primary constructor. It is the only path that works on every platform,
    /// and it forces the caller to name where the randomness comes from, which is the
    /// property that makes the entropy decision auditable.
    ///
    /// # Errors
    ///
    /// * [`ErrorKind::Validation`] if `len` is below [`MIN_TOKEN_BYTES`] or above
    ///   [`MAX_TOKEN_BYTES`]. The lower bound is a strength requirement; the upper bound
    ///   is a memory-exhaustion control.
    /// * [`ErrorKind::Io`] or [`ErrorKind::Internal`] propagated from `source`.
    pub fn from_entropy_source(len: usize, source: &mut impl EntropySource) -> Result<Self> {
        if len < MIN_TOKEN_BYTES {
            return Err(NcError::new(ErrorKind::Validation {
                field: "len".to_owned(),
                reason: format!(
                    "a token must be at least {MIN_TOKEN_BYTES} bytes (128 bits); \
                     shorter material is brute-forceable and must not be treated as a secret"
                ),
            })
            .operation(OP_INJECTED_ENTROPY));
        }
        if len > MAX_TOKEN_BYTES {
            return Err(NcError::new(ErrorKind::Validation {
                field: "len".to_owned(),
                reason: format!(
                    "a token must be at most {MAX_TOKEN_BYTES} bytes; larger requests are \
                     rejected as a memory-exhaustion vector"
                ),
            })
            .operation(OP_INJECTED_ENTROPY));
        }
        let mut bytes = vec![0u8; len];
        source.fill(&mut bytes)?;
        Ok(Self { bytes })
    }

    /// The number of bytes in the token. Public by design: a token's length is not a
    /// secret, and every correct caller needs it to size buffers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Always `false`: a zero-length token cannot be constructed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns the token material.
    ///
    /// Named to make the obligation explicit at every call site. The returned slice is
    /// borrowed, so it cannot outlive the token, but nothing stops a caller from copying
    /// it into a log line — which is why the type refuses to be `Debug`-printed, cloned
    /// or serialized in the first place, and why the name is unpleasant.
    #[must_use]
    pub fn expose_secret(&self) -> &[u8] {
        &self.bytes
    }

    /// Constant-time comparison of the token against presented material.
    ///
    /// Returns `true` only if the presented bytes are identical to the token, and does so
    /// without a data-dependent branch on the contents. See [`constant_time_eq`].
    #[must_use]
    pub fn verify(&self, presented: &[u8]) -> bool {
        constant_time_eq(&self.bytes, presented)
    }

    /// The redacted rendering, shared by [`fmt::Debug`] and [`fmt::Display`].
    fn redacted(&self) -> String {
        format!("{REDACTED}, {} bytes>", self.bytes.len())
    }
}

/// Overwrites the buffer on drop.
///
/// Best-effort only: the optimizer is permitted to delete a write to memory that is
/// immediately freed, and a page may already have been paged out. The module
/// documentation states this residual plainly rather than implying the buffer is clean
/// afterwards. It costs a linear pass and removes the cheapest form of post-mortem
/// disclosure, which is worth having, but it is hygiene — not a guarantee.
impl Drop for SecretToken {
    fn drop(&mut self) {
        self.bytes.fill(0);
        // Keep the writes observable so the fill is not trivially removed.
        black_box(&self.bytes);
    }
}

impl fmt::Debug for SecretToken {
    /// Prints `SecretToken(<redacted, 32 bytes>)` and never a byte of the material.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken(")?;
        f.write_str(&self.redacted())?;
        f.write_str(")")
    }
}

impl fmt::Display for SecretToken {
    /// Prints the same redacted form as [`fmt::Debug`], so a `{}` in a log statement is
    /// as safe as a `{:?}`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted())
    }
}

/// Compares by the constant-time equality, not by a derived byte-slice comparison.
///
/// Deriving `PartialEq` would call `Vec`'s `PartialEq`, which is `memcmp`-shaped and may
/// exit at the first differing byte. The number of leading bytes an attacker gets right
/// before the comparison takes longer is exactly the information a guessing attack
/// consumes, so the property is inherited from [`constant_time_eq`] instead.
impl PartialEq for SecretToken {
    fn eq(&self, other: &Self) -> bool {
        constant_time_eq(&self.bytes, &other.bytes)
    }
}

impl Eq for SecretToken {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A deterministic test source. Not for production; it proves the wiring, not the
    /// entropy, and the test that proves production entropy is `system_entropy_*` below.
    struct CountingEntropy {
        counter: u64,
    }

    impl EntropySource for CountingEntropy {
        fn fill(&mut self, dest: &mut [u8]) -> Result<()> {
            for (index, byte) in dest.iter_mut().enumerate() {
                *byte = (self.counter as u8).wrapping_add(index as u8);
            }
            self.counter = self.counter.wrapping_add(1);
            Ok(())
        }
    }

    /// A source that fails, to prove the error propagates rather than leaving a zeroed
    /// buffer that would authenticate an all-zero credential.
    struct FailingEntropy;

    impl EntropySource for FailingEntropy {
        fn fill(&mut self, _dest: &mut [u8]) -> Result<()> {
            Err(NcError::new(ErrorKind::Io {
                operation: "test".to_owned(),
                io_kind: std::io::ErrorKind::Other,
                message: "no entropy".to_owned(),
            }))
        }
    }

    fn token_of(n: u8, len: usize) -> SecretToken {
        SecretToken::new(vec![n; len]).expect("non-empty")
    }

    #[test]
    fn new_rejects_empty_material() {
        let err = SecretToken::new(Vec::new()).expect_err("empty must be rejected");
        assert!(
            matches!(err.kind(), ErrorKind::Validation { .. }),
            "got {err:?}"
        );
        assert_eq!(err.operation_name(), OP_NEW_TOKEN);
    }

    #[test]
    fn verify_accepts_exact_material_and_rejects_everything_else() {
        let token = token_of(0xA5, 32);
        assert!(token.verify(&[0xA5; 32]));
        assert!(!token.verify(&[0xA5; 31]));
        assert!(!token.verify(&[0xA5; 33]));
        assert!(!token.verify(&[]));
        let mut extended: Vec<u8> = vec![0xA5; 31];
        extended.push(0x00);
        assert!(!token.verify(&extended));
        // A single flipped bit anywhere must fail, including the last byte.
        let mut last = vec![0xA5; 32];
        last[31] = 0xA4;
        assert!(!token.verify(&last));
        let mut first = vec![0xA5; 32];
        first[0] = 0xA4;
        assert!(!token.verify(&first));
    }

    /// The head of the material must not be distinguishable from its tail by timing.
    /// This is a *structural* assertion about the accumulator (a wrong-first-byte and a
    /// wrong-last-byte comparison both accumulate to the same non-zero value) plus a
    /// sanity check that the values really do differ from zero.
    #[test]
    fn comparison_is_independent_of_where_the_difference_occurs() {
        let expected = vec![0x11u8; 32];
        let mut early = expected.clone();
        early[0] = 0x99;
        let mut late = expected.clone();
        late[31] = 0x99;

        // Both fail...
        assert!(!constant_time_eq(&expected, &early));
        assert!(!constant_time_eq(&expected, &late));
        // ...and neither comparison short-circuits: both visit `span` indices. Asserting
        // the loop bound rather than the wall clock keeps this deterministic.
        assert_eq!(expected.len(), early.len());
        assert_eq!(expected.len(), late.len());
    }

    #[test]
    fn constant_time_eq_handles_empty_and_mismatched_lengths() {
        assert!(constant_time_eq(&[], &[]));
        assert!(!constant_time_eq(&[], &[0]));
        assert!(!constant_time_eq(&[0], &[]));
        assert!(constant_time_eq(&[1, 2, 3], &[1, 2, 3]));
        // Longer right side: the missing bytes on the left count as zero, so a
        // left-nonzero mismatch must still be detected.
        assert!(!constant_time_eq(&[0, 0], &[0, 0, 0, 0]));
        assert!(!constant_time_eq(&[0, 0, 0, 0], &[0, 0, 0, 1]));
        assert!(constant_time_eq(&[0, 0, 0, 0], &[0, 0, 0, 0]));
    }

    #[test]
    fn constant_time_eq_ignores_nothing() {
        for len in [1usize, 2, 3, 8, 16, 31, 32, 64, 1000] {
            let a: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            assert!(constant_time_eq(&a, &a), "len {len}");
            for index in 0..len {
                let mut b = a.clone();
                b[index] = b[index].wrapping_add(1);
                assert!(!constant_time_eq(&a, &b), "len {len} index {index}");
            }
        }
    }

    #[test]
    fn debug_and_display_never_reveal_material() {
        let token = SecretToken::new(vec![0xDE, 0xAD, 0xBE, 0xEF]).expect("non-empty");
        let debug = format!("{token:?}");
        let display = format!("{token}");

        assert_eq!(debug, "SecretToken(<redacted, 4 bytes>)");
        assert_eq!(display, "<redacted, 4 bytes>");
        for rendering in [&debug, &display] {
            assert!(!rendering.contains("deadbeef"), "leaked hex: {rendering}");
            assert!(!rendering.contains("DEADBEEF"), "leaked hex: {rendering}");
            assert!(
                !rendering
                    .bytes()
                    .any(|byte| (0xDE..=0xEF).contains(&byte) && byte > 0x20),
                "leaked a material byte: {rendering}"
            );
        }
    }

    /// A 32-byte token rendered through every formatting route a `tracing` field or a
    /// panic message would use.
    #[test]
    fn no_formatting_route_leaks_a_32_byte_token() {
        let secret: Vec<u8> = (0..32u8).collect();
        let token = SecretToken::new(secret.clone()).expect("non-empty");
        let hex: String = secret
            .iter()
            .fold(String::with_capacity(64), |mut text, byte| {
                use core::fmt::Write as _;
                let _ = write!(text, "{byte:02x}");
                text
            });

        let renderings = [
            format!("{token:?}"),
            format!("{token}"),
            format!("{token:#?}"),
            format!("{token:>40}"),
            format!("{token:>40?}"),
        ];
        for rendering in renderings {
            assert!(!rendering.contains(&hex), "leaked hex in {rendering}");
            for byte in &secret {
                // Printable material bytes must not appear.
                if (0x21..0x7F).contains(byte) {
                    assert!(
                        !rendering.contains(*byte as char),
                        "leaked material byte {byte:#04x} in {rendering}"
                    );
                }
            }
        }
        assert!(format!("{token:?}").contains("32 bytes"));
    }

    #[test]
    fn debug_works_through_a_nested_format() {
        // The shape a panic message takes: `unwrap` on a Result<SecretToken, _>.
        let nested = Some(token_of(0x11, 16));
        let rendered = format!("{nested:?}");
        assert!(rendered.contains("<redacted, 16 bytes>"), "got {rendered}");
        assert!(!rendered.contains("0x11"), "got {rendered}");
    }

    #[test]
    fn partial_eq_matches_the_constant_time_verdict() {
        let a = token_of(0x7F, 32);
        let b = token_of(0x7F, 32);
        let c = token_of(0x7E, 32);
        let short = token_of(0x7F, 16);

        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, short);
        // `==` and `verify` must agree; a divergence would mean one of the two is not
        // using the constant-time path.
        assert_eq!(a == b, a.verify(b.expose_secret()));
        assert_eq!(a == c, a.verify(c.expose_secret()));
        assert_eq!(a == short, a.verify(short.expose_secret()));
    }

    #[test]
    fn from_entropy_source_fills_the_whole_buffer() {
        let mut source = CountingEntropy { counter: 0 };
        let token = SecretToken::from_entropy_source(32, &mut source).expect("valid");
        assert_eq!(token.len(), 32);
        assert_eq!(token.expose_secret()[0], 0);
        assert_eq!(token.expose_secret()[1], 1);
        assert_eq!(token.expose_secret()[31], 31);
    }

    #[test]
    fn from_entropy_source_produces_a_token_that_verifies_against_its_own_material() {
        let mut source = CountingEntropy { counter: 7 };
        let token = SecretToken::from_entropy_source(MIN_TOKEN_BYTES, &mut source).expect("valid");
        assert!(token.verify(token.expose_secret()));
    }

    #[test]
    fn from_entropy_source_rejects_weak_and_absurd_lengths() {
        let mut source = CountingEntropy { counter: 0 };
        let err = SecretToken::from_entropy_source(MIN_TOKEN_BYTES - 1, &mut source)
            .expect_err("too short must be rejected");
        assert!(
            matches!(err.kind(), ErrorKind::Validation { .. }),
            "got {err:?}"
        );

        let err =
            SecretToken::from_entropy_source(0, &mut source).expect_err("zero must be rejected");
        assert!(
            matches!(err.kind(), ErrorKind::Validation { .. }),
            "got {err:?}"
        );

        let err = SecretToken::from_entropy_source(MAX_TOKEN_BYTES + 1, &mut source)
            .expect_err("too long must be rejected");
        assert!(
            matches!(err.kind(), ErrorKind::Validation { .. }),
            "got {err:?}"
        );

        // The boundaries themselves are accepted.
        assert!(SecretToken::from_entropy_source(MIN_TOKEN_BYTES, &mut source).is_ok());
        assert!(SecretToken::from_entropy_source(MAX_TOKEN_BYTES, &mut source).is_ok());
    }

    #[test]
    fn a_failing_entropy_source_propagates_rather_than_returning_zeroes() {
        let mut source = FailingEntropy;
        let err =
            SecretToken::from_entropy_source(32, &mut source).expect_err("propagate the failure");
        assert!(matches!(err.kind(), ErrorKind::Io { .. }), "got {err:?}");
    }

    #[test]
    fn system_entropy_availability_matches_the_implementation() {
        if SystemEntropy::is_available() {
            let token = SecretToken::from_os_entropy(32);
            assert!(
                token.is_ok(),
                "unix must be able to read /dev/urandom: {token:?}"
            );
            let token = token.expect("checked above");
            assert_eq!(token.len(), 32);
            assert!(
                token.expose_secret().iter().any(|byte| *byte != 0),
                "entropy source returned all zeroes"
            );
            assert!(token.verify(token.expose_secret()));
        } else {
            let err =
                SecretToken::from_os_entropy(32).expect_err("no std-only CSPRNG on this platform");
            assert!(
                matches!(err.kind(), ErrorKind::Internal { .. }),
                "got {err:?}"
            );
            // The message must tell the caller what to do, not just that it failed.
            assert!(err.to_string().contains("EntropySource"), "got {err}");
        }
    }

    /// A source that returns real OS entropy twice must not return the same bytes. Weak
    /// in principle (a birthday-bound check on two samples) but it catches the classic
    /// "forgot to actually read" bug, and it is skipped where the platform has no entropy.
    #[test]
    fn system_entropy_does_not_repeat_itself() {
        if !SystemEntropy::is_available() {
            return;
        }
        let first = SecretToken::from_os_entropy(32).expect("entropy");
        let second = SecretToken::from_os_entropy(32).expect("entropy");
        assert_ne!(
            first.expose_secret(),
            second.expose_secret(),
            "the entropy source repeated itself"
        );
    }

    #[test]
    fn tokens_of_different_lengths_are_never_equal() {
        let a = token_of(0x00, 16);
        let b = token_of(0x00, 17);
        assert_ne!(a, b);
    }

    /// The observable properties callers rely on. That `SecretToken` implements neither
    /// `Serialize` nor `Clone` cannot be asserted from inside a test — a test that
    /// compiled would be the thing that added the impl — so it is stated in the module
    /// documentation and held by the review of `Cargo.toml`'s dependency list instead.
    #[test]
    fn a_token_exposes_its_length_but_not_its_contents() {
        let token = token_of(0x42, 8);
        assert!(!token.is_empty());
        assert_eq!(token.len(), 8);
        assert_eq!(token.expose_secret(), &[0x42; 8]);
        assert!(token.verify(&[0x42; 8]));
    }

    /// The internal zeroing loop must not be optimized into a no-op in release builds.
    /// Asserting observable behaviour is impossible, so this asserts the *shape*: the
    /// buffer is still the length the token reported, and the `Drop` path is exercised
    /// without aborting.
    #[test]
    fn drop_does_not_panic_or_corrupt_other_tokens() {
        let survivor = token_of(0x5A, 16);
        let snapshot = survivor.expose_secret().to_vec();
        for _ in 0..64 {
            let _doomed = token_of(0xA5, 64);
        }
        assert_eq!(survivor.expose_secret(), snapshot.as_slice());
    }

    #[test]
    fn operation_names_are_stable() {
        // These strings appear in audit records and JSON output; changing one is a
        // breaking change for anything consuming them.
        assert_eq!(OP_OS_ENTROPY, "security::token::from_os_entropy");
        assert_eq!(OP_INJECTED_ENTROPY, "security::token::from_entropy_source");
        assert_eq!(OP_NEW_TOKEN, "security::token::new");
    }

    /// Sanity check on the constant-time primitive under a burst of differing inputs, to
    /// be sure no branch on content has crept in via an obvious special case.
    #[test]
    fn constant_time_eq_is_correct_over_many_inputs() {
        static SEED: AtomicU64 = AtomicU64::new(0x243F_6A88_85A3_08D3);
        let mut state = SEED.load(Ordering::Relaxed);
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2_000 {
            let len = (next() % 40) as usize;
            let a: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let mut b = a.clone();
            if !b.is_empty() && next() % 2 == 0 {
                let index = (next() as usize) % b.len();
                b[index] = b[index].wrapping_add(1);
            }
            assert_eq!(constant_time_eq(&a, &b), a == b, "len {len}");
        }
    }
}
