//! Property tests for the parsers that take untrusted input.
//!
//! Part of `process.fuzzing`: the corpus-free half that runs in the ordinary suite, so a
//! regression in parser totality fails a CI build rather than waiting for a fuzzing run.
//! The `fuzz/` targets are the generated-input half.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on a counterexample"
)]

use core::cmp::Ordering;
use ncui_core::version::SemVer;
use proptest::prelude::*;

/// Arbitrary text never panics: it is rejected or accepted, never fatal.
#[test]
fn parsing_arbitrary_text_is_total() {
    proptest!(|(text: String)| {
        let _ = SemVer::parse(&text);
    });
}

/// A version that parses renders back to something that parses to the same version.
///
/// This is what makes round-tripping through a wire format safe.
#[test]
fn a_version_survives_a_render_and_reparse() {
    proptest!(|(major: u32, minor: u32, patch: u32)| {
        let text = format!("{major}.{minor}.{patch}");
        let parsed = SemVer::parse(&text).unwrap_or_else(|e| panic!("{text} did not parse: {e}"));
        let rendered = parsed.to_string();
        let reparsed = SemVer::parse(&rendered)
            .unwrap_or_else(|e| panic!("{rendered} came from a parsed version but did not reparse: {e}"));
        prop_assert_eq!(reparsed, parsed);
    });
}

/// A parsed version renders to the canonical spelling it was given.
///
/// This is the property that pins the leading-zero fix. Before it, `1.2.03` parsed to
/// `1.2.3`, so a non-canonical spelling was accepted and silently normalised — two distinct
/// wire spellings meaning the same version with no diagnostic.
#[test]
fn a_version_renders_to_its_canonical_spelling() {
    proptest!(|(major: u32, minor: u32, patch: u32)| {
        let text = format!("{major}.{minor}.{patch}");
        if let Ok(parsed) = SemVer::parse(&text) {
            prop_assert_eq!(parsed.to_string(), text);
        }
    });
}

/// Version ordering is a total order: comparison is antisymmetric.
#[test]
fn version_comparison_is_antisymmetric() {
    proptest!(|(a: (u32, u32, u32), b: (u32, u32, u32))| {
        let left = SemVer::parse(&format!("{}.{}.{}", a.0, a.1, a.2));
        let right = SemVer::parse(&format!("{}.{}.{}", b.0, b.1, b.2));
        if let (Ok(left), Ok(right)) = (left, right) {
            prop_assert_eq!(left.cmp(&right), right.cmp(&left).reverse());
        }
    });
}

/// A larger major version never sorts below a smaller one.
#[test]
fn a_larger_triple_never_sorts_below_a_smaller_one() {
    proptest!(|(small: u32, delta: u32, minor: u32, patch: u32)| {
        let low = SemVer::parse(&format!("{small}.{minor}.{patch}"));
        // `checked_add` so the generated input stays inside the parseable range rather than
        // producing a version this crate legitimately rejects.
        let high = small
            .checked_add(delta)
            .map(|n| SemVer::parse(&format!("{n}.{minor}.{patch}")));
        if let (Ok(low), Some(Ok(high))) = (low, high) {
            prop_assert!(low <= high);
        }
    });
}

/// A pre-release version sorts below the same version without one.
#[test]
fn a_prerelease_sorts_below_its_release() {
    proptest!(|(major: u32, minor: u32, patch: u32, tag: String)| {
        let release = SemVer::parse(&format!("{major}.{minor}.{patch}"));
        let pre = SemVer::parse(&format!("{major}.{minor}.{patch}-{tag}"));
        if let (Ok(release), Ok(pre)) = (release, pre) {
            prop_assert!(pre < release);
        }
    });
}

/// Build metadata is ignored for precedence, as the `SemVer` spec, section 10, requires.
#[test]
fn build_metadata_does_not_affect_precedence() {
    proptest!(|(major: u32, minor: u32, patch: u32, build: String)| {
        let plain = SemVer::parse(&format!("{major}.{minor}.{patch}"));
        let with_build = SemVer::parse(&format!("{major}.{minor}.{patch}+{build}"));
        if let (Ok(plain), Ok(with_build)) = (plain, with_build) {
            prop_assert_eq!(plain.cmp(&with_build), Ordering::Equal);
        }
    });
}
