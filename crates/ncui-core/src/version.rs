//! Semantic versioning for public APIs and the component ABI.
//!
//! NC-UI makes explicit compatibility promises (spec 32) about its public API surface,
//! the component ABI, and the agent-facing JSON-RPC interface. This module is the single
//! implementation of those rules so that the version negotiation in the runtime and in
//! the agent tools cannot drift apart.
//!
//! Parsing follows the Semantic Versioning 2.0.0 grammar, with one deliberate
//! relaxation: build metadata is parsed and preserved rather than rejected, because the
//! component ABI needs to record it for diagnostics even though it plays no part in
//! precedence.

use core::cmp::Ordering;
use core::fmt;
use core::str::FromStr;
use serde::{Deserialize, Serialize};

/// Error returned when a version string does not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseVersionError {
    /// The unparseable input, truncated.
    pub input: String,
    /// Why parsing failed.
    pub reason: &'static str,
}

impl fmt::Display for ParseVersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid semantic version {:?}: {}",
            self.input, self.reason
        )
    }
}

impl std::error::Error for ParseVersionError {}

/// A semantic version: `MAJOR.MINOR.PATCH[-prerelease][+build]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SemVer {
    /// Breaking-change counter.
    pub major: u64,
    /// Feature counter.
    pub minor: u64,
    /// Bugfix counter.
    pub patch: u64,
    /// Dot-separated pre-release identifiers, e.g. `["alpha", "1"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre: Vec<String>,
    /// Dot-separated build identifiers, ignored for precedence.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub build: String,
}

impl SemVer {
    /// Parses a version string.
    pub fn parse(input: &str) -> Result<Self, ParseVersionError> {
        let fail = |reason: &'static str| ParseVersionError {
            input: input.chars().take(64).collect(),
            reason,
        };

        // Build metadata is split off first; it is the only part permitted to contain
        // characters that would confuse the core parser.
        let (without_build, build) = match input.split_once('+') {
            Some((_, "")) => return Err(fail("build metadata is present but empty")),
            Some((head, tail)) => (head, tail.to_owned()),
            None => (input, String::new()),
        };
        if !build.is_empty() && !is_valid_dot_identifiers(&build, is_valid_identifier) {
            return Err(fail(
                "build metadata must be dot-separated alphanumeric identifiers",
            ));
        }

        let (core, pre) = match without_build.split_once('-') {
            Some((head, tail)) => {
                if !is_valid_dot_identifiers(tail, is_valid_identifier) {
                    return Err(fail(
                        "pre-release identifiers must be alphanumeric or hyphen",
                    ));
                }
                (head, tail.split('.').map(str::to_owned).collect())
            }
            None => (without_build, Vec::new()),
        };

        let mut parts = core.split('.');
        let mut next = || {
            parts
                .next()
                .ok_or_else(|| fail("expected MAJOR.MINOR.PATCH"))
                .and_then(|segment| {
                    if segment.is_empty() {
                        return Err(fail("version component is empty"));
                    }
                    if !segment.chars().all(|c| c.is_ascii_digit()) {
                        return Err(fail("version components must be decimal digits"));
                    }
                    // SemVer 2.0.0 §9: a numeric identifier is `0` or a positive digit
                    // followed by digits, so `01` is not a valid version component. Without
                    // this, `01.2.3` parsed to `1.2.3` and rendered as `1.2.3`, so two
                    // distinct wire spellings bound to the same version with no diagnostic
                    // — the module below already enforced this rule for pre-release and
                    // build identifiers via `is_valid_identifier`, and `1.0.0-01` was
                    // rejected while `1.2.03` was accepted.
                    if segment.len() > 1 && segment.starts_with('0') {
                        return Err(fail("version components must not have leading zeros"));
                    }
                    segment
                        .parse::<u64>()
                        .map_err(|_| fail("version component is too large"))
                })
        };

        let major = next()?;
        let minor = next()?;
        let patch = next()?;
        if parts.next().is_some() {
            return Err(fail("expected exactly MAJOR.MINOR.PATCH"));
        }

