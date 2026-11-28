//! The page-dispatch half of the window surface: serving page messages and
//! resuming calls parked at the consent gate.
//!
//! `pump_pages` is the pump-thread entry point — each page message is answered
//! through the seam here. The consent half is the part that matters for lock
//! discipline: `&mut Runtime` *is* the runtime lock in the shared-transport
//! topology, so a call whose consent needs a person is parked rather than
//! waited on (see [`Runtime::park_page_consent`]), and the worker's answer
//! settles the parked call on a later pump.

use std::sync::Arc;
use std::time::SystemTime;

use ncui_core::{CapabilitySet, CorrelationId, ErrorKind, NcError};
use ncui_security::audit::{AuditEvent, AuditOutcome};
use ncui_security::authz::ConsentToken;
use ncui_webview::{HostSeam, PageTarget, SeamError};

use crate::Runtime;
use crate::commands::CommandTarget;

impl Runtime {
    pub fn pump_pages(&mut self, now: core::time::Duration) -> usize {
        // Consent answers land between pumps: a worker asks the person with no
        // lock held and posts the outcome for the next pump to settle.
        self.settle_consent_answers(now);
        let mut handled = 0usize;
        while let Ok(Some((target, body))) = self.windows.take_page_target_message() {
            handled += 1;
            // Supersede semantics for interaction-rate command streams. A scrub
            // produces playhead positions faster than a busy engine can apply
            // them, and every position but the newest is stale the moment a
            // newer one exists — executing it anyway is wasted work and visible
            // jitter. A Perform whose wire message carries a `supersede` key is
            // answered `superseded` without executing when a newer message with
            // the same key is already queued behind it from the same page. The
            // answer settles the page's promise truthfully: the outcome says
            // what happened, and the alternative — silence — would cost the
            // page a 30 s bridge timeout per superseded call.
            if let PageTarget::Window(window) = target
                && let Some((id, key)) = probe_supersede_key(&body)
            {
                let drained = self.windows.drain_window_page_messages(window);
                let stale = drained.iter().any(|message| {
                    probe_supersede_key(&message.body).is_some_and(|(_, other)| other == key)
                });
                self.windows.requeue_window_page_messages(window, drained);
                if stale {
                    let response = ncui_webview::SeamResponse::ok("superseded", None).with_id(id);
                    self.send_seam_response(target, &response);
                    continue;
                }
            }
            self.serve_page_message(target, &body, now);
        }
        handled
    }

    /// Evaluates one message from one page and answers it.
    ///
    /// Two messages are not answered here and are handled before the seam's dispatcher runs:
    /// a page-handled command, which is *started* (its answer comes back later), and a
    /// handler's answer, which *settles* a call the runtime started earlier. Everything else
    /// is dispatched and answered as it always was.
    fn serve_page_message(&mut self, target: PageTarget, body: &str, now: core::time::Duration) {
        let Ok(Some(seam)) = self.windows.seam_for_target(target) else {
            return;
        };
        let Ok(Some(granted)) = self.windows.grant_for_target(target) else {
            return;
        };
        let request = match ncui_webview::SeamRequest::parse(body) {
            Ok(request) => request,
            Err(refusal) => {
                self.send_seam_response(target, &refusal);
                return;
            }
        };
        // The page's answer to a handler the runtime handed it: settle the call it completes.
        if let ncui_webview::SeamMessage::CommandResult { ok, payload, error } = &request.message {
            self.complete_page_handler(target, *ok, &request.id, payload.clone(), error.clone());
            return;
        }
        // A page-handled command is started here and answered when the page posts its result.
        if self.try_begin_page_handler(target, &seam, &request) {
            return;
        }
        let mut response = seam.handle(&request.message, &mut |request, payload| {
            self.dispatch_page_action(target, request, payload, granted, now)
        });
        if let Some(key) = self.deferred_page_call.take() {
            // The call was parked at the consent gate; its answer leaves when
            // the person's decision lands, under the id the page is waiting on.
            // The sentinel response `handle` wrapped is never sent.
            if let Some(pending) = self.pending_page_consents.get_mut(&key) {
                pending.request_id = Some(request.id.clone());
            }
            return;
        }
        response.id = Some(request.id);
        self.send_seam_response(target, &response);
    }

