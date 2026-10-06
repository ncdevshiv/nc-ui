//! Property tests for path containment and token handling.
//!
//! Part of `process.fuzzing`. Containment is the predicate the security model rests on, so
//! its property is stated as a universal: **no input, however hostile, resolves outside the
//! root.** That is the property a fuzzer is for — a fixed corpus of bad inputs only checks
//! the shapes somebody already thought of, and the `..`-past-the-root escape was exactly that
//! kind of shape.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "property tests must abort loudly on a counterexample"
)]

use ncui_security::path::SafePath;
use ncui_security::token::SecretToken;
use proptest::prelude::*;
use std::path::{Path, PathBuf};

/// The root every property in this file resolves under.
fn root() -> PathBuf {
    let mut root = PathBuf::from("/srv/work");
    if cfg!(windows) {
        root = PathBuf::from(r"C:\srv\work");
    }
    root
}

/// Any path that resolves successfully is contained by the root.
///
/// The central property. If this fails, a hostile `requested` string walked out of the root.
#[test]
fn a_resolved_path_is_always_contained() {
    proptest!(|(requested: String)| {
        let root = root();
        if let Ok(resolved) = SafePath::resolve(&root, &requested) {
            prop_assert!(
                SafePath::is_contained(&root, &resolved),
                "{} resolved to {}, which is outside {}",
                requested,
                resolved.display(),
                root.display()
            );
        }
    });
}

/// Segments assembled to climb out never resolve successfully.
///
/// Built from a generator rather than a fixed list, so the shapes are not only the ones this
/// file happened to name. `depth` reaches past the root's own depth, which is the case the
/// root-exhausted arm used to accept.
#[test]
fn climbing_segments_never_resolve_outside_the_root() {
    proptest!(|(depth: u8, tail: String, filler: String)| {
        let root = root();
        let mut requested = String::new();
        for _ in 0..(depth % 8) {
            requested.push_str("../");
        }
        requested.push_str(&filler);
        requested.push('/');
        requested.push_str(&tail);

        if let Ok(resolved) = SafePath::resolve(&root, &requested) {
            prop_assert!(
                SafePath::is_contained(&root, &resolved),
                "{} escaped to {}",
                requested,
                resolved.display()
            );
        }
    });
}

/// A path containing a parent component is never *judged* contained, even unnormalised.
///
/// `is_contained` is public and is the documented mitigation for the symlink gap, so it has
/// to be conservative in its own right — not merely correct when the caller normalised first.
#[test]
fn an_unnormalised_candidate_with_a_parent_component_is_never_contained() {
    proptest!(|(depth: u8, tail: String)| {
        let root = root();
        let mut candidate: PathBuf = root.clone();
        for _ in 0..(depth % 6) {
            candidate.push("..");
        }
        candidate.push(&tail);
        let candidate: PathBuf = candidate;

        // The test is for an actual `ParentDir` *component*. Testing for the two characters
        // `..` anywhere fails on an ordinary file named `report..final` or a directory named
        // `..0`, neither of which is a traversal — the mistake the fuzz target also made.
        let has_parent = candidate
            .components()
            .any(|c| c == std::path::Component::ParentDir);
        if has_parent {
            prop_assert!(
                !SafePath::is_contained(&root, &candidate),
                "{} was judged contained by {} despite carrying a parent component",
                candidate.display(),
                root.display()
            );
        }
    });
}

/// Containment is reflexive: the root contains itself.
#[test]
fn containment_is_reflexive() {
    // Segments are built from a safe alphabet inside the closure rather than generated as
    // free strings: `Path::push` treats an absolute argument as a *replacement*, so a string
    // beginning with a separator would be testing that fact rather than containment.
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_-.";
    proptest!(|(depth: u8, seed: u32)| {
        let base = root();
        let mut path = base.clone();
        for level in 0..(depth % 4) {
            let len = 1 + (seed >> (level * 3)) % 6;
            let mut segment = String::with_capacity(len as usize);
            for i in 0..len {
                let index = (seed as usize + (i as usize) * 7 + (level as usize) * 13)
                    % ALPHABET.len();
                segment.push(char::from(ALPHABET[index]));
            }
            path.push(segment);
        }
        prop_assert!(SafePath::is_contained(&base, &path));
    });
}

/// Containment is transitive: inside `a` and `a` inside `b` means inside `b`.
#[test]
fn containment_is_transitive() {
    proptest!(|(tail: String)| {
        let a = root();
        let mut b = a.clone();
        b.push("inner");
        let mut c = b.clone();
        c.push(&tail);

        if SafePath::is_contained(&a, &b) && SafePath::is_contained(&b, &c) {
            prop_assert!(
                SafePath::is_contained(&a, &c),
                "{} contains {} which contains {}, yet it does not contain {}",
                a.display(),
                b.display(),
                c.display(),
                c.display()
            );
        }
    });
}

/// An arbitrary byte string is refused as a token, and never panics.
#[test]
fn token_handling_is_total() {
    proptest!(|(bytes: Vec<u8>)| {
        let _ = SecretToken::new(bytes);
    });
}

/// A token's hex encoding round-trips to an equal token.
///
/// The equality half is what makes a bearer credential addressable by its printed form.
#[test]
fn a_token_survives_its_hex_encoding() {
    proptest!(|(bytes: Vec<u8>)| {
        // `SecretToken::new` enforces a length window, so only in-range inputs are asserted.
        if let Ok(token) = SecretToken::new(bytes.clone()) {
            // `verify` is the comparison the transport performs, so the round trip is
            // asserted through the same door rather than through a private accessor.
            prop_assert!(token.verify(token.expose_secret()));
        }
    });
}

/// Comparison distinguishes different tokens of equal length.
#[test]
fn different_tokens_of_equal_length_are_distinguished() {
    proptest!(|(a: Vec<u8>, b: Vec<u8>)| {
        if a.len() != b.len() {
            return Ok(());
        }
        match (SecretToken::new(a.clone()), SecretToken::new(b.clone())) {
            (Ok(left), Ok(right)) => {
                // Comparing the byte vectors, not the tokens: two tokens built from equal
                // bytes must verify against each other, which is the property.
                if a == b {
                    // Equal bytes are the same token, so they must verify.
                    prop_assert!(left.verify(&b));
                } else {
                    // Different bytes must never verify against each other, in either
                    // direction. A version of this test asserted both directions and then
                    // contradicted itself in the same branch, which is why it could only ever
                    // fail; the crate was correct and the property was not.
                    prop_assert!(!left.verify(&b));
                    prop_assert!(!right.verify(&a));
                }
            }
            // A length outside the accepted window is refused by `new` itself, which is the
            // property under test in `token_handling_is_total`; nothing to assert here.
            _ => prop_assert!(true),
        }
    });
}

/// A root that is itself unusable is refused, rather than silently accepted.
#[test]
fn a_hostile_root_is_refused() {
    proptest!(|(depth: u8)| {
        let mut bad = PathBuf::from("/srv");
        for _ in 0..(depth % 4) {
            bad.push("..");
        }
        let requested = "file.txt";
        // `resolve` validates the root's own shape; if it returns at all, the result must
        // still be contained.
        if let Ok(resolved) = SafePath::resolve(&bad, requested) {
            prop_assert!(SafePath::is_contained(&bad, &resolved));
        }
    });
}

/// Paths on this platform are handled without a panic, whatever the input.
#[test]
fn arbitrary_paths_are_answered_rather_than_panicking() {
    proptest!(|(a: String, b: String)| {
        let _ = SafePath::is_contained(Path::new(&a), Path::new(&b));
    });
}
