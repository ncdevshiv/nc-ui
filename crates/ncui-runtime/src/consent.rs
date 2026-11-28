//! Host approval for destructive agent and page operations.
//!
//! A page or remote agent cannot mint its own approval. The runtime either validates a
//! host-issued, subject- and argument-bound token or asks the installed host prompter after
//! authentication, rate limiting and standing authorization have succeeded.

use std::sync::Arc;

use ncui_core::{CapabilitySet, CorrelationId, ErrorKind, NcError, Result};
#[cfg(feature = "dialogs")]
use ncui_native::DialogBackend;
use ncui_native::dialog::MAX_CONFIRM_MESSAGE_BYTES;
use ncui_security::authz::{
    Authorizer, ConsentToken, consent_payload_digest, validate_consent_binding,
};
use ncui_webview::WindowConfirmation;
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

/// What the caller must do about per-call consent before dispatching.
///
/// The human half of consent is unbounded — a person reading a dialog takes as
/// long as they take. `Ask` hands the caller everything needed to wait for that
/// answer with no runtime lock held: the prepared request and a surface that
/// owns its handles. `&mut Runtime` is parked only for the two bookends —
/// deciding in [`gate`] and consuming in [`settle`] — never for the wait
/// between them.
pub(crate) enum ConsentGate {
    /// Dispatch normally: this call reaches no prompt. It needs no consent, the
    /// caller supplied a token, or the pipeline itself produces the refusal the
    /// gate saw coming — an ungranted capability, a malformed token, a request
    /// too large to present, or no installed approval surface. Routing those
    /// through the pipeline keeps one implementation of every refusal.
    Dispatch,
    /// Ask `surface` for `request` with no runtime lock held, then re-enter
    /// dispatch with the answer [`settle`] produces.
    Ask {
        /// The exact operation the person is shown.
        request: ConsentRequest,
        /// The capabilities the method requires; [`settle`] binds them into the mint.
        required: CapabilitySet,
        /// The surface to ask through.
        surface: ConsentSurface,
    },
}

/// The prompt surface, held in a form that asks without the runtime lock.
///
/// Both variants own what they need — the prompter behind an `Arc`, the
/// window's parented confirmation behind its `Box` — so neither borrows the
/// runtime. That is the whole reason the gate exists.
pub(crate) enum ConsentSurface {
    /// The installed host prompter: the embedder's own boundary, or the
    /// runtime-default native one.
    Prompter(Arc<dyn ConsentPrompter>),
    /// The asking window's own parented confirmation.
    Windowed(Box<dyn WindowConfirmation>),
}

impl ConsentSurface {
    /// Presents `request` and blocks on the person's answer.
    ///
    /// Called with no runtime lock held; the answer is delivered to [`settle`].
    /// An error means the prompt could not be shown or completed, which
    /// withholds approval exactly as the prompt paths inside [`resolve`] treat it.
    pub(crate) fn ask(&self, request: &ConsentRequest) -> Result<bool> {
        match self {
            Self::Prompter(prompter) => prompter.confirm(request),
            Self::Windowed(confirmation) => confirmation
                .confirm(CONSENT_PROMPT_TITLE, request.message())
                .map_err(|reason| {
                    NcError::new(ErrorKind::Internal {
                        operation: "runtime::consent::window".to_owned(),
                        reason,
                    })
                    .operation("runtime::consent::window")
                }),
        }
    }
}

/// Decides whether dispatching this call would reach the human prompt.
///
/// The answer must be taken under the runtime lock — it reads the installed
/// prompter and the caller's authorization — but it changes nothing, so the
/// lock is held only for the read. The order of checks mirrors [`resolve`]'s
/// path to the prompt, so `Ask` is exactly the set of calls that would block
/// on a person if dispatched in place.
///
/// `window_confirmation` is the asking window's parented surface, for the page
/// path; agent transport calls pass `None`.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors resolve's inputs so the gate predicts exactly what resolve would do"
)]
pub(crate) fn gate(
    runtime: &Runtime,
    session: &AgentSession,
    method: &str,
    required: CapabilitySet,
    destructive: bool,
    params: &Value,
    now: core::time::Duration,
    window_confirmation: Option<Box<dyn WindowConfirmation>>,
) -> ConsentGate {
    if !(destructive || required.has_destructive()) {
        return ConsentGate::Dispatch;
    }
    match crate::dispatch::consent_token_param(params) {
        // A supplied token takes the pipeline's validation path and a malformed
        // one takes its parse error — the prompt is involved in neither.
        Ok(Some(_)) | Err(_) => return ConsentGate::Dispatch,
        Ok(None) => {}
    }
    // Rate limiting precedes consent in the pipeline; a call the bucket would
    // refuse is refused there, not asked about. `available` checks without
    // spending — the spend stays the pipeline's own, in order.
    if session.available(now) == 0 {
        return ConsentGate::Dispatch;
    }
    // Standing authorization precedes the prompt in `resolve`; a call it
    // refuses is refused there, not asked about.
    if session.authorize(required).is_err() {
        return ConsentGate::Dispatch;
    }
    let Ok(request) = ConsentRequest::new(&session.agent_id, method, required, params) else {
        return ConsentGate::Dispatch;
    };
    // The surface ordering is `obtain_human_approval`'s: the embedder's own
    // boundary is asked first, always; the window's parented surface outranks
    // only the runtime-default native prompt; then the installed prompter.
    // No surface at all fails closed in the pipeline, which is where that
    // refusal stays.
    let surface = if runtime.consent_prompter_custom {
        runtime
            .consent_prompter
            .clone()
            .map(ConsentSurface::Prompter)
    } else if let (Some(confirmation), true) =
        (window_confirmation, runtime.consent_prompter.is_some())
    {
        Some(ConsentSurface::Windowed(confirmation))
    } else {
        runtime
            .consent_prompter
            .clone()
            .map(ConsentSurface::Prompter)
    };
    match surface {
        Some(surface) => ConsentGate::Ask {
            request,
            required,
            surface,
        },
        None => ConsentGate::Dispatch,
    }
}

