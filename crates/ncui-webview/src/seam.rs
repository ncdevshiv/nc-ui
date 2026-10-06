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

use ncui_core::CapabilitySet;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::capability::{CapabilityGrant, CapabilityRequest};
use crate::lifecycle::WindowLifecycle;
use crate::navigation::{NavigationPolicy, NavigationRequest};

/// A message from page content to the runtime.
///
/// Deliberately *not* an RPC envelope: there is no correlation id and no method name, because
/// a page must not be able to address the runtime's dispatch table.
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

impl SeamMessage {
    /// Parses a message from page JSON.
    ///
    /// Total: unknown actions, malformed JSON and wrong shapes all produce a response rather
    /// than an error the caller must handle separately.
    pub fn parse(raw: &str) -> Result<Self, SeamResponse> {
        // Total by construction: every rejection is a `SeamResponse` carrying a stable code,
        // so a page cannot distinguish a parse failure from a crash, and the caller has one
        // type to return rather than two.
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            return Err(SeamResponse::malformed("the message is not valid JSON"));
        };
        let Some(action) = value.get("action").and_then(Value::as_str) else {
            return Err(SeamResponse::malformed("the message has no `action`"));
        };
        match action {
            "perform" => {
                Ok(value
                    .get("name")
                    .and_then(Value::as_str)
                    .map_or(Self::Describe, |name| Self::Perform {
                        name: name.to_owned(),
                    }))
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

/// What the runtime answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeamResponse {
    /// Whether the request was honoured.
    pub ok: bool,
    /// The machine-readable outcome.
    pub outcome: String,
    /// Any payload the page is entitled to see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

impl SeamResponse {
    /// A successful response.
    #[must_use]
    pub fn ok(outcome: &str, payload: Option<Value>) -> Self {
        Self {
            ok: true,
            outcome: outcome.to_owned(),
            payload,
        }
    }

    /// A refusal with a stable, machine-readable code.
    ///
    /// `impl AsRef<str>` rather than `&str` so a formatted reason does not need a borrow at
    /// every call site — the reason is usually built, not literal.
    #[must_use]
    pub fn refused(code: &str, detail: impl AsRef<str>) -> Self {
        Self {
            ok: false,
            outcome: format!("{code}: {}", detail.as_ref()),
            payload: None,
        }
    }

    /// A message the seam could not parse at all.
    #[must_use]
    pub fn malformed(detail: impl AsRef<str>) -> Self {
        Self::refused("malformed", detail)
    }
}

/// The policy a page is held to.
#[derive(Debug, Clone)]
pub struct HostSeam {
    lifecycle: WindowLifecycle,
    grant: CapabilityGrant,
    navigation: NavigationPolicy,
    /// Maps a page action name to what it requires.
    ///
    /// A page that names an action with no entry here gets a refusal — the default is
    /// "denied", never "allowed".
    actions: Vec<(String, CapabilityRequest)>,
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
            actions: Vec::new(),
        }
    }

    /// Declares an action the page may perform, and what it requires.
    #[must_use]
    pub fn with_action(mut self, name: &str, requires: CapabilitySet) -> Self {
        self.actions.push((
            name.to_owned(),
            CapabilityRequest {
                action: name.to_owned(),
                requires,
            },
        ));
        self
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
    #[must_use]
    pub fn handle(
        &self,
        message: &SeamMessage,
        dispatch: &dyn Fn(&CapabilityRequest, Value) -> Result<Value, String>,
    ) -> SeamResponse {
        // A host that is not serving must not serve. This is checked before anything else so
        // a crashed engine cannot answer a request — ADR-001's crash requirement, enforced
        // rather than documented.
        if self.lifecycle.is_crashed() {
            return SeamResponse::refused(
                "unavailable",
                "the host crashed; the page is not being served",
            );
        }
        if !self.lifecycle.is_serving() {
            return SeamResponse::refused("unavailable", "the host is not serving");
        }

        match message {
            SeamMessage::Describe => SeamResponse::ok(
                "described",
                Some(json!({
                    "capabilities": self.grant.granted(),
                    "actions": self.actions.iter().map(|(n, r)| json!({
                        "name": n,
                        "requires": r.requires
                    })).collect::<Vec<_>>(),
                })),
            ),
            SeamMessage::ProbeNavigation { url } => self.probe_navigation(url),
            SeamMessage::Perform { name } => self.perform(name, dispatch),
        }
    }

    /// Answers a navigation probe using the host's policy.
    fn probe_navigation(&self, url: &str) -> SeamResponse {
        // Parsing is deliberately minimal and total: a URL the seam cannot read is refused,
        // never guessed at.
        let Some((scheme, rest)) = url.split_once("://") else {
            return SeamResponse::refused("malformed", "the url has no scheme");
        };
        if rest.is_empty() {
            return SeamResponse::refused("malformed", "the url has no host");
        }
        // The authority ends at the first `/`, `?` or `#`. Without this, `https://app/x`
        // yields a host of `app/x`, which matches no allow-list entry and refuses a
        // navigation the policy was written to permit — a real URL would never load.
        let authority = rest
            .split(['/', '?', '#'])
            .next()
            .unwrap_or(rest)
            .to_owned();
        if authority.is_empty() {
            return SeamResponse::refused("malformed", "the url has no host");
        }

        // A bare IPv6 literal is bracketed, so the *last* colon is the port separator rather
        // than the first — `rsplit_once` handles both, and a non-numeric tail falls back to
        // being part of the host.
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => match port.parse::<u16>() {
                Ok(port) => (host.to_owned(), Some(port)),
                Err(_) => (authority.clone(), None),
            },
            None => (authority.clone(), None),
        };

        let request = NavigationRequest {
            target: crate::navigation::Origin::new(
                crate::navigation::Scheme::parse(scheme),
                &host,
                port,
            ),
            top_level: true,
        };
        match self.navigation.evaluate(&request) {
            crate::navigation::Verdict::Allow => SeamResponse::ok("allowed", None),
            crate::navigation::Verdict::Deny { reason } => {
                SeamResponse::refused("navigation_denied", reason)
            }
        }
    }

    /// Runs an action after checking it is declared *and* granted.
    fn perform(
        &self,
        action: &str,
        dispatch: &dyn Fn(&CapabilityRequest, Value) -> Result<Value, String>,
    ) -> SeamResponse {
        let Some(request) = self
            .actions
            .iter()
            .find(|(name, _)| name == action)
            .map(|(_, request)| request)
        else {
            // An undeclared action is refused rather than dispatched. This is the default that
            // stops a page inventing a route into the runtime.
            return SeamResponse::refused(
                "capability_denied",
                format!("`{action}` is not an action this host offers"),
            );
        };

        if let Err(reason) = self.grant.require(request.requires) {
            return SeamResponse::refused("capability_denied", reason);
        }

        match dispatch(request, json!({ "action": action })) {
            Ok(value) => SeamResponse::ok("performed", Some(value)),
            Err(error) => SeamResponse::refused("dispatch_failed", error),
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
        let mut lifecycle = WindowLifecycle::new();
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
            },
            &|request, _| {
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
            },
            &|_, _| {
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
            },
            &|_, _| {
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
        let mut lifecycle = WindowLifecycle::new();
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
            },
        ] {
            let response = seam.handle(&message, &|_, _| Ok(Value::Null));
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
        let mut seam = running_seam();
        seam.lifecycle
            .transition(HostState::Suspended)
            .expect("suspend");
        let response = seam.handle(&SeamMessage::Describe, &|_, _| Ok(Value::Null));
        assert!(!response.ok);
        assert!(
            response.outcome.contains("not serving"),
            "{}",
            response.outcome
        );
    }

    #[test]
    fn describing_tells_the_page_only_what_it_may_do() {
        let response = running_seam().handle(&SeamMessage::Describe, &|_, _| Ok(Value::Null));
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
            &|_, _| Ok(Value::Null),
        );
        assert!(allowed.ok, "{}", allowed.outcome);

        let denied = seam.handle(
            &SeamMessage::ProbeNavigation {
                url: "https://evil.test/steal".to_owned(),
            },
            &|_, _| Ok(Value::Null),
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
            &|_, _| Ok(Value::Null),
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
    fn a_perform_without_a_name_degrades_to_describe_rather_than_failing() {
        // Total in the permissive direction only where it is safe: a `perform` naming nothing
        // asks for nothing, which `describe` answers.
        assert_eq!(
            SeamMessage::parse(r#"{"action":"perform"}"#).expect("parsed"),
            SeamMessage::Describe
        );
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
