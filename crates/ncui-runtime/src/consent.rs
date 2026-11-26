//! Host approval for destructive agent and page operations.
//!
//! A page or remote agent cannot mint its own approval. The runtime either validates a
//! host-issued, subject- and argument-bound token or asks the installed host prompter after
//! authentication, rate limiting and standing authorization have succeeded.

use ncui_core::{CapabilitySet, CorrelationId, ErrorKind, NcError, Result};
#[cfg(feature = "dialogs")]
use ncui_native::DialogBackend;
use ncui_native::dialog::MAX_CONFIRM_MESSAGE_BYTES;
use ncui_security::authz::{
    Authorizer, ConsentToken, consent_payload_digest, validate_consent_binding,
};
use serde_json::Value;

use crate::Runtime;
use crate::agent::AgentSession;

/// The title shown for every destructive-operation confirmation.
pub const CONSENT_PROMPT_TITLE: &str = "Confirm destructive operation";

/// The exact operation a host is asked to approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentRequest {
    subject: String,
    method: String,
    capabilities: CapabilitySet,
    params: Value,
    message: String,
}

impl ConsentRequest {
    fn new(
        subject: &str,
        method: &str,
        capabilities: CapabilitySet,
        params: &Value,
    ) -> Result<Self> {
        validate_consent_binding(subject, method)?;
        let operation = match params {
            Value::Null => Value::Object(serde_json::Map::new()),
            Value::Object(object) => {
                let mut object = object.clone();
                object.remove("consent_token");
                object.remove("consent");
                Value::Object(object)
            }
            _ => {
                return Err(NcError::new(ErrorKind::Validation {
                    field: "consent_token.params".to_owned(),
                    reason: "must be an object, or absent".to_owned(),
                })
                .operation("runtime::consent::request"));
            }
        };
        let rendered = serde_json::to_string_pretty(&operation).map_err(|error| {
            NcError::new(ErrorKind::Codec {
                operation: "runtime::consent::request".to_owned(),
                reason: error.to_string(),
            })
        })?;
        let message = format!(
            "Allow {subject} to run {method}?\n\nRequired capabilities: {capabilities}\n\n\
             Parameters:\n{rendered}\n\nThis action may be irreversible."
        );
        if message.len() > MAX_CONFIRM_MESSAGE_BYTES {
            return Err(NcError::new(ErrorKind::Validation {
                field: "consent_token.params".to_owned(),
                reason: format!(
                    "the complete operation is too large to show for approval; at most \
                     {MAX_CONFIRM_MESSAGE_BYTES} bytes of confirmation text are allowed"
                ),
            })
            .operation("runtime::consent::request"));
        }
        Ok(Self {
            subject: subject.to_owned(),
            method: method.to_owned(),
            capabilities,
            params: operation,
            message,
        })
    }

    /// The authenticated agent or page asking to run the operation.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// The method or application command to be approved.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The standing capabilities the operation requires.
    #[must_use]
    pub const fn capabilities(&self) -> CapabilitySet {
        self.capabilities
    }

    /// The exact normalized parameters, without consent transport fields.
    #[must_use]
    pub const fn params(&self) -> &Value {
        &self.params
    }

    /// Bounded human-readable text containing the subject, method, capabilities and parameters.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// A host surface that can obtain real human approval for one exact operation.
///
/// Returning `true` means the host actually presented the operation and the person approved
/// it. Runtime token issuance by itself is not approval; embedders must not install a prompter
/// that answers without a person unless they are explicitly modelling an already-approved host
/// action in a test.
pub trait ConsentPrompter: core::fmt::Debug + Send + Sync {
    /// Presents `request` and returns whether the person approved it.
    ///
    /// # Errors
    ///
    /// Returns an error when the prompt cannot be shown or completed. The runtime treats every
    /// error as withholding approval and never falls back to trusting caller-supplied consent.
    fn confirm(&self, request: &ConsentRequest) -> Result<bool>;
}

/// Resolves per-call consent for an agent method or native page command.
#[allow(
    clippy::too_many_arguments,
    reason = "the shared security pipeline takes every authorization input explicitly"
)]
pub(crate) fn resolve(
    runtime: &mut Runtime,
    session: &AgentSession,
    method: &str,
    required: CapabilitySet,
    destructive: bool,
    params: &Value,
    supplied_token: Option<&ConsentToken>,
    correlation: CorrelationId,
) -> Result<()> {
    session.authorize(required)?;
    let payload_digest = consent_payload_digest(params)?;
    let needs_consent = destructive || required.has_destructive();

    if let Some(token) = supplied_token {
        Authorizer.require_destructive_with_token(
            &session.agent_id,
            required,
            session.granted,
            Some(token),
            correlation,
            method,
            payload_digest,
        )?;
        if needs_consent {
            runtime.consume_consent(
                token,
                &session.agent_id,
                correlation,
                method,
                required,
                params,
            )?;
        } else {
            runtime.validate_consent(
                token,
                &session.agent_id,
                correlation,
                method,
                required,
                payload_digest,
            )?;
        }
        return Ok(());
    }

    if !needs_consent {
        return Ok(());
    }

    let request = ConsentRequest::new(&session.agent_id, method, required, params)?;
    let prompter = runtime.consent_prompter.clone().ok_or_else(|| {
        consent_denied(
            &session.agent_id,
            "runtime::consent::prompt",
            "no host approval surface is installed",
        )
    })?;
    let approved = prompter.confirm(&request).map_err(|error| {
        tracing::warn!(
            subject = %session.agent_id,
            method,
            %error,
            "the host could not obtain destructive-operation approval"
        );
        consent_denied(
            &session.agent_id,
            "runtime::consent::prompt",
            "approval failed",
        )
    })?;
    if !approved {
        return Err(consent_denied(
            &session.agent_id,
            "runtime::consent::prompt",
            "the person declined the operation",
        ));
    }

    let token = runtime.mint_consent(correlation, &session.agent_id, method, required, params)?;
    Authorizer.require_destructive_with_token(
        &session.agent_id,
        required,
        session.granted,
        Some(&token),
        correlation,
        method,
        payload_digest,
    )?;
    runtime.consume_consent(
        &token,
        &session.agent_id,
        correlation,
        method,
        required,
        params,
    )?;
    Ok(())
}

fn consent_denied(subject: &str, operation: &'static str, reason: &str) -> NcError {
    tracing::info!(%subject, %operation, reason, "destructive operation was not approved");
    NcError::new(ErrorKind::CapabilityDenied {
        capability: "consent_token".to_owned(),
        subject: subject.to_owned(),
    })
    .operation(operation)
}

/// The real native confirmation surface installed by the desktop CLI.
#[cfg(feature = "dialogs")]
#[derive(Debug)]
pub struct NativeConsentPrompter {
    dialogs: ncui_native::RfdDialogs,
}

#[cfg(feature = "dialogs")]
impl NativeConsentPrompter {
    /// Creates a prompter backed by the platform's own warning dialog.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            dialogs: ncui_native::RfdDialogs::new(),
        }
    }
}

#[cfg(feature = "dialogs")]
impl Default for NativeConsentPrompter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "dialogs")]
impl ConsentPrompter for NativeConsentPrompter {
    fn confirm(&self, request: &ConsentRequest) -> Result<bool> {
        self.dialogs
            .confirm(CONSENT_PROMPT_TITLE, request.message())
            .map_err(|error| {
                let reason = error.to_string();
                NcError::new(ErrorKind::Internal {
                    operation: "runtime::consent::native".to_owned(),
                    reason,
                })
                .operation("runtime::consent::native")
            })
    }
}