    /// Sends a seam response to a window's page under the correlation id it carries.
    fn send_seam_response(&mut self, target: PageTarget, response: &ncui_webview::SeamResponse) {
        // The answer is embedded as a JSON object literal. JSON escaping is a superset of
        // what a JavaScript string literal needs, and the bridge reads the object directly.
        let Ok(encoded) = serde_json::to_string(&response) else {
            tracing::warn!(?target, "the bridge response could not be serialised");
            return;
        };
        let script = format!("window.ncui && window.ncui.__dispatch({encoded});");
        if !self
            .windows
            .post_to_target(target, &script)
            .unwrap_or(false)
        {
            tracing::warn!(
                ?target,
                "the bridge response could not be delivered to the page"
            );
        }
    }

    /// Starts a page-handled command: gate it, remember the call, and hand it to the page.
    ///
    /// Returns `true` when the message was a page-handled command and has been dealt with —
    /// either started (its answer comes back later) or refused (the reply is already sent) —
    /// so the caller sends nothing further. Returns `false` when the message is some other
    /// action the caller should dispatch and answer as usual.
    fn try_begin_page_handler(
        &mut self,
        target: PageTarget,
        seam: &Arc<HostSeam>,
        request: &ncui_webview::SeamRequest,
    ) -> bool {
        let ncui_webview::SeamMessage::Perform { name, payload } = &request.message else {
            return false;
        };
        let Some(command) = self.page_commands.get(name) else {
            return false;
        };
        if !matches!(command.target, CommandTarget::Page) {
            return false;
        }
        // The same gate the seam applies to every action, applied here because a page command
        // is started rather than run. Every refusal below is answered under the ORIGINAL id, so
        // the page's own invoke settles (with the error) instead of hanging until it times out.
        if let Err(error) = seam.authorize_perform(name) {
            let mut refusal = ncui_webview::SeamResponse::failure(error);
            refusal.id = Some(request.id.clone());
            self.send_seam_response(target, &refusal);
            return true;
        }
        let Some(reply_id) = self.page_commands.begin_page_call(target, &request.id) else {
            let mut refusal = ncui_webview::SeamResponse::failure(SeamError::new(
                "busy",
                "too many page-handled calls are awaiting an answer",
            ));
            refusal.id = Some(request.id.clone());
            self.send_seam_response(target, &refusal);
            return true;
        };
        // Hand the command to the page: the bridge runs the registered handler and posts the
        // result back under `reply_id`, which is what settles the original `invoke`.
        let script = format!(
            "window.ncui && window.ncui.__runCommand({},{},{});",
            serde_json::to_string(&reply_id).unwrap_or_default(),
            serde_json::to_string(name).unwrap_or_default(),
            payload,
        );
        if !self
            .windows
            .post_to_target(target, &script)
            .unwrap_or(false)
        {
            // The page could not be reached to run the handler, so the call will never be
            // answered: take it back and refuse the original `invoke` rather than let it hang.
            let _ = self.page_commands.take_page_call(&reply_id, target);
            let mut refusal = ncui_webview::SeamResponse::failure(SeamError::unavailable(
                "the page could not be reached to run its own handler",
            ));
            refusal.id = Some(request.id.clone());
            self.send_seam_response(target, &refusal);
            return true;
        }
        true
    }

    /// Settles the original `invoke` a page-handled command produced, with the handler's answer.
    fn complete_page_handler(
        &mut self,
        target: PageTarget,
        ok: bool,
        reply_id: &str,
        payload: serde_json::Value,
        error: Option<SeamError>,
    ) {
        let Some(pending) = self.page_commands.take_page_call(reply_id, target) else {
            // Not a reply to a call this runtime started (or already settled). Nothing is
            // waiting on it, so there is nothing to answer — the bridge drops an id it has
            // no pending promise for.
            return;
        };
        let mut response = if ok {
            ncui_webview::SeamResponse::ok("performed", Some(payload))
        } else {
            ncui_webview::SeamResponse::failure(error.unwrap_or_else(|| {
                SeamError::dispatch("the page's handler failed without a reason")
            }))
        };
        // The answer settles the *original* invoke, not the reply id: the bridge matches on
        // the id the page minted when it called, which is what the runtime is holding.
        response.id = Some(pending.original_id);
        self.send_seam_response(pending.target, &response);
    }

