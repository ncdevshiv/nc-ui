//! The seam between page content and the NC-UI runtime.
//!
//! Every message here originates in a document the runtime does not control, so the seam is
//! written to be **total** and **narrow**:
//!
//! * Total — every input produces a [`SeamResponse`]. Nothing panics, nothing blocks,
//!   nothing allocates on a length the page chose. A page is a hostile input source.
//! * Narrow — the page cannot name a runtime method. It names an *action*, and the seam
//!   decides what that action requires. If the page could dispatch by method name it would be
//!   the JSON-RPC agent surface with the capability checks removed.
//!
//! The pipeline is the runtime's, not this crate's: `handle` takes a dispatcher closure, so
//! the same capability check, consent gate and audit trail apply that any other caller gets.
//! That is what ADR-001's "narrow, documented boundary" means in practice.
//!
//! Every page request carries an opaque `id` minted by the bridge. The host echoes it
//! unchanged, so a page can have several requests in flight and match each answer to the call
//! that produced it. The id is data only: it never selects a handler.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use ncui_core::CapabilitySet;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::capability::{CapabilityGrant, CapabilityRequest};
use crate::lifecycle::WindowLifecycle;
use crate::navigation::{NavigationPolicy, NavigationRequest, Verdict};

/// The bridge protocol version. A page reads this from `describe`.
pub const BRIDGE_PROTOCOL_VERSION: u32 = 1;

/// The longest request id the bridge accepts.
pub const MAX_REQUEST_ID_BYTES: usize = 128;

/// A message from page content to the runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SeamMessage {
    /// Ask the runtime to do something.
    ///
    /// The field is `name` rather than `action` because the enum is internally tagged on
    /// `action`; a variant field cannot share the tag's name.
    Perform {
        /// Which action the page wants.
        name: String,
        /// The action's arguments. Null when the page supplied none.
        #[serde(default)]
        payload: Value,
    },
    /// Ask whether a navigation would be permitted, without performing it.
    ///
    /// Useful for a page that wants to grey out a link it cannot follow. The answer is the
    /// same verdict the engine would get, because it is the same policy.
    ProbeNavigation {
        /// The origin the page wants to reach.
        url: String,
    },
    /// Ask what the page is permitted to do.
    Describe,
}

/// A page request with the correlation id the bridge minted for it.
#[derive(Debug, Clone, PartialEq)]
pub struct SeamRequest {
    /// Opaque caller-minted id, echoed on the response.
    pub id: String,
    /// The request itself.
    pub message: SeamMessage,
}

impl SeamRequest {
    /// Parses a request from page JSON.
    ///
    /// The id is read before the action so a malformed action can still be answered under
    /// the id the page used. An absent or invalid id cannot be echoed, because echoing an
    /// attacker-chosen unbounded string would put it in every log and audit record.
    pub fn parse(raw: &str) -> Result<Self, SeamResponse> {
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            return Err(SeamResponse::malformed("the message is not valid JSON"));
        };
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            return Err(SeamResponse::malformed("the message has no `id`"));
        };
        if !valid_request_id(id) {
            return Err(SeamResponse::malformed(format!(
                "`id` must be 1..={MAX_REQUEST_ID_BYTES} ASCII letters, digits, `-`, `_`, `.`, or `:`"
            )));
        }
        let id = id.to_owned();
        match SeamMessage::parse_value(&value) {
            Ok(message) => Ok(Self { id, message }),
            Err(mut response) => {
                response.id = Some(id);
                Err(response)
            }
        }
    }
}

impl SeamMessage {
    /// Parses a message from page JSON without requiring a correlation id.
    ///
    /// This is the parser for callers that own the request lifecycle themselves. The page
    /// bridge uses [`SeamRequest::parse`] because a page request without an id cannot be
    /// answered unambiguously.
    pub fn parse(raw: &str) -> Result<Self, SeamResponse> {
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            return Err(SeamResponse::malformed("the message is not valid JSON"));
        };
        Self::parse_value(&value)
    }

    fn parse_value(value: &Value) -> Result<Self, SeamResponse> {
        let Some(action) = value.get("action").and_then(Value::as_str) else {
            return Err(SeamResponse::malformed("the message has no `action`"));
        };
        match action {
            "perform" => {
                let Some(name) = value.get("name").and_then(Value::as_str) else {
                    return Err(SeamResponse::malformed("`perform` needs a `name`"));
                };
                Ok(Self::Perform {
                    name: name.to_owned(),
                    payload: value.get("payload").cloned().unwrap_or(Value::Null),
                })
            }
            "probe_navigation" => value
                .get("url")
                .and_then(Value::as_str)
                .map(|url| Self::ProbeNavigation {
                    url: url.to_owned(),
                })
                .ok_or_else(|| SeamResponse::malformed("`probe_navigation` needs a `url`")),
            "describe" => Ok(Self::Describe),
            other => Err(SeamResponse::malformed(format!("unknown action `{other}`"))),
        }
    }
}

fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REQUEST_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

/// A structured failure a page can branch on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeamError {
    /// Stable machine-readable code.
    pub code: String,
    /// Human-readable detail.
    pub message: String,
    /// Whether a later attempt could succeed unchanged.
    #[serde(default)]
    pub retryable: bool,
}

impl SeamError {
    /// Creates an error.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }

    /// An error from the action's dispatcher.
    #[must_use]
    pub fn dispatch(message: impl Into<String>) -> Self {
        Self::new("dispatch_failed", message)
    }

    /// A malformed request.
    #[must_use]
    pub fn malformed(message: impl Into<String>) -> Self {
        Self::new("malformed", message)
    }

    /// A capability refusal.
    #[must_use]
    pub fn capability_denied(message: impl Into<String>) -> Self {
        Self::new("capability_denied", message)
    }

    /// A host that is not serving.
    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new("unavailable", message)
    }

    /// A refused navigation.
    #[must_use]
    pub fn navigation_denied(message: impl Into<String>) -> Self {
        Self::new("navigation_denied", message)
    }
}

/// What the runtime answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeamResponse {
    /// The request id this answers, when the request carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Whether the request was honoured.
    pub ok: bool,
    /// The machine-readable outcome.
    pub outcome: String,
    /// Any payload the page is entitled to see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    /// The structured failure, when `ok` is false.
    ///
    /// Boxed so the response stays small: a response is returned in the error path of
    /// every parse, and a large error type makes every caller pay for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Box<SeamError>>,
}

impl SeamResponse {
    /// A successful response.
    #[must_use]
    pub fn ok(outcome: &str, payload: Option<Value>) -> Self {
        Self {
            id: None,
            ok: true,
            outcome: outcome.to_owned(),
            payload,
            error: None,
        }
    }

    /// A refusal with a stable, machine-readable code.
    ///
    /// `impl AsRef<str>` rather than `&str` so a formatted reason does not need a borrow at
    /// every call site — the reason is usually built, not literal.
    #[must_use]
    pub fn refused(code: &str, detail: impl AsRef<str>) -> Self {
        Self::failure(SeamError::new(code, detail.as_ref()))
    }

    /// A structured failure.
    #[must_use]
    pub fn failure(error: SeamError) -> Self {
        Self {
            id: None,
            ok: false,
            outcome: format!("{}: {}", error.code, error.message),
            payload: None,
            error: Some(Box::new(error)),
        }
    }

    /// A message the seam could not parse at all.
    #[must_use]
    pub fn malformed(detail: impl AsRef<str>) -> Self {
        Self::failure(SeamError::malformed(detail.as_ref()))
    }

    /// Attaches the request id this response answers.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
}

/// One action's declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionDefinition {
    /// What the page must hold to call it.
    pub requires: CapabilitySet,
    /// What the action does, for page introspection.
    pub summary: String,
}

/// The actions a host makes available to pages.
///
/// One table is shared by every window, so a command registered after a window opens is
/// visible to that window without rebuilding its seam. That is what lets an application
/// register commands during startup and plugins register commands later without a second
/// source of truth for what a page may call.
#[derive(Debug, Clone, Default)]
pub struct ActionTable {
    actions: Arc<RwLock<BTreeMap<String, ActionDefinition>>>,
}

/// A refused action declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionTableError {
    /// The action name is empty, too long, or contains characters the bridge cannot carry.
    InvalidName {
        /// The offending name.
        name: String,
    },
    /// The action is already declared.
    Duplicate {
        /// The duplicate name.
        name: String,
    },
}

impl std::fmt::Display for ActionTableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName { name } => write!(
                f,
                "action name `{name}` must start with a lowercase letter and contain only lowercase letters, digits, `.`, `_` or `-` (max 64 bytes)"
            ),
            Self::Duplicate { name } => write!(f, "action `{name}` is already declared"),
        }
    }
}

impl std::error::Error for ActionTableError {}

