//! Agent sessions: authentication, rate limiting, authorization, and audit.
//!
//! An [`AgentSession`] is the runtime's view of one connected agent. It bundles the
//! identity, the credentials, the granted capabilities, and the rate-limit state that
//! together decide whether a given request may proceed.
//!
//! The pipeline is enforced in [`crate::dispatch::dispatch`], not here, because the order of
//! the checks is itself a security property: a caller must not be able to distinguish
//! "unknown agent" from "known agent, insufficient capability" by timing or by error
//! message, and an unauthenticated caller must not be able to consume a rate-limit bucket
//! that belongs to somebody else.

use core::time::Duration;
use std::sync::{Arc, Mutex};

use ncui_core::{CapabilitySet, CorrelationId, ErrorKind, NcError, Result};
use ncui_security::authz::{AgentRole, Authorizer};
use ncui_security::ratelimit::RateLimiter;
use ncui_security::token::SecretToken;

/// Identifies one agent session.
/// `PartialEq` and `Eq` are deliberately not derived: two sessions are never compared, and
/// deriving them over a `Mutex`-wrapped limiter would assert that two agents sharing a
/// budget are the same agent, which is the opposite of what the identity field means.
#[derive(Debug, Clone)]
pub struct AgentSession {
    /// Stable identifier recorded in every audit event.
    pub agent_id: String,
    /// The standing grant derived from the agent's role.
    pub granted: CapabilitySet,
    /// The role the grant came from.
    pub role: AgentRole,
    /// The request budget this session spends from.
    ///
    /// Shared rather than owned because the remote transport hands one limiter to every
    /// connection: a per-session limiter gave a peer a full bucket on each reconnect, so the
    /// aggregate rate was `burst × connections`. The in-process path still hands each session
    /// its own, which is correct there because there is only one session.
    limiter: Arc<Mutex<RateLimiter>>,
    authenticated: bool,
}

impl AgentSession {
    /// Creates a session for `role`, with rate limits from the runtime configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Validation`] if `agent_id` is blank — an audit trail that
    /// cannot name its actor cannot answer "who did this", which is the only question it
    /// exists to answer — or if the rate limits are incoherent.
    pub fn new(
        agent_id: impl Into<String>,
        role: AgentRole,
        per_second: u32,
        burst: u32,
    ) -> Result<Self> {
        let limiter = RateLimiter::new(per_second, burst)?;
        Self::with_limiter(agent_id, role, Arc::new(Mutex::new(limiter)))
    }

    /// Creates a session that spends from an already-constructed limiter.
    ///
    /// The remote transport holds one limiter per *server* rather than per connection, so a
    /// peer cannot buy a fresh budget by reconnecting. The in-process path still uses
    /// [`AgentSession::new`], where a per-session budget is the right shape because there is
    /// only one session.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Validation`] if `agent_id` is blank, for the same reason as
    /// [`AgentSession::new`].
    pub fn with_limiter(
        agent_id: impl Into<String>,
        role: AgentRole,
        limiter: Arc<Mutex<RateLimiter>>,
    ) -> Result<Self> {
        let agent_id = agent_id.into();
        if agent_id.trim().is_empty() {
            return Err(NcError::new(ErrorKind::Validation {
                field: "agent_id".to_owned(),
                reason: "an agent session must be attributable to a named actor".to_owned(),
            })
            .operation("agent::session::with_limiter"));
        }
        Ok(Self {
            agent_id,
            granted: role.capabilities(),
            role,
            limiter,
            authenticated: false,
        })
    }