    /// Dispatches a page's action through the runtime's own pipeline.
    ///
    /// Consent is decided before the action's body runs because the prompt is a
    /// human wait, and this dispatcher runs under `&mut Runtime` — the runtime
    /// lock in the shared-transport topology. A call that would reach the prompt
    /// is parked by [`Runtime::park_page_consent`] and resumed by
    /// [`Runtime::settle_page_consent`], so the person is never waited on with
    /// the lock held; everything else dispatches in place.
    fn dispatch_page_action(
        &mut self,
        target: PageTarget,
        request: &ncui_webview::CapabilityRequest,
        payload: serde_json::Value,
        granted: CapabilitySet,
        now: core::time::Duration,
    ) -> Result<serde_json::Value, SeamError> {
        let Some(command) = self.page_commands.get(&request.action) else {
            // The seam already refuses an undeclared action; this arm exists so that a
            // mismatch between the seam's declarations and the runtime's table is a
            // refusal with a reason rather than a dispatch to a method nobody chose.
            return Err(SeamError::capability_denied(format!(
                "`{}` is not an action this host offers",
                request.action
            )));
        };
        if !payload.is_null() && !payload.is_object() {
            return Err(SeamError::new(
                "invalid_params",
                "an action payload must be a JSON object",
            ));
        }
        let mut params = if payload.is_null() {
            serde_json::json!({})
        } else {
            payload
        };
        if command.caller_window {
            if target.is_child() {
                return Err(SeamError::capability_denied(
                    "a child page cannot act on its parent window",
                ));
            }
            // The page's own window, taken from the call's origin. A page that could name
            // a window could close one it does not own.
            if let Some(object) = params.as_object_mut() {
                object.insert(
                    "window".to_owned(),
                    serde_json::Value::String(target.parent().to_string()),
                );
            }
        }
        // The consent binding depends on where the work runs: a runtime method
        // binds under its own name and its table authority, a native command
        // under the action's declared requirements.
        let (consent_method, consent_required, consent_destructive, confirmation) =
            match &command.target {
                CommandTarget::RuntimeMethod(method) => {
                    let Some(description) = crate::dispatch::method_table_cached()
                        .iter()
                        .find(|entry| entry.name == *method)
                    else {
                        return Err(SeamError::capability_denied(format!(
                            "`{method}` is not a method this runtime offers"
                        )));
                    };
                    (
                        (*method).to_owned(),
                        description.requires,
                        description.destructive,
                        None,
                    )
                }
                CommandTarget::Native(_) => (
                    request.action.clone(),
                    command.requires,
                    command.requires.has_destructive(),
                    self.windows.confirmation_for(target.parent()),
                ),
                // A page command is intercepted in `serve_page_message` before this dispatcher
                // runs, because its answer comes back from the page over a second message rather
                // than as this call's return value. Reaching here means that interception was
                // bypassed, so the honest answer is a refusal rather than a fabricated result.
                CommandTarget::Page => {
                    return Err(SeamError::dispatch(
                        "a page-handled command is answered by the page and cannot be run here",
                    ));
                }
            };
        let session = crate::agent::AgentSession::for_page(
            format!("page:{target}"),
            granted,
            self.page_limiter(),
        )
        .map_err(|error| seam_error(&error))?;
        match crate::consent::gate(
            self,
            &session,
            &consent_method,
            consent_required,
            consent_destructive,
            &params,
            now,
            confirmation,
        ) {
            crate::consent::ConsentGate::Dispatch => self.dispatch_page_action_inner(
                target,
                &command,
                &request.action,
                params,
                now,
                session,
                None,
            ),
            crate::consent::ConsentGate::Ask {
                request: approval,
                required,
                surface,
            } => self.park_page_consent(
                target,
                &request.action,
                required,
                params,
                now,
                session,
                approval,
                surface,
            ),
        }
    }

