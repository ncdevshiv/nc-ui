//! The error type a GPUI host reports, with the stable machine-readable codes the
//! registry carries into refusals.

/// One way the GPUI host can fail.
///
/// Every variant carries the values that caused it, because a refusal that says only
/// "the engine failed" leaves an operator guessing which of the bounds was hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuiHostError {
    /// The engine thread could not be started or has died.
    EngineUnavailable {
        /// What the engine reported, or what the supervisor observed.
        reason: String,
    },
    /// A command sent to the engine thread was not answered in time.
    EngineTimeout {
        /// Which operation timed out.
        operation: String,
    },
    /// The window the operation named is not open.
    WindowGone {
        /// The window the caller asked about.
        window: String,
    },
    /// The content provider a native window named is not registered.
    UnknownProvider {
        /// The provider identifier the window asked for.
        provider: String,
    },
    /// The provider refused the props it was given.
    ProviderRefused {
        /// The provider identifier.
        provider: String,
        /// What the provider reported.
        reason: String,
    },
    /// The factory was asked for content it does not host.
    WrongContentKind {
        /// What the factory was asked for.
        kind: String,
    },
    /// The platform cannot perform the operation at all.
    ///
    /// Distinct from a refusal: the provider was never asked, because gpui 0.2.2 has no
    /// call that could answer — there is no unmaximise, no hide, no repositioning. The
    /// reason names the missing capability rather than pretending a decision was made.
    UnsupportedOperation {
        /// Which operation was asked for.
        operation: String,
        /// Why the platform cannot do it.
        reason: String,
    },
    /// The asset source a factory was configured with cannot be honoured, because the
    /// engine is already running and gpui fixes the application's asset source at
    /// construction.
    ///
    /// This is an ordering failure, not a missing capability: the first construction
    /// wins, so a factory that needs its own source must be the one that starts the
    /// engine — or must share, by `Arc` identity, the source the running engine was
    /// built with. The field distinguishes the two ways the conflict arises, because
    /// they are different composition mistakes: an engine with no asset source means
    /// assets were never configured, while an engine carrying one means two different
    /// sources were configured and only the first could win.
    AssetSourceConflict {
        /// Whether the running engine carries any asset source at all: `false` means it
        /// was constructed with gpui's empty default, `true` means it carries one that
        /// is not the one being asked for.
        engine_has_assets: bool,
    },
}

impl GpuiHostError {
    /// The stable machine-readable failure name.
    ///
    /// These strings are part of the agent-facing API surface. Renaming one is a breaking
    /// change, so they are concentrated here where a review sees them together.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::EngineUnavailable { .. } => "gpui_engine_unavailable",
            Self::EngineTimeout { .. } => "gpui_engine_timeout",
            Self::WindowGone { .. } => "gpui_window_gone",
            Self::UnknownProvider { .. } => "gpui_unknown_provider",
            Self::ProviderRefused { .. } => "gpui_provider_refused",
            Self::WrongContentKind { .. } => "gpui_wrong_content_kind",
            Self::UnsupportedOperation { .. } => "gpui_unsupported_operation",
            Self::AssetSourceConflict { .. } => "gpui_asset_source_conflict",
        }
    }
}

impl core::fmt::Display for GpuiHostError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EngineUnavailable { reason } => {
                write!(f, "the GPUI engine is unavailable: {reason}")
            }
            Self::EngineTimeout { operation } => {
                write!(f, "the GPUI engine did not answer {operation} in time")
            }
            Self::WindowGone { window } => write!(f, "the window {window} is not open"),
            Self::UnknownProvider { provider } => {
                write!(f, "no native view provider named {provider} is registered")
            }
            Self::ProviderRefused { provider, reason } => {
                write!(f, "the provider {provider} refused its props: {reason}")
            }
            Self::WrongContentKind { kind } => {
                write!(f, "the GPUI host cannot host {kind} content")
            }
            Self::UnsupportedOperation { operation, reason } => {
                write!(f, "the platform cannot {operation}: {reason}")
            }
            Self::AssetSourceConflict { engine_has_assets } => {
                let installed = if *engine_has_assets {
                    "a different asset source"
                } else {
                    "gpui's empty default asset source"
                };
                write!(
                    f,
                    "the asset source cannot be installed: the GPUI engine is already running \
                     with {installed}, and gpui fixes the application's asset source at \
                     construction — the factory carrying assets must start the engine, share \
                     the running engine's source, or accept the running engine as it is"
                )
            }
        }
    }
}

impl std::error::Error for GpuiHostError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_a_distinct_stable_code() {
        let errors = [
            GpuiHostError::EngineUnavailable {
                reason: "x".to_owned(),
            },
            GpuiHostError::EngineTimeout {
                operation: "x".to_owned(),
            },
            GpuiHostError::WindowGone {
                window: "x".to_owned(),
            },
            GpuiHostError::UnknownProvider {
                provider: "x".to_owned(),
            },
            GpuiHostError::ProviderRefused {
                provider: "x".to_owned(),
                reason: "y".to_owned(),
            },
            GpuiHostError::WrongContentKind {
                kind: "x".to_owned(),
            },
            GpuiHostError::UnsupportedOperation {
                operation: "x".to_owned(),
                reason: "y".to_owned(),
            },
            GpuiHostError::AssetSourceConflict {
                engine_has_assets: false,
            },
        ];
        let mut codes: Vec<&str> = errors.iter().map(GpuiHostError::code).collect();
        let count = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), count, "every variant needs its own code");
    }

    #[test]
    fn display_messages_render_the_values_that_caused_them() {
        let error = GpuiHostError::UnknownProvider {
            provider: "kit::gallery".to_owned(),
        };
        assert!(error.to_string().contains("kit::gallery"));
    }
}