/// The transport's gate entry point: the method's authority comes from `table`,
/// so a name the table does not know takes the pipeline's own not-found.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors resolve's inputs so the gate predicts exactly what resolve would do"
)]
pub(crate) fn gate_for_method(
    runtime: &Runtime,
    session: &AgentSession,
    request: &crate::rpc::Request,
    table: &[crate::describe::MethodDescription],
    now: core::time::Duration,
    window_confirmation: Option<Box<dyn WindowConfirmation>>,
) -> ConsentGate {
    let Some(method) = table.iter().find(|m| m.name == request.method) else {
        return ConsentGate::Dispatch;
    };
    gate(
        runtime,
        session,
        &method.name,
        method.requires,
        method.destructive,
        &request.params,
        now,
        window_confirmation,
    )
}

/// Turns the answer a caller obtained outside the lock into what the pipeline
/// consumes: a freshly minted token on approval, or the refusal a declined or
/// failed prompt produces.
///
/// The outcomes are identical to resolving the prompt inside [`resolve`],
/// because the answer is the only thing that moved — who waited for it.
pub(crate) fn settle(
    runtime: &mut Runtime,
    session: &AgentSession,
    method: &str,
    required: CapabilitySet,
    params: &Value,
    approved: Result<bool>,
) -> Result<ConsentToken> {
    match approved {
        Ok(true) => runtime.mint_consent(
            CorrelationId::new(),
            &session.agent_id,
            method,
            required,
            params,
        ),
        Ok(false) => Err(consent_denied(
            &session.agent_id,
            "runtime::consent::prompt",
            "the person declined the operation",
        )),
        Err(error) => {
            tracing::warn!(
                subject = %session.agent_id,
                method,
                %error,
                "the host could not obtain destructive-operation approval"
            );
            Err(consent_denied(
                &session.agent_id,
                "runtime::consent::prompt",
                "approval failed",
            ))
        }
    }
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
    pre_approved: Option<Result<ConsentToken>>,
    window_confirmation: Option<&dyn WindowConfirmation>,
    correlation: CorrelationId,
) -> Result<()> {
    session.authorize(required)?;
    let payload_digest = consent_payload_digest(params)?;
    let needs_consent = destructive || required.has_destructive();

    // A caller that already ran the prompt outside the lock hands its answer in
    // as a minted token, or as the refusal a declined or failed prompt produced.
    // Either takes the supplied-token path, so an out-of-lock answer and a
    // caller-supplied token are indistinguishable to validation.
    let minted = match pre_approved {
        Some(pre) => Some(pre?),
        None => None,
    };
    let supplied_token = minted.as_ref().or(supplied_token);
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
    let approved = obtain_human_approval(runtime, session, method, &request, window_confirmation)?;
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

/// Obtains the person's decision for one exact operation.
///
/// A window that can parent its prompt is asked through its own confirmation surface, and
/// its failure is final: falling back to an unparented prompt would quietly re-create the
/// orphaned dialog this path exists to remove. Without such a surface the installed host
/// prompter is the approval boundary.
fn obtain_human_approval(
    runtime: &Runtime,
    session: &AgentSession,
    method: &str,
    request: &ConsentRequest,
    window_confirmation: Option<&dyn WindowConfirmation>,
) -> Result<bool> {
    // An embedder's own boundary is asked first, always: a test or embedding that
    // substitutes the prompter expects the substitute to decide, and a window that
    // could parent its own prompt must not bypass that substitution. The window's
    // parented surface is preferred over only the runtime-default native prompt,
    // whose unparented dialog is the orphaned-window failure this ordering removes.
    if runtime.consent_prompter_custom {
        let prompter = runtime.consent_prompter.clone().ok_or_else(|| {
            consent_denied(
                &session.agent_id,
                "runtime::consent::prompt",
                "no host approval surface is installed",
            )
        })?;
        return prompter.confirm(request).map_err(|error| {
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
        });
    }
    // The parented surface is the better form of the runtime-default native prompt, so it
    // is asked only when that default is installed. A host that installs no approval
    // surface at all must fail closed: inventing a prompt the host never asked for would
    // turn "no consent boundary" into "the runtime consents on the host's behalf".
    if let (Some(confirmation), true) = (window_confirmation, runtime.consent_prompter.is_some()) {
        return confirmation
            .confirm(CONSENT_PROMPT_TITLE, request.message())
            .map_err(|error| {
                tracing::warn!(
                    subject = %session.agent_id,
                    method,
                    %error,
                    "the asking window could not obtain destructive-operation approval"
                );
                consent_denied(
                    &session.agent_id,
                    "runtime::consent::prompt",
                    "approval failed",
                )
            });
    }
    let prompter = runtime.consent_prompter.clone().ok_or_else(|| {
        consent_denied(
            &session.agent_id,
            "runtime::consent::prompt",
            "no host approval surface is installed",
        )
    })?;
    prompter.confirm(request).map_err(|error| {
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
    })
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