    /// Runs a page action's body after its consent position is known.
    ///
    /// `pre` is `None` for a call that resolves consent in place — the shape
    /// every call had before consent moved off the lock — and `Some` for a call
    /// resuming from the park, carrying the answer an off-lock prompt produced.
    #[allow(
        clippy::too_many_arguments,
        reason = "the call's own inputs plus its parked consent state"
    )]
    fn dispatch_page_action_inner(
        &mut self,
        target: PageTarget,
        command: &crate::commands::PageCommand,
        action: &str,
        params: serde_json::Value,
        now: core::time::Duration,
        mut session: crate::agent::AgentSession,
        pre: Option<ncui_core::Result<ConsentToken>>,
    ) -> Result<serde_json::Value, SeamError> {
        match &command.target {
            CommandTarget::RuntimeMethod(method) => {
                let call = crate::rpc::Request::new(serde_json::Value::Null, *method, params);
                match crate::dispatch::dispatch_with_consent(
                    self,
                    &mut session,
                    &call,
                    now,
                    None,
                    crate::dispatch::method_table_cached(),
                    pre,
                ) {
                    Some(response) => match response.result {
                        Some(value) => Ok(value),
                        None => match response.error {
                            Some(error) => Err(SeamError {
                                code: error.data.ncui_code,
                                message: error.message,
                                retryable: error.data.retryable,
                            }),
                            None => Err(SeamError::dispatch("the action produced no result")),
                        },
                    },
                    None => Ok(serde_json::Value::Null),
                }
            }
            CommandTarget::Native(handler) => self.run_native_page_command(PageCall {
                target,
                name: action,
                requires: command.requires,
                handler: Arc::clone(handler),
                payload: params,
                now,
                session,
                pre,
            }),
            CommandTarget::Page => Err(SeamError::dispatch(
                "a page-handled command is answered by the page and cannot be run here",
            )),
        }
    }

    /// Parks a page call whose consent needs a person: a worker asks with no
    /// runtime lock held, and the call resumes when the pump drains the answer.
    ///
    /// The sentinel error is recognised by `serve_page_message`, which withholds
    /// the response — the answer leaves when the person's decision lands, under
    /// the id the page is waiting on. It never reaches the bridge.
    #[allow(
        clippy::too_many_arguments,
        reason = "the parked call is the call's whole context; bundling earlier would hide it"
    )]
    fn park_page_consent(
        &mut self,
        target: PageTarget,
        action: &str,
        required: CapabilitySet,
        params: serde_json::Value,
        now: core::time::Duration,
        session: crate::agent::AgentSession,
        approval: crate::consent::ConsentRequest,
        surface: crate::consent::ConsentSurface,
    ) -> Result<serde_json::Value, SeamError> {
        if self.pending_page_consents.len() >= MAX_PENDING_PAGE_CONSENTS {
            return Err(SeamError::new(
                "busy",
                "too many approvals are waiting for an answer",
            ));
        }
        let key = self.next_pending_consent;
        self.next_pending_consent += 1;
        self.pending_page_consents.insert(
            key,
            PendingPageConsent {
                target,
                action: action.to_owned(),
                required,
                payload: params,
                session,
                request_id: None,
                parked_at: now,
            },
        );
        let answers = Arc::clone(&self.consent_answers);
        if let Err(error) = std::thread::Builder::new()
            .name(format!("ncui-consent-{key}"))
            .spawn(move || {
                let answer = surface.ask(&approval);
                answers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push_back((key, answer));
            })
        {
            // A worker that could not start asks no one: take the park back and
            // refuse, so the call does not wait on a prompt that does not exist.
            let _ = self.pending_page_consents.remove(&key);
            return Err(SeamError::unavailable(format!(
                "the consent prompt could not be started: {error}"
            )));
        }
        self.deferred_page_call = Some(key);
        Err(SeamError::new(
            "consent_pending",
            "the operation is waiting for the person's approval",
        ))
    }

    /// Settles every consent answer the prompt workers produced since the last
    /// pump, then expires parks that outlived the bridge's patience.
    fn settle_consent_answers(&mut self, now: core::time::Duration) {
        loop {
            let next = self
                .consent_answers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front();
            let Some((key, approved)) = next else {
                break;
            };
            self.settle_page_consent(key, approved, now);
        }
        let stale: Vec<u64> = self
            .pending_page_consents
            .iter()
            .filter(|(_, pending)| now.saturating_sub(pending.parked_at) > PENDING_CONSENT_TTL)
            .map(|(key, _)| *key)
            .collect();
        for key in stale {
            // A prompt the person never answers parks only the entry — the
            // worker still waits — but the bridge's caller gave up long ago, so
            // the refusal is what the entry owes it. The worker's late answer
            // lands on a missing key and is dropped.
            self.settle_page_consent(
                key,
                Err(NcError::new(ErrorKind::Internal {
                    operation: "runtime::consent::expired".to_owned(),
                    reason: "the approval wait outlived the bridge's patience".to_owned(),
                })
                .operation("runtime::consent::expired")),
                now,
            );
        }
    }

    /// Resumes a page call parked at the consent gate.
    ///
    /// The answer is the only input the park did not already hold: an approval
    /// becomes the token the call consumes when it runs, and every other outcome
    /// is the refusal that answers the page. The call itself re-enters the same
    /// dispatch it would have run — rate limit, capability check, audit —
    /// because consent is the only step the park skipped.
    fn settle_page_consent(
        &mut self,
        key: u64,
        approved: ncui_core::Result<bool>,
        now: core::time::Duration,
    ) {
        let Some(pending) = self.pending_page_consents.remove(&key) else {
            return;
        };
        let PendingPageConsent {
            target,
            action,
            required,
            payload,
            session,
            request_id,
            parked_at: _,
        } = pending;
        // The registration is read again rather than held: commands can change
        // while a call is parked, and the live entry is the honest authority.
        let Some(command) = self.page_commands.get(&action) else {
            let mut refusal = ncui_webview::SeamResponse::failure(SeamError::capability_denied(
                format!("`{action}` is not an action this host offers"),
            ));
            refusal.id = request_id;
            self.send_seam_response(target, &refusal);
            return;
        };
        let consent_method = match &command.target {
            CommandTarget::RuntimeMethod(method) => (*method).to_owned(),
            _ => action.clone(),
        };
        let pre = crate::consent::settle(
            self,
            &session,
            &consent_method,
            required,
            &payload,
            approved,
        );
        let outcome = self.dispatch_page_action_inner(
            target,
            &command,
            &action,
            payload,
            now,
            session,
            Some(pre),
        );
        let mut response = match outcome {
            Ok(value) => ncui_webview::SeamResponse::ok("performed", Some(value)),
            Err(error) => ncui_webview::SeamResponse::failure(error),
        };
        response.id = request_id;
        self.send_seam_response(target, &response);
    }

    /// Runs an application command through the same checks a runtime method receives.
    fn run_native_page_command(
        &mut self,
        call: PageCall<'_>,
    ) -> Result<serde_json::Value, SeamError> {
        let PageCall {
            target,
            name,
            requires,
            handler,
            payload,
            now,
            mut session,
            pre,
        } = call;
        let mut correlation = CorrelationId::new();
        let outcome = (|| -> Result<serde_json::Value, SeamError> {
            session
                .rate_limit(now, 1)
                .map_err(|error| seam_error(&error))?;
            let consent_token = crate::dispatch::consent_token_param(&payload)
                .map_err(|error| seam_error(&error))?;
            if let Some(token) = &consent_token {
                correlation = token.correlation();
            }
            // A token minted from an out-of-lock prompt carries its own
            // correlation, for the same reason a supplied token's does.
            if let Some(Ok(token)) = &pre {
                correlation = token.correlation();
            }
            let confirmation = self.windows.confirmation_for(target.parent());
            crate::consent::resolve(
                self,
                &session,
                name,
                requires,
                requires.has_destructive(),
                &payload,
                consent_token.as_ref(),
                pre,
                confirmation.as_deref(),
                correlation,
            )
            .map_err(|error| seam_error(&error))?;
            handler(payload)
        })();
        let audit_outcome = match &outcome {
            Ok(_) => AuditOutcome::Allowed,
            Err(error) if error.code == "capability_denied" || error.code == "limit_exceeded" => {
                AuditOutcome::Denied
            }
            Err(_) => AuditOutcome::Failed,
        };
        if let Ok(event) = AuditEvent::new(
            SystemTime::now(),
            &session.agent_id,
            correlation,
            format!("page.{name}"),
            name,
            audit_outcome,
            requires,
        ) {
            self.audit.record(event);
        }
        outcome
    }
}

