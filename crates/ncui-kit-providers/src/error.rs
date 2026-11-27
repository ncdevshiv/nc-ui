//! The bridge's typed failures.
//!
//! The provider contract carries refusals as strings because `ncui-gpui` cannot know
//! every provider's taxonomy. Keeping a typed error inside the bridge still matters: the
//! validation code composes exact failures here, and only the trait boundary erases them
//! into the rendering the runtime is allowed to see.

/// One way a kit provider can refuse props, a command or an asset path.
///
/// Every variant carries the values that caused it. A refusal naming only "invalid
/// props" would push the burden of guessing onto a caller that cannot inspect the view,
/// so validation failures always carry the field that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KitProviderError {
    /// The top-level props value was neither an object nor the empty-object shorthand
    /// `null`.
    PropsNotObject {
        /// The provider that rejected the props.
        provider: &'static str,
        /// The JSON kind that was supplied instead.
        actual: &'static str,
    },
    /// One props field failed validation.
    InvalidProps {
        /// The provider that rejected the props.
        provider: &'static str,
        /// The field that failed, using array indices where nested values are involved.
        field: String,
        /// What is wrong with the field.
        reason: String,
    },
    /// The command names a verb the provider does not expose.
    UnknownAction {
        /// The provider that rejected the command.
        provider: &'static str,
        /// The verb the caller asked for.
        action: String,
    },
    /// A known command received arguments of the wrong shape.
    InvalidActionPayload {
        /// The provider that rejected the command.
        provider: &'static str,
        /// The verb whose arguments failed.
        action: String,
        /// The payload field that failed, or `payload` when the whole value did.
        field: String,
        /// What is wrong with the field.
        reason: String,
    },
    /// A known command is meaningful but unavailable in the view's current state.
    ActionUnavailable {
        /// The provider that rejected the command.
        provider: &'static str,
        /// The verb that cannot run now.
        action: String,
        /// Why the state does not admit the verb.
        reason: String,
    },
    /// An asset path would escape the resolved asset root.
    AssetPathOutsideRoot {
        /// The path that was refused, rendered as supplied.
        path: String,
    },
}

impl KitProviderError {
    /// The stable machine-readable failure name.
    ///
    /// These strings are stable machine-readable diagnostics. They are also rendered at
    /// the start of [`Display`](core::fmt::Display) output because the native-provider
    /// trait boundary erases typed errors into strings; dropping the code there would make
    /// the taxonomy impossible for a host to recover.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::PropsNotObject { .. } => "kit_props_not_object",
            Self::InvalidProps { .. } => "kit_invalid_props",
            Self::UnknownAction { .. } => "kit_unknown_action",
            Self::InvalidActionPayload { .. } => "kit_invalid_action_payload",
            Self::ActionUnavailable { .. } => "kit_action_unavailable",
            Self::AssetPathOutsideRoot { .. } => "kit_asset_path_outside_root",
        }
    }
}

impl core::fmt::Display for KitProviderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "[{}] ", self.code())?;
        match self {
            Self::PropsNotObject { provider, actual } => write!(
                f,
                "the provider {provider} expected props to be a JSON object or null, not {actual}"
            ),
            Self::InvalidProps {
                provider,
                field,
                reason,
            } => {
                write!(
                    f,
                    "the provider {provider} rejected props field `{field}`: {reason}"
                )
            }
            Self::UnknownAction { provider, action } => {
                write!(
                    f,
                    "the provider {provider} does not expose an action named `{action}`"
                )
            }
            Self::InvalidActionPayload {
                provider,
                action,
                field,
                reason,
            } => write!(
                f,
                "the provider {provider} rejected action `{action}` at field `{field}`: {reason}"
            ),
            Self::ActionUnavailable {
                provider,
                action,
                reason,
            } => {
                write!(
                    f,
                    "the provider {provider} cannot perform `{action}` now: {reason}"
                )
            }
            Self::AssetPathOutsideRoot { path } => {
                write!(
                    f,
                    "the asset path `{path}` would escape the resolved asset root"
                )
            }
        }
    }
}

impl std::error::Error for KitProviderError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_a_distinct_stable_code() {
        let errors = [
            KitProviderError::PropsNotObject {
                provider: "p",
                actual: "array",
            },
            KitProviderError::InvalidProps {
                provider: "p",
                field: "f".to_owned(),
                reason: "r".to_owned(),
            },
            KitProviderError::UnknownAction {
                provider: "p",
                action: "a".to_owned(),
            },
            KitProviderError::InvalidActionPayload {
                provider: "p",
                action: "a".to_owned(),
                field: "f".to_owned(),
                reason: "r".to_owned(),
            },
            KitProviderError::ActionUnavailable {
                provider: "p",
                action: "a".to_owned(),
                reason: "r".to_owned(),
            },
            KitProviderError::AssetPathOutsideRoot {
                path: "../x".to_owned(),
            },
        ];
        let mut codes: Vec<&str> = errors.iter().map(KitProviderError::code).collect();
        let count = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), count, "every variant needs its own code");
    }

    #[test]
    fn display_messages_render_the_values_that_caused_them() {
        let error = KitProviderError::InvalidProps {
            provider: "nc-ui-kit::button",
            field: "label".to_owned(),
            reason: "must be a string".to_owned(),
        };
        let rendered = error.to_string();
        assert!(
            rendered.starts_with("[kit_invalid_props]"),
            "the string crossing the provider trait boundary preserves the machine-readable code: {rendered}"
        );
        assert!(rendered.contains("nc-ui-kit::button"));
        assert!(rendered.contains("label"));
        assert!(rendered.contains("must be a string"));
    }
}