        Ok(Self {
            major,
            minor,
            patch,
            pre,
            build,
        })
    }

    /// The major version, i.e. the compatibility boundary.
    #[must_use]
    pub const fn major(&self) -> u64 {
        self.major
    }

    /// Whether this version carries a pre-release marker.
    #[must_use]
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    /// Whether a peer implementing `peer` satisfies a requirement stated at this version.
    ///
    /// This is caret-range negotiation, and it is deliberately directional: `self` is the
    /// *requirement* and `peer` is the *implementation*. `1.2.0.is_compatible_with(1.1.0)`
    /// is false because a peer that is older than the required version cannot satisfy it,
    /// while `1.2.0.is_compatible_with(1.9.9)` is true.
    ///
    /// The rules are:
    ///
    /// * The major version must match exactly.
    /// * While the major version is `0`, the minor version must also match exactly,
    ///   because in `0.y.z` the minor version is where breaking changes land.
    /// * The peer must be at least as new as the requirement.
    /// * A peer carrying a pre-release marker only satisfies a requirement sharing its
    ///   exact `major.minor.patch` triple, so an agent can never silently bind to an
    ///   unreleased build while asking for a stable one.
    #[must_use]
    pub fn is_compatible_with(&self, peer: &Self) -> bool {
        if self.major != peer.major {
            return false;
        }
        if self.major == 0 && self.minor != peer.minor {
            return false;
        }
        if peer.is_prerelease() {
            return peer.major == self.major
                && peer.minor == self.minor
                && peer.patch == self.patch
                && peer.cmp(self) != Ordering::Less;
        }
        // The peer is a stable release, so it must be at or beyond the requirement.
        self.cmp(peer) != Ordering::Greater
    }
}

/// Whether one pre-release or build identifier is legal under the specification.
///
/// Identifiers may be purely numeric or alphanumeric-with-hyphens. A purely numeric
/// identifier must not carry leading zeros, because `1.0.0-01` would otherwise have two
/// spellings that compare differently under the precedence rules.
fn is_valid_identifier(identifier: &str) -> bool {
    if identifier.is_empty()
        || !identifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return false;
    }
    if identifier.chars().all(|c| c.is_ascii_digit()) {
        return identifier.len() == 1 || !identifier.starts_with('0');
    }
    true
}

fn is_valid_dot_identifiers(input: &str, check: impl Fn(&str) -> bool) -> bool {
    !input.is_empty() && input.split('.').all(check)
}