impl ActionTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares an action and what it requires.
    ///
    /// # Errors
    ///
    /// Returns [`ActionTableError::InvalidName`] for a name the bridge cannot carry and
    /// [`ActionTableError::Duplicate`] when the action already exists.
    pub fn declare(
        &self,
        name: &str,
        requires: CapabilitySet,
        summary: impl Into<String>,
    ) -> Result<(), ActionTableError> {
        if !valid_action_name(name) {
            return Err(ActionTableError::InvalidName {
                name: name.to_owned(),
            });
        }
        let mut actions = self
            .actions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if actions.contains_key(name) {
            return Err(ActionTableError::Duplicate {
                name: name.to_owned(),
            });
        }
        actions.insert(
            name.to_owned(),
            ActionDefinition {
                requires,
                summary: summary.into(),
            },
        );
        Ok(())
    }

    /// Declares an action from trusted code, replacing an existing entry.
    pub(crate) fn declare_trusted(
        &self,
        name: &str,
        requires: CapabilitySet,
        summary: impl Into<String>,
    ) {
        self.actions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                name.to_owned(),
                ActionDefinition {
                    requires,
                    summary: summary.into(),
                },
            );
    }

    /// The capabilities an action requires, when it is declared.
    #[must_use]
    pub fn requires(&self, name: &str) -> Option<CapabilitySet> {
        self.actions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .map(|definition| definition.requires)
    }

    /// Every declared action, ordered by name.
    #[must_use]
    pub fn entries(&self) -> Vec<(String, ActionDefinition)> {
        self.actions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(name, definition)| (name.clone(), definition.clone()))
            .collect()
    }

    /// Removes an action, reporting whether it existed.
    pub fn remove(&self, name: &str) -> bool {
        self.actions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name)
            .is_some()
    }
}

fn valid_action_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() || name.starts_with("ncui.") {
        return false;
    }
    name.len() <= 64
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

/// The policy a page is held to.
#[derive(Debug, Clone)]
pub struct HostSeam {
    lifecycle: WindowLifecycle,
    grant: CapabilityGrant,
    navigation: NavigationPolicy,
    /// Which actions a page may call, and what each requires.
    ///
    /// A page that names an action with no entry here gets a refusal — the default is
    /// "denied", never "allowed".
    actions: ActionTable,
}

impl HostSeam {
    /// A seam over a lifecycle, a grant and a navigation policy.
    #[must_use]
    pub fn new(
        lifecycle: WindowLifecycle,
        grant: CapabilityGrant,
        navigation: NavigationPolicy,
    ) -> Self {
        Self {
            lifecycle,
            grant,
            navigation,
            actions: ActionTable::new(),
        }
    }

    /// A seam that refuses everything.
    ///
    /// Used where a host exists without a window behind it — an engine created outside the
    /// registry, or a build with the embedding layer switched off. It grants nothing, allows
    /// no navigation, and is not serving, so every message is refused with a reason rather
    /// than the seam quietly permitting what it cannot check.
    #[must_use]
    pub fn disabled() -> Self {
        Self::new(
            WindowLifecycle::new(),
            CapabilityGrant::none(),
            NavigationPolicy::new(&[]),
        )
    }

    /// Declares an action the page may perform, and what it requires.
    #[must_use]
    pub fn with_action(self, name: &str, requires: CapabilitySet) -> Self {
        self.actions.declare_trusted(name, requires, "");
        self
    }

    /// Uses a table shared with the window registry, so later declarations are visible.
    #[must_use]
    pub fn with_action_table(mut self, actions: ActionTable) -> Self {
        self.actions = actions;
        self
    }

    /// The action table this seam enforces.
    #[must_use]
    pub fn actions(&self) -> &ActionTable {
        &self.actions
    }

    /// The grant the page holds.
    #[must_use]
    pub const fn grant(&self) -> &CapabilityGrant {
        &self.grant
    }