    /// Creates a session for a page inside a granted window.
    ///
    /// A page is not an agent. It has no bearer token and no role; its authority is the
    /// window's grant, and the host vouched for it by granting that window in the first
    /// place. So the session starts authenticated — the grant *is* the credential — and it
    /// holds exactly the capabilities the grant carries and nothing else.
    ///
    /// The role is `Observer` because there is no page role: it names the absence of a
    /// standing administrative authority, and the granted set is the real authority. The
    /// audit trail records both, so a reader can see that the actor was a page and what it
    /// was permitted to do.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Validation`] if `agent_id` is blank, for the same reason as
    /// [`AgentSession::new`]: an audit trail that cannot name its actor cannot answer
    /// "who did this".
    pub fn for_page(
        agent_id: impl Into<String>,
        granted: CapabilitySet,
        limiter: Arc<Mutex<RateLimiter>>,
    ) -> Result<Self> {
        let agent_id = agent_id.into();
        if agent_id.trim().is_empty() {
            return Err(NcError::new(ErrorKind::Validation {
                field: "agent_id".to_owned(),
                reason: "a page session must be attributable to a named window".to_owned(),
            })
            .operation("agent::session::for_page"));
        }
        Ok(Self {
            agent_id,
            granted,
            role: AgentRole::Observer,
            limiter,
            authenticated: true,
        })
    }

    /// Verifies a bearer token and, on success, authenticates the session.
    ///
    /// Authentication is state rather than a convention. An unauthenticated session cannot
    /// authorize, and therefore cannot dispatch, so a caller that forgets to authenticate
    /// gets a refusal instead of silent access.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::CapabilityDenied`] when the token does not match. The failure
    /// does not distinguish "wrong token" from "wrong length" or from "no token supplied",
    /// because that difference is a probing aid.
    pub fn authenticate(&mut self, presented: &SecretToken, expected: &SecretToken) -> Result<()> {
        if expected.verify(presented.expose_secret()) {
            self.authenticated = true;
            return Ok(());
        }
        Err(NcError::new(ErrorKind::CapabilityDenied {
            capability: "authenticate".to_owned(),
            subject: self.agent_id.clone(),
        })
        .operation("agent::authenticate"))
    }

    /// Consumes one unit of the request budget.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::LimitExceeded`] when the bucket is empty. Rate limiting runs
    /// after authentication on purpose: an unauthenticated caller must not be able to
    /// exhaust a real agent's budget.
    pub fn rate_limit(&mut self, now: Duration, cost: u32) -> Result<()> {
        self.limiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_acquire(now, cost)
    }

    /// How many requests the session may still make at `now`.
    ///
    /// When several sessions share one limiter — the remote transport's shape — this is the
    /// shared remaining budget rather than a per-session one.
    #[must_use]
    pub fn available(&self, now: Duration) -> u64 {
        self.limiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .available(now)
    }