impl FromStr for SemVer {
    type Err = ParseVersionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            f.write_str("-")?;
            let mut first = true;
            for identifier in &self.pre {
                if !first {
                    f.write_str(".")?;
                }
                first = false;
                write!(f, "{identifier}")?;
            }
        }
        if !self.build.is_empty() {
            let _ = write!(f, "+{}", self.build);
        }
        Ok(())
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        // Precedence per SemVer 2.0.0 §11: numeric components first, then pre-release
        // presence, then identifier-by-identifier comparison where numeric identifiers
        // rank below alphanumeric ones. Build metadata is excluded entirely.
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A release outranks any of its pre-releases.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => compare_pre(&self.pre, &other.pre),
            })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_pre(left: &[String], right: &[String]) -> Ordering {
    for (a, b) in left.iter().zip(right.iter()) {
        let a_numeric = a.chars().all(|c| c.is_ascii_digit());
        let b_numeric = b.chars().all(|c| c.is_ascii_digit());
        let ordering = match (a_numeric, b_numeric) {
            (true, true) => match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                // A numeric identifier that overflows u64 still sorts deterministically
                // by length-then-lexicographic, which matches numeric order for the
                // magnitudes involved and never panics.
                _ => a.len().cmp(&b.len()).then_with(|| a.cmp(b)),
            },
            // Numeric identifiers always have lower precedence than alphanumeric ones.
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => a.cmp(b),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_canonical_form() {
        let version = SemVer::parse("1.4.2").expect("valid version");
        assert_eq!(version.major, 1);
        assert_eq!(version.minor, 4);
        assert_eq!(version.patch, 2);
        assert!(!version.is_prerelease());
        assert_eq!(version.to_string(), "1.4.2");
    }

    #[test]
    fn parses_pre_release_and_build_metadata() {
        let version = SemVer::parse("2.0.0-rc.1+build.77").expect("valid version");
        assert!(version.is_prerelease());
        assert_eq!(version.pre, vec!["rc".to_owned(), "1".to_owned()]);
        assert_eq!(version.build, "build.77");
        assert_eq!(version.to_string(), "2.0.0-rc.1+build.77");
    }

    #[test]
    fn rejects_malformed_versions() {
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "a.b.c",
            "1.2.x",
            "1..3",
            "-1.2.3",
            "1.2.3-",
            "1.2.3-rc..1",
            "1.2.3+",
            "1.2.3+b..d",
            " 1.2.3",
            // SemVer 2.0.0 §9: a numeric identifier is `0`, or a positive digit followed by
            // digits. These were all accepted and normalised to `1.2.3`, so two different
            // wire spellings bound to the same version with no diagnostic. The module below
            // already rejected `1.0.0-01` for exactly this reason, and the inconsistency was
            // only that the rule was not applied to the core triple.
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "00.1.2",
            "1.2.3-rc.01",
            "1.2.3+001",
        ] {
            assert!(
                SemVer::parse(bad).is_err(),
                "{bad:?} should not parse as a semantic version"
            );
        }
    }

    #[test]
    fn ordering_follows_the_specification() {
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.1.0",
            "2.0.0",
        ];
        for window in ordered.windows(2) {
            let low = SemVer::parse(window[0]).expect("valid");
            let high = SemVer::parse(window[1]).expect("valid");
            assert!(low < high, "expected {} < {}", window[0], window[1]);
        }
    }

    #[test]
    fn build_metadata_is_ignored_for_precedence() {
        let a = SemVer::parse("1.0.0+one").expect("valid");
        let b = SemVer::parse("1.0.0+two").expect("valid");
        assert_eq!(a.cmp(&b), Ordering::Equal);
    }

    #[test]
    fn compatibility_is_directional() {
        let required = SemVer::parse("1.2.0").expect("valid");
        // A newer peer in the same major satisfies the requirement.
        assert!(required.is_compatible_with(&SemVer::parse("1.9.9").expect("valid")));
        assert!(required.is_compatible_with(&SemVer::parse("1.2.0").expect("valid")));
        assert!(required.is_compatible_with(&SemVer::parse("1.2.7").expect("valid")));
        // An older peer, a different major, and a zero-major peer all fail.
        assert!(!required.is_compatible_with(&SemVer::parse("1.1.0").expect("valid")));
        assert!(!required.is_compatible_with(&SemVer::parse("0.9.0").expect("valid")));
        assert!(!required.is_compatible_with(&SemVer::parse("2.0.0").expect("valid")));
    }

    #[test]
    fn zero_major_pins_the_minor_version() {
        let required = SemVer::parse("0.4.1").expect("valid");
        assert!(required.is_compatible_with(&SemVer::parse("0.4.9").expect("valid")));
        assert!(!required.is_compatible_with(&SemVer::parse("0.4.0").expect("valid")));
        assert!(!required.is_compatible_with(&SemVer::parse("0.3.9").expect("valid")));
        assert!(!required.is_compatible_with(&SemVer::parse("0.5.0").expect("valid")));
    }

    #[test]
    fn pre_release_peers_only_satisfy_their_own_triple() {
        let stable = SemVer::parse("1.0.0").expect("valid");
        let rc1 = SemVer::parse("1.0.0-rc.1").expect("valid");
        let rc2 = SemVer::parse("1.0.0-rc.2").expect("valid");
        let next_minor = SemVer::parse("1.1.0-rc.1").expect("valid");

        assert!(
            !stable.is_compatible_with(&rc2),
            "stable must not silently take a prerelease"
        );
        assert!(
            rc1.is_compatible_with(&rc2),
            "a later rc satisfies an earlier one"
        );
        assert!(!rc2.is_compatible_with(&rc1));
        assert!(!stable.is_compatible_with(&next_minor));
    }
}