/// How many page calls may be parked on unanswered consent prompts at once.
///
/// A prompt is a person's attention, which serializes naturally — the bound
/// exists so a page cannot grow the map by asking faster than anyone could
/// ever answer. Past it, further consent-bearing calls are refused busy.
const MAX_PENDING_PAGE_CONSENTS: usize = 8;

/// How long a parked call waits for the person before the park itself refuses.
///
/// Past the bridge's own patience the page is no longer waiting, so holding the
/// slot is the only thing left to stop. The worker still waits; its late answer
/// is dropped because nothing remains to settle.
const PENDING_CONSENT_TTL: core::time::Duration = core::time::Duration::from_secs(120);

/// A page call parked while a person answers its consent prompt off-lock.
pub(crate) struct PendingPageConsent {
    /// The window or child whose page made the call.
    target: PageTarget,
    /// The action the page performed.
    action: String,
    /// What the action requires — the consent binding's capability set.
    required: CapabilitySet,
    /// The normalized params: the digest's input and the handler's payload.
    payload: serde_json::Value,
    /// The session the call's gate ran under, resumed at settle.
    session: crate::agent::AgentSession,
    /// The page's request id, attached when the deferred response is withheld.
    request_id: Option<String>,
    /// When the call was parked, on the pump's clock — expiry reads it against
    /// the same clock, so a test's frozen `now` expires calls deterministically.
    parked_at: core::time::Duration,
}