    /// Whether this session has presented a valid credential.
    #[must_use]
    pub const fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Refuses an unauthenticated session.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::CapabilityDenied`] when [`AgentSession::authenticate`] has
    /// not succeeded.
    pub fn require_authenticated(&self) -> Result<()> {
        if self.authenticated {
            return Ok(());
        }
        Err(NcError::new(ErrorKind::CapabilityDenied {
            capability: "authenticate".to_owned(),
            subject: self.agent_id.clone(),
        })
        .operation("agent::require_authenticated"))
    }

    /// Checks that the session holds every capability in `required`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::CapabilityDenied`] when the grant does not cover `required`,
    /// and [`ErrorKind::Validation`] if the session has no name.
    pub fn authorize(&self, required: CapabilitySet) -> Result<()> {
        self.require_authenticated()?;
        Authorizer.authorize(&self.agent_id, required, self.granted)
    }

    /// Authorizes a request that may include destructive capabilities.
    ///
    /// Holding a destructive capability authorises the *possibility* of the operation, not
    /// this call. `consent` must come from a real signal — a user confirmation or an
    /// explicit agent argument — never from a constant.
    ///
    /// # Errors
    ///
    /// As [`AgentSession::authorize`], and additionally [`ErrorKind::CapabilityDenied`]
    /// when `required` contains a destructive capability and `consent` is `false`.
    pub fn authorize_destructive(&self, required: CapabilitySet, consent: bool) -> Result<()> {
        self.require_authenticated()?;
        Authorizer.require_destructive(&self.agent_id, required, self.granted, consent)
    }

    /// Requires explicit consent for a method the registry declares destructive.
    ///
    /// This is deliberately separate from [`AgentSession::authorize_destructive`]. That
    /// method asks whether a *capability* is destructive, which is the wrong question at
    /// dispatch: purging the log ring destroys data while requiring only `read.logs` to
    /// request, so a capability-keyed gate never fires. Destructiveness is a property of
    /// the operation, so this gate is keyed on the method's declared flag.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::CapabilityDenied`] when the method is destructive and the
    /// caller did not state consent. A request that omits the consent parameter is
    /// treated as withholding it, so the default is to refuse.
    pub fn require_consent(&self, destructive: bool, consent: bool) -> Result<()> {
        self.require_authenticated()?;
        if destructive && !consent {
            return Err(NcError::new(ErrorKind::CapabilityDenied {
                capability: "destructive authorization".to_owned(),
                subject: self.agent_id.clone(),
            })
            .operation("agent::require_consent"));
        }
        Ok(())
    }

    /// A fresh correlation id for one request handled by this session.
    #[must_use]
    pub fn next_correlation(&self) -> CorrelationId {
        CorrelationId::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::Capability;

    /// The credential every test session presents.
    fn token() -> SecretToken {
        SecretToken::new(vec![0x11; 32]).expect("token")
    }

    /// An authenticated session, which is what dispatch requires.
    fn session(role: AgentRole) -> AgentSession {
        let mut session = AgentSession::new("agent-under-test", role, 100, 200).expect("session");
        session
            .authenticate(&token(), &token())
            .expect("test session authenticates");
        session
    }

    /// A session that has not authenticated, for testing that it is enforced.
    fn anonymous(role: AgentRole) -> AgentSession {
        AgentSession::new("anonymous", role, 100, 200).expect("session")
    }

    /// An authenticated session with explicit rate limits.
    fn authenticated(role: AgentRole, per_second: u32, burst: u32) -> AgentSession {
        let mut session =
            AgentSession::new("agent-under-test", role, per_second, burst).expect("session");
        session
            .authenticate(&token(), &token())
            .expect("authenticates");
        session
    }

    #[test]
    fn a_session_cannot_be_created_without_a_name() {
        let err = AgentSession::new("   ", AgentRole::Observer, 10, 10)
            .expect_err("a blank agent id must be refused");
        assert!(err.to_string().contains("agent_id"), "got: {err}");
    }

    #[test]
    fn a_matching_token_authenticates_and_a_wrong_one_does_not() {
        let mut session = anonymous(AgentRole::Observer);
        assert!(!session.is_authenticated());
        assert!(session.authenticate(&token(), &token()).is_ok());
        assert!(session.is_authenticated());

        let mut other = anonymous(AgentRole::Observer);
        let wrong = SecretToken::new(vec![9u8; 32]).expect("token");
        let err = other
            .authenticate(&token(), &wrong)
            .expect_err("a wrong token must be refused");
        assert_eq!(err.code(), "capability_denied");
        assert!(
            !other.is_authenticated(),
            "a failed authentication must not leave the session usable"
        );
    }

    #[test]
    fn an_unauthenticated_session_cannot_authorize_or_dispatch() {
        let session = anonymous(AgentRole::Administrator);
        let err = session
            .authorize(CapabilitySet::empty().with(Capability::ReadLogs))
            .expect_err("an unauthenticated session must be refused");
        assert_eq!(err.code(), "capability_denied");
        assert!(session.require_consent(false, true).is_err());
        assert!(
            session
                .authorize_destructive(CapabilitySet::empty(), true)
                .is_err()
        );
    }

    #[test]
    fn a_token_of_a_different_length_is_refused() {
        let mut session = anonymous(AgentRole::Observer);
        let short = SecretToken::new(vec![1; 16]).expect("token");
        let long = SecretToken::new(vec![1; 32]).expect("token");
        assert!(session.authenticate(&short, &long).is_err());
    }

    #[test]
    fn an_observer_cannot_spend_more_than_its_budget() {
        let mut session = authenticated(AgentRole::Observer, 1, 2);
        let now = Duration::from_secs(0);
        assert!(session.rate_limit(now, 1).is_ok());
        assert!(session.rate_limit(now, 1).is_ok());
        let err = session
            .rate_limit(now, 1)
            .expect_err("the burst must be finite");
        assert_eq!(err.code(), "limit_exceeded");
    }

    #[test]
    fn the_budget_refills_over_time() {
        let mut session = authenticated(AgentRole::Observer, 1, 1);
        assert!(session.rate_limit(Duration::from_secs(0), 1).is_ok());
        assert!(session.rate_limit(Duration::from_secs(0), 1).is_err());
        assert!(
            session.rate_limit(Duration::from_secs(2), 1).is_ok(),
            "the bucket must refill"
        );
    }

    #[test]
    fn an_observer_may_read_logs_but_may_not_delete_data() {
        let session = session(AgentRole::Observer);
        assert!(
            session
                .authorize(CapabilitySet::empty().with(Capability::ReadLogs))
                .is_ok()
        );
        let err = session
            .authorize(CapabilitySet::empty().with(Capability::DeleteData))
            .expect_err("an observer must not hold delete.data");
        assert_eq!(err.code(), "capability_denied");
    }

    #[test]
    fn a_destructive_call_needs_consent_even_when_the_capability_is_held() {
        let session = session(AgentRole::Administrator);
        let required = CapabilitySet::empty().with(Capability::DeleteData);
        assert!(
            session.authorize(required).is_ok(),
            "the capability is held"
        );

        let err = session
            .authorize_destructive(required, false)
            .expect_err("consent must be required for a destructive call");
        assert_eq!(err.code(), "capability_denied");
        assert!(
            session.authorize_destructive(required, true).is_ok(),
            "consent plus the capability must succeed"
        );
    }

    #[test]
    fn consent_never_manufactures_authority_that_was_never_granted() {
        let session = session(AgentRole::Observer);
        let required = CapabilitySet::empty().with(Capability::DeleteData);
        let err = session
            .authorize_destructive(required, true)
            .expect_err("consent must not substitute for the capability");
        assert_eq!(err.code(), "capability_denied");
    }

    #[test]
    fn each_session_keeps_its_own_budget() {
        let small = |id: &str| {
            let mut session = AgentSession::new(id, AgentRole::Observer, 1, 2).expect("session");
            session
                .authenticate(&token(), &token())
                .expect("authenticates");
            session
        };
        let mut first = small("first");
        let mut second = small("second");
        for _ in 0..2 {
            assert!(first.rate_limit(Duration::from_secs(0), 1).is_ok());
        }
        assert!(first.rate_limit(Duration::from_secs(0), 1).is_err());
        assert!(
            second.rate_limit(Duration::from_secs(0), 1).is_ok(),
            "one agent must not exhaust another's budget"
        );
    }

    #[test]
    fn consent_is_required_for_a_destructive_method_even_with_read_only_authority() {
        let session = session(AgentRole::Administrator);
        // The administrator holds `read.logs`, but purging the log ring destroys data, so
        // capability alone must not be enough.
        assert!(session.require_consent(true, false).is_err());
        assert!(session.require_consent(true, true).is_ok());
        assert!(session.require_consent(false, false).is_ok());
    }

    #[test]
    fn every_request_gets_its_own_correlation_id() {
        let session = session(AgentRole::Observer);
        assert_ne!(session.next_correlation(), session.next_correlation());
    }
}
