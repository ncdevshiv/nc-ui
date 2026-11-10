//! Every way a component can be refused, named.
//!
//! A component runs inside the user's process, so the reason it did not run is the difference
//! between an operator who can act and one who cannot. Each variant therefore states which
//! bound or rule produced it and what the value was: a refusal that says only "rejected"
//! teaches nobody anything, and the first thing a component author does is search for the reason.

use core::fmt;

/// Why a component did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentError {
    /// The host's own limits were configured so that they bound nothing.
    InvalidLimits(String),
    /// Two host limits contradict each other, so a request could never be served.
    InconsistentLimits(String),
    /// The artifact is larger than `max_module_bytes`.
    TooLarge {
        /// Bytes the artifact had.
        bytes: usize,
        /// The bound it exceeded.
        limit: usize,
    },
    /// The engine would not compile it: invalid bytes, a start section, or a feature this host
    /// has not enabled.
    InvalidModule(String),
    /// The module imports something other than the two services in the contract.
    UnlinkedImport(String),
    /// A required export is missing.
    MissingExport(String),
    /// A required export exists but is not the kind of thing the contract says it is.
    BadExport {
        /// The export's name.
        name: String,
        /// What the host found instead.
        reason: String,
    },
    /// The component's own numeric ABI revision is not the one this host implements.
    AbiMismatch {
        /// The revision the component's export returned.
        declared: i32,
        /// The revision the host implements.
        expected: i32,
    },
    /// The manifest document exceeded its bound.
    ManifestTooLarge {
        /// Bytes the manifest claimed.
        bytes: usize,
        /// The bound.
        limit: usize,
    },
    /// The manifest could not be read.
    MalformedManifest(String),
    /// Negotiation refused the component, so nothing was linked or run.
    NotNegotiated(String),
    /// The scratch region is too small for the method name and the payload together.
    ScratchTooSmall {
        /// Bytes the call needed.
        needed: u32,
        /// Bytes the component offered.
        available: u32,
    },
    /// The component named a document outside its own memory.
    GuestBounds(String),
    /// Guest code ran out of fuel.
    FuelExhausted {
        /// The budget the call was given.
        fuel: u64,
    },
    /// Guest code trapped for any other reason.
    Trapped(String),
    /// The component's response exceeded its bound.
    OutputTooLarge {
        /// Bytes the component returned.
        bytes: u32,
        /// The bound.
        limit: usize,
    },
    /// The response was not readable as UTF-8 JSON.
    BadResult(String),
    /// Something the *caller* handed the host was longer than the bound for it, so the call was
    /// never attempted. A method name or request body from the runtime's own code is not a
    /// component fault, and blaming the artifact would point the reader at the wrong file.
    Overlong {
        /// What was too long, in the caller's own terms.
        what: String,
        /// Its length.
        bytes: usize,
        /// The bound.
        limit: usize,
    },
    /// The request could not be written as JSON.
    Unserialisable(String),
    /// The caller asked for a method or service the component never declared.
    Undeclared {
        /// What kind of thing was asked for.
        kind: &'static str,
        /// The name that is not on the declaration.
        name: String,
    },
}

impl ComponentError {
    /// Whether the failure is the component's or the host's.
    ///
    /// A caller that logs or retries needs the distinction: an artifact that will not compile
    /// fails identically forever, while a trap under an unusual payload is worth reporting.
    #[must_use]
    pub const fn is_component_fault(&self) -> bool {
        matches!(
            self,
            Self::TooLarge { .. }
                | Self::InvalidModule(_)
                | Self::UnlinkedImport(_)
                | Self::MissingExport(_)
                | Self::BadExport { .. }
                | Self::AbiMismatch { .. }
                | Self::ManifestTooLarge { .. }
                | Self::MalformedManifest(_)
                | Self::NotNegotiated(_)
                | Self::ScratchTooSmall { .. }
                | Self::GuestBounds(_)
                | Self::FuelExhausted { .. }
                | Self::Trapped(_)
                | Self::OutputTooLarge { .. }
                | Self::BadResult(_)
                | Self::Undeclared { .. }
        )
    }
}