/// One page command call, gathered so the execution path takes one argument.
struct PageCall<'a> {
    /// The window or child whose page made the call.
    target: PageTarget,
    /// The command name.
    name: &'a str,
    /// What the command requires of the page.
    requires: CapabilitySet,
    /// The command body.
    handler: crate::commands::CommandHandler,
    /// The page's payload.
    payload: serde_json::Value,
    /// The clock the rate limiter reads.
    now: core::time::Duration,
    /// The call's session — the one the consent gate ran under for a resumed
    /// call, or a fresh one for a call dispatching in place.
    session: crate::agent::AgentSession,
    /// The answer an off-lock prompt already produced, when this call resumes
    /// from the consent park rather than resolving consent in place.
    pre: Option<ncui_core::Result<ConsentToken>>,
}

fn seam_error(error: &NcError) -> SeamError {
    SeamError {
        code: error.code().to_owned(),
        message: error.to_string(),
        retryable: error.is_retryable(),
    }
}

/// The supersede key a wire message carries, if it is a keyed Perform.
///
/// The probe reads the raw page JSON rather than the seam's types on purpose:
/// the supersede decision belongs to the pump, which answers stale commands
/// before the seam ever sees them, and the seam's own parse is unaffected —
/// the key is transport metadata, not part of the command. A missing or
/// malformed key means the message does not participate, which is the correct
/// default: supersede is opt-in per call because only the caller knows which
/// of its commands form a latest-wins stream.
///
/// The key is bounded for the same reason the request id is: it is echoed into
/// scans on every pump, and an unbounded attacker-chosen string would tax every
/// message behind it. 256 ASCII bytes is generous for `playhead` or
/// `trim:clip-7` and small enough that the scan is never the cost center.
const MAX_SUPERSEDE_KEY_BYTES: usize = 256;

pub(super) fn probe_supersede_key(body: &str) -> Option<(String, String)> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    if value.get("action").and_then(serde_json::Value::as_str) != Some("perform") {
        return None;
    }
    let key = value.get("supersede").and_then(serde_json::Value::as_str)?;
    if key.is_empty()
        || key.len() > MAX_SUPERSEDE_KEY_BYTES
        || !key.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return None;
    }
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)?
        .to_owned();
    Some((id, key.to_owned()))
}