    /// Handles one message from the page.
    ///
    /// `dispatch` is the runtime's own dispatcher. Passing it in rather than calling into the
    /// runtime from here is what keeps the capability check, consent gate and audit trail the
    /// ones the runtime already has, instead of a second implementation that will drift.
    ///
    /// The dispatcher is taken as `FnMut` because the runtime's dispatch path needs
    /// `&mut Runtime` — the same reason a page's action is a *call* rather than a query.
    /// The seam itself is read-only throughout: it holds policy, and policy does not change
    /// because a page asked.
    #[must_use]
    pub fn handle(
        &self,
        message: &SeamMessage,
        dispatch: &mut dyn FnMut(&CapabilityRequest, Value) -> Result<Value, SeamError>,
    ) -> SeamResponse {
        // A host that is not serving must not serve. This is checked before anything else so
        // a crashed engine cannot answer a request — ADR-001's crash requirement, enforced
        // rather than documented.
        if self.lifecycle.is_crashed() {
            return SeamResponse::failure(SeamError::unavailable(
                "the host crashed; the page is not being served",
            ));
        }
        if !self.lifecycle.is_serving() {
            return SeamResponse::failure(SeamError::unavailable("the host is not serving"));
        }

        match message {
            SeamMessage::Describe => SeamResponse::ok(
                "described",
                Some(json!({
                    "protocol_version": BRIDGE_PROTOCOL_VERSION,
                    "capabilities": self.grant.granted(),
                    "actions": self.actions.entries().into_iter().map(|(name, definition)| json!({
                        "name": name,
                        "requires": definition.requires,
                        "summary": definition.summary
                    })).collect::<Vec<_>>(),
                })),
            ),
            SeamMessage::ProbeNavigation { url } => self.probe_navigation(url),
            SeamMessage::Perform { name, payload } => self.perform(name, payload.clone(), dispatch),
        }
    }

    /// Decides a navigation the engine proposes.
    ///
    /// This is the method an engine calls when it is about to navigate. It is public and
    /// separate from the probe because the two have different callers and different
    /// consequences: the probe answers a page, and this one *stops* an engine. Keeping
    /// them on one policy object is what makes the answer the page gets the same answer the
    /// engine is held to.
    #[must_use]
    pub fn evaluate_navigation(&self, request: &NavigationRequest) -> Verdict {
        self.navigation.evaluate(request)
    }

    /// Answers a navigation probe using the host's policy.
    fn probe_navigation(&self, url: &str) -> SeamResponse {
        // The same parser the engine's navigation handler uses, so the answer a page gets
        // and the answer the engine is held to cannot disagree about what a URL means.
        let Some(request) = NavigationRequest::from_url(url, true) else {
            return SeamResponse::malformed("the url could not be read");
        };
        match self.navigation.evaluate(&request) {
            Verdict::Allow => SeamResponse::ok("allowed", None),
            Verdict::Deny { reason } => SeamResponse::failure(SeamError::navigation_denied(reason)),
        }
    }