impl fmt::Display for ComponentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits(field) => {
                write!(f, "the component limits are unusable: `{field}` is zero")
            }
            Self::InconsistentLimits(reason) => {
                write!(f, "the component limits contradict each other: {reason}")
            }
            Self::TooLarge { bytes, limit } => write!(
                f,
                "the component artifact is {bytes} bytes, over the host bound of {limit}"
            ),
            Self::InvalidModule(reason) => {
                write!(f, "the component would not compile: {reason}")
            }
            Self::UnlinkedImport(reason) => write!(
                f,
                "the component imports a service this host does not provide: {reason}"
            ),
            Self::MissingExport(name) => {
                write!(
                    f,
                    "the component does not export `{name}`, which the contract requires"
                )
            }
            Self::BadExport { name, reason } => {
                write!(f, "the component's export `{name}` is unusable: {reason}")
            }
            Self::AbiMismatch { declared, expected } => write!(
                f,
                "the component was built against calling convention {declared}, this host \
                 implements {expected}"
            ),
            Self::ManifestTooLarge { bytes, limit } => write!(
                f,
                "the component's manifest claims {bytes} bytes, over the bound of {limit}"
            ),
            Self::MalformedManifest(reason) => {
                write!(f, "the component's manifest could not be read: {reason}")
            }
            Self::NotNegotiated(reason) => {
                write!(
                    f,
                    "the component was not negotiated with this host: {reason}"
                )
            }
            Self::ScratchTooSmall { needed, available } => write!(
                f,
                "the call needs {needed} bytes of scratch and the component offers {available}"
            ),
            Self::GuestBounds(reason) => {
                write!(
                    f,
                    "the component named a document outside its own memory: {reason}"
                )
            }
            Self::FuelExhausted { fuel } => write!(
                f,
                "the component used all {fuel} units of fuel it was given and was stopped"
            ),
            Self::Trapped(reason) => write!(f, "the component trapped: {reason}"),
            Self::OutputTooLarge { bytes, limit } => write!(
                f,
                "the component returned {bytes} bytes, over the response bound of {limit}"
            ),
            Self::BadResult(reason) => {
                write!(f, "the component's response is not readable: {reason}")
            }
            Self::Unserialisable(reason) => {
                write!(f, "the request could not be written as JSON: {reason}")
            }
            Self::Overlong { what, bytes, limit } => write!(
                f,
                "the {what} is {bytes} bytes, over the host bound of {limit}"
            ),
            Self::Undeclared { kind, name } => write!(
                f,
                "the component declares no {kind} named `{name}`, so it cannot be asked for one"
            ),
        }
    }
}

impl std::error::Error for ComponentError {}

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
    fn every_variant_renders_with_the_number_that_caused_it() {
        // A refusal the operator cannot read is a refusal they cannot act on, and the values
        // are the actionable part.
        let cases: Vec<(ComponentError, &[&str])> = vec![
            (
                ComponentError::TooLarge {
                    bytes: 12_345,
                    limit: 1024,
                },
                &["12345", "1024"],
            ),
            (
                ComponentError::MissingExport("ncui_invoke".to_owned()),
                &["ncui_invoke"],
            ),
            (
                ComponentError::AbiMismatch {
                    declared: 2,
                    expected: 1,
                },
                &["2", "1"],
            ),
            (
                ComponentError::ScratchTooSmall {
                    needed: 4096,
                    available: 8,
                },
                &["4096", "8"],
            ),
            (ComponentError::FuelExhausted { fuel: 99 }, &["99", "fuel"]),
            (
                ComponentError::OutputTooLarge { bytes: 9, limit: 8 },
                &["9", "8"],
            ),
            (
                ComponentError::InvalidLimits("max_module_bytes".to_owned()),
                &["max_module_bytes"],
            ),
            (
                ComponentError::BadExport {
                    name: "ncui_scratch".to_owned(),
                    reason: "it is a function, the contract wants a memory-backed region"
                        .to_owned(),
                },
                &["ncui_scratch"],
            ),
        ];
        for (error, needles) in cases {
            let rendered = error.to_string();
            for needle in needles {
                assert!(
                    rendered.contains(needle),
                    "{error} should mention {needle:?}"
                );
            }
            assert!(!rendered.is_empty());
        }
    }

    #[test]
    fn the_limit_variants_are_not_component_faults() {
        // These are the host's own misconfiguration. Reporting them as a bad artifact would
        // point every component author at a bug in the host.
        assert!(!ComponentError::InvalidLimits("x".to_owned()).is_component_fault());
        assert!(!ComponentError::InconsistentLimits("x".to_owned()).is_component_fault());
        assert!(ComponentError::FuelExhausted { fuel: 1 }.is_component_fault());
        assert!(
            ComponentError::NotNegotiated("no ABI 2.0".to_owned()).is_component_fault(),
            "a component built for a host this is not is still the component's answer"
        );
    }

    #[test]
    fn the_error_type_is_usable_as_a_std_error() {
        let boxed: Box<dyn std::error::Error> =
            Box::new(ComponentError::Trapped("integer overflow".to_owned()));
        assert!(boxed.to_string().contains("integer overflow"));
    }
}