    /// Runs an action after checking it is declared *and* granted.
    fn perform(
        &self,
        action: &str,
        payload: Value,
        dispatch: &mut dyn FnMut(&CapabilityRequest, Value) -> Result<Value, SeamError>,
    ) -> SeamResponse {
        let Some(requires) = self.actions.requires(action) else {
            // An undeclared action is refused rather than dispatched. This is the default that
            // stops a page inventing a route into the runtime.
            return SeamResponse::failure(SeamError::capability_denied(format!(
                "`{action}` is not an action this host offers"
            )));
        };

        if let Err(reason) = self.grant.require(requires) {
            return SeamResponse::failure(SeamError::capability_denied(reason));
        }

        let request = CapabilityRequest {
            action: action.to_owned(),
            requires,
        };
        match dispatch(&request, payload) {
            Ok(value) => SeamResponse::ok("performed", Some(value)),
            Err(error) => SeamResponse::failure(error),
        }
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::HostState;
    use crate::navigation::Origin;
    use ncui_core::Capability;

    fn running_seam() -> HostSeam {
        let lifecycle = WindowLifecycle::new();
        lifecycle.transition(HostState::Created).expect("created");
        lifecycle
            .transition(HostState::Attaching)
            .expect("attaching");
        lifecycle.transition(HostState::Running).expect("running");

        let policy = NavigationPolicy::new(&[Origin::new(
            crate::navigation::Scheme::Https,
            "app.example.com",
            None,
        )]);
        HostSeam::new(
            lifecycle,
            CapabilityGrant::of(&[Capability::InspectRuntime]),
            policy,
        )
        .with_action(
            "describe_metrics",
            CapabilitySet::empty().with(Capability::InspectRuntime),
        )
    }

    #[test]
    fn a_declared_and_granted_action_is_dispatched() {
        let seam = running_seam();
        let called = std::cell::RefCell::new(Vec::new());
        let response = seam.handle(
            &SeamMessage::Perform {
                name: "describe_metrics".to_owned(),
                payload: json!({ "probe": 7 }),
            },
            &mut |request, payload| {
                assert_eq!(payload, json!({ "probe": 7 }));
                called.borrow_mut().push(request.action.clone());
                Ok(json!({ "ok": true }))
            },
        );
        assert!(
            response.ok,
            "a granted, declared action must be served: {response:?}"
        );
        assert_eq!(called.borrow().as_slice(), ["describe_metrics"]);
    }

    #[test]
    fn an_undeclared_action_is_refused_without_dispatch() {
        // The default matters: a page inventing a route into the runtime is the failure this
        // closes, so nothing may reach the dispatcher unless it was declared here.
        let seam = running_seam();
        let called = std::cell::Cell::new(false);
        let response = seam.handle(
            &SeamMessage::Perform {
                name: "shutdown_runtime".to_owned(),
                payload: Value::Null,
            },
            &mut |_, _| {
                called.set(true);
                Ok(Value::Null)
            },
        );
        assert!(!response.ok);
        assert!(!called.get(), "an undeclared action reached the dispatcher");
        assert!(
            response.outcome.contains("capability_denied"),
            "{}",
            response.outcome
        );
    }

    #[test]
    fn a_declared_but_ungranted_action_is_refused() {
        let seam =
            running_seam().with_action("reset", CapabilitySet::empty().with(Capability::Deploy));
        let called = std::cell::Cell::new(false);
        let response = seam.handle(
            &SeamMessage::Perform {
                name: "reset".to_owned(),
                payload: Value::Null,
            },
            &mut |_, _| {
                called.set(true);
                Ok(Value::Null)
            },
        );
        assert!(
            !response.ok,
            "an ungranted action must be refused: {response:?}"
        );
        assert!(!called.get(), "an ungranted action reached the dispatcher");
    }

    #[test]
    fn a_crashed_host_refuses_everything() {
        // ADR-001 calls the crash a real failure mode. The enforcement is that a dead host
        // answers nothing at all.
        let lifecycle = WindowLifecycle::new();
        lifecycle.transition(HostState::Created).expect("created");
        lifecycle
            .transition(HostState::Attaching)
            .expect("attaching");
        lifecycle.transition(HostState::Running).expect("running");
        lifecycle.transition(HostState::Crashed).expect("crash");

        let seam = HostSeam::new(
            lifecycle,
            CapabilityGrant::of(&[Capability::InspectRuntime]),
            NavigationPolicy::new(&[]),
        )
        .with_action(
            "describe_metrics",
            CapabilitySet::empty().with(Capability::InspectRuntime),
        );

        for message in [
            SeamMessage::Describe,
            SeamMessage::Perform {
                name: "describe_metrics".to_owned(),
                payload: Value::Null,
            },
        ] {
            let response = seam.handle(&message, &mut |_, _| Ok(Value::Null));
            assert!(!response.ok, "a crashed host served {message:?}");
            assert!(
                response.outcome.contains("unavailable"),
                "{}",
                response.outcome
            );
        }
    }

    #[test]
    fn a_suspended_host_refuses_rather_than_queueing_silently() {
        let seam = running_seam();
        seam.lifecycle
            .transition(HostState::Suspended)
            .expect("suspend");
        let response = seam.handle(&SeamMessage::Describe, &mut |_, _| Ok(Value::Null));
        assert!(!response.ok);
        assert!(
            response.outcome.contains("not serving"),
            "{}",
            response.outcome
        );
    }

    #[test]
    fn describing_tells_the_page_only_what_it_may_do() {
        let response = running_seam().handle(&SeamMessage::Describe, &mut |_, _| Ok(Value::Null));
        assert!(response.ok);
        let payload = response.payload.expect("a payload");
        let actions = payload["actions"].as_array().expect("an array");
        assert_eq!(actions.len(), 1, "only declared actions are advertised");
        assert_eq!(actions[0]["name"], json!("describe_metrics"));
    }

    #[test]
    fn a_navigation_probe_answers_with_the_hosts_policy() {
        let seam = running_seam();
        let allowed = seam.handle(
            &SeamMessage::ProbeNavigation {
                url: "https://app.example.com/next".to_owned(),
            },
            &mut |_, _| Ok(Value::Null),
        );
        assert!(allowed.ok, "{}", allowed.outcome);

        let denied = seam.handle(
            &SeamMessage::ProbeNavigation {
                url: "https://evil.test/steal".to_owned(),
            },
            &mut |_, _| Ok(Value::Null),
        );
        assert!(!denied.ok);
        assert!(
            denied.outcome.contains("navigation_denied"),
            "{}",
            denied.outcome
        );
    }

    #[test]
    fn a_javascript_url_is_refused_through_the_seam_too() {
        // The seam must not be a way around the navigation policy.
        let response = running_seam().handle(
            &SeamMessage::ProbeNavigation {
                url: "javascript://app.example.com/alert(1)".to_owned(),
            },
            &mut |_, _| Ok(Value::Null),
        );
        assert!(!response.ok, "javascript: was allowed through the seam");
    }

    #[test]
    fn malformed_page_json_produces_a_response_rather_than_a_panic() {
        for raw in [
            "",
            "not json",
            "{}",
            r#"{"action":"nope"}"#,
            r#"{"action":"probe_navigation"}"#,
            r#"{"action":"perform"}"#,
        ] {
            // Total: every one of these is answered, and the refusals carry a stable code.
            // `parse` returns a `Result`, so a malformed message has exactly one failure
            // shape rather than being flattened into a value the caller must re-check.
            match SeamMessage::parse(raw) {
                Ok(SeamMessage::Describe) => {}
                Ok(other) => panic!("{raw:?} was accepted as {other:?}"),
                Err(response) => {
                    assert!(!response.ok, "{raw:?} produced a successful error response");
                    assert!(
                        response.outcome.starts_with("malformed"),
                        "{raw:?} produced {:?}, which is not a malformed-message refusal",
                        response.outcome
                    );
                }
            }
        }
    }

    #[test]
    fn a_perform_without_a_name_is_refused() {
        let response = SeamMessage::parse(r#"{"action":"perform"}"#)
            .expect_err("a perform with no action must not degrade into a describe");
        assert_eq!(response.error.expect("an error").code, "malformed");
    }

    #[test]
    fn a_request_id_and_payload_round_trip() {
        let raw = r#"{"id":"call-1","action":"perform","name":"echo","payload":{"n":7}}"#;
        let request = SeamRequest::parse(raw).expect("a well-formed request");
        assert_eq!(request.id, "call-1");
        assert_eq!(
            request.message,
            SeamMessage::Perform {
                name: "echo".to_owned(),
                payload: json!({ "n": 7 }),
            }
        );

        let response = SeamResponse::malformed("refused").with_id(&request.id);
        assert_eq!(response.id.as_deref(), Some("call-1"));
        let encoded = serde_json::to_string(&response).expect("serialise");
        assert!(encoded.contains(r#""id":"call-1""#), "{encoded}");
        assert!(encoded.contains(r#""code":"malformed""#), "{encoded}");
    }

    #[test]
    fn a_missing_or_invalid_request_id_is_refused() {
        for raw in [
            r#"{"action":"describe"}"#,
            r#"{"id":"","action":"describe"}"#,
            r#"{"id":"contains space","action":"describe"}"#,
            r#"{"id":7,"action":"describe"}"#,
        ] {
            let response = SeamRequest::parse(raw)
                .expect_err("a request without a usable id cannot be answered");
            assert_eq!(response.error.expect("an error").code, "malformed");
            assert!(response.id.is_none(), "an invalid id must not be echoed");
        }
    }

    #[test]
    fn a_malformed_action_keeps_the_request_id() {
        let response = SeamRequest::parse(r#"{"id":"call-2","action":"nope"}"#)
            .expect_err("an unknown action is refused");
        assert_eq!(response.id.as_deref(), Some("call-2"));
    }

    #[test]
    fn an_oversized_request_id_is_refused_before_it_reaches_an_audit_log() {
        let raw = format!(
            r#"{{"id":"{}","action":"describe"}}"#,
            "a".repeat(MAX_REQUEST_ID_BYTES + 1)
        );
        let response = SeamRequest::parse(&raw).expect_err("an oversized id is refused");
        assert_eq!(response.error.expect("an error").code, "malformed");
    }

    #[test]
    fn a_round_tripped_message_parses_to_the_same_value() {
        for message in [
            SeamMessage::Describe,
            SeamMessage::ProbeNavigation {
                url: "https://app.example.com/x".to_owned(),
            },
            SeamMessage::Perform {
                name: "describe_metrics".to_owned(),
                payload: json!({ "deep": true }),
            },
        ] {
            let encoded = serde_json::to_string(&message).expect("serialise");
            assert_eq!(
                SeamMessage::parse(&encoded).expect("reparses"),
                message,
                "{encoded:?} did not round trip"
            );
        }
    }
}
