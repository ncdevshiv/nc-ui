//! Capability authorization and agent role grants.
//!
//! NC-UI has no ambient authority: nothing acts unless a [`CapabilitySet`] says it may.
//! This module turns "an agent asked for X" into "X is allowed / denied", and it defines
//! the standing grants each [`AgentRole`] carries.
//!
//! Two independent gates exist, and conflating them is the design mistake this module is
//! built to prevent:
//!
//! 1. **Standing authority** — [`Authorizer::authorize`] answers "does the subject hold
//!    the capability?". This is a set comparison and nothing more.
//! 2. **Per-call consent** — [`Authorizer::require_destructive_with_token`] answers "even
//!    though the subject holds the capability, did the host approve this exact invocation?".
//!
//! Spec 88 requires the second gate. The reason is that a capability grant is a *standing*
//! authorization: it stays valid for the whole session, including for code paths, agent
//! plans and tool invocations that did not exist when the grant was made. Destructive
//! operations are precisely the ones where that gap is unacceptable — a grant of
//! `delete.data` obtained for "clean up stale build output" would otherwise silently
//! authorize deleting the user's source. Requiring an exact, consume-once token means a
//! destructive call fails *closed* when the surrounding consent plumbing is missing,
//! broken, or bypassed. It converts "we forgot to ask" from silent data loss into a
//! denial.
//!
//! The [`Authorizer`] is a zero-sized, stateless decision function. It holds no policy
//! store, ledger or clock: the caller supplies the granted set and the token binding, so
//! the decision is reproducible from its arguments alone and can be replayed in an audit.

use core::fmt;
use ncui_core::capability::CapabilityScope;
use ncui_core::{Capability, CapabilitySet, CorrelationId, ErrorKind, NcError, Result};

/// The operation name recorded on authorization failures.
const OP_AUTHORIZE: &str = "security::authz::authorize";
/// The operation name recorded when a destructive call lacks per-call consent.
const OP_REQUIRE_DESTRUCTIVE: &str = "security::authz::require_destructive";
/// The operation name recorded when a destructive call lacks a consent token.
const OP_REQUIRE_TOKEN: &str = "security::authz::require_destructive_with_token";
/// The operation name recorded when a consent token fails to parse.
const OP_CONSENT_PARSE: &str = "security::authz::ConsentToken::from_value";

/// The longest subject identifier carried by a consent token.
pub const MAX_CONSENT_SUBJECT_BYTES: usize = 256;
/// The longest method name carried by a consent token.
pub const MAX_CONSENT_METHOD_BYTES: usize = 256;

/// An opaque, consume-once proof that the host approved one destructive invocation.
///
/// The runtime ledger is the authority. A token binds the authenticated subject,
/// logical-operation correlation, method, capabilities and operation-parameter digest;
/// the ledger records the same binding and spends the nonce on first use. Runtime
/// issuance alone is not evidence of human approval: embedders must mint only after
/// their host has actually approved the displayed operation.
///
/// The wire form alone proves nothing because a client can fabricate any JSON. A
/// fabricated nonce is absent from the ledger and is refused.
pub struct ConsentToken {
    correlation: CorrelationId,
    subject: String,
    method: String,
    capability: CapabilitySet,
    payload_digest: [u8; 32],
    nonce: u128,
}

impl ConsentToken {
    /// Mints a token from a complete binding and 128 bits of OS entropy.
    #[must_use]
    pub fn mint(
        correlation: CorrelationId,
        subject: impl Into<String>,
        method: impl Into<String>,
        capability: CapabilitySet,
        payload_digest: [u8; 32],
        nonce: u128,
    ) -> Self {
        Self {
            correlation,
            subject: subject.into(),
            method: method.into(),
            capability,
            payload_digest,
            nonce,
        }
    }

    /// The logical operation this token was minted for.
    #[must_use]
    pub const fn correlation(&self) -> CorrelationId {
        self.correlation
    }

    /// The authenticated subject this token was minted for.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// The method this token was minted for.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The capability set this token authorizes.
    #[must_use]
    pub const fn capability(&self) -> CapabilitySet {
        self.capability
    }

    /// The SHA-256 digest of the operation parameters, excluding consent fields.
    #[must_use]
    pub const fn payload_digest(&self) -> [u8; 32] {
        self.payload_digest
    }

    /// The 128-bit nonce identifying this token in the runtime's ledger.
    #[must_use]
    pub const fn nonce(&self) -> u128 {
        self.nonce
    }

    /// Whether this token authorizes the exact subject, operation and requirements.
    #[must_use]
    pub fn matches(
        &self,
        subject: &str,
        correlation: CorrelationId,
        method: &str,
        required: CapabilitySet,
        payload_digest: [u8; 32],
    ) -> bool {
        self.subject == subject
            && self.correlation == correlation
            && self.method == method
            && required.is_subset_of(self.capability)
            && self.payload_digest == payload_digest
    }

    /// Serializes the token to its wire JSON object.
    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        let capabilities: Vec<&str> = self.capability.iter().map(Capability::as_str).collect();
        serde_json::json!({
            "correlation": self.correlation.to_string(),
            "subject": self.subject,
            "method": self.method,
            "capability": capabilities,
            "payload_sha256": hex_digest(&self.payload_digest),
            "nonce": format!("{:#034x}", self.nonce),
        })
    }

    /// Parses the wire JSON object into a token.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] if any field is missing or malformed.
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        let correlation_text = value
            .get("correlation")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| consent_validation("correlation", "must be a correlation id string"))?;
        let correlation = correlation_text
            .parse::<CorrelationId>()
            .map_err(|error| consent_validation("correlation", error.to_string()))?;
        let subject = value
            .get("subject")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| consent_validation("subject", "must be a subject string"))?;
        let method = value
            .get("method")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| consent_validation("method", "must be a method name string"))?;
        validate_consent_binding(subject, method)?;
        let capability_value = value.get("capability").ok_or_else(|| {
            consent_validation("capability", "must be an array of capability strings")
        })?;
        let names = capability_value.as_array().ok_or_else(|| {
            consent_validation("capability", "must be an array of capability strings")
        })?;
        let mut capability = CapabilitySet::empty();
        for name in names {
            let text = name.as_str().ok_or_else(|| {
                consent_validation("capability", "each entry must be a capability string")
            })?;
            let parsed = text.parse::<Capability>().map_err(|error| {
                consent_validation(
                    "capability",
                    format!("unknown capability `{text}`: {error}"),
                )
            })?;
            capability = capability.with(parsed);
        }
        let digest_text = value
            .get("payload_sha256")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                consent_validation("payload_sha256", "must be a 64-digit SHA-256 hex string")
            })?;
        let payload_digest = parse_digest(digest_text)?;
        let nonce_text = value
            .get("nonce")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| consent_validation("nonce", "must be a hex nonce string"))?;
        let stripped = nonce_text
            .strip_prefix("0x")
            .or_else(|| nonce_text.strip_prefix("0X"))
            .unwrap_or(nonce_text);
        let padded = format!("{stripped:0>32}");
        let nonce = u128::from_str_radix(&padded, 16)
            .map_err(|_| consent_validation("nonce", "must be 32 hex digits"))?;
        Ok(Self::mint(
            correlation,
            subject,
            method,
            capability,
            payload_digest,
            nonce,
        ))
    }
}

/// Computes the digest of the operation parameters, excluding consent transport fields.
///
/// # Errors
///
/// Returns [`ErrorKind::Validation`] when `params` is neither an object nor `null`, and
/// [`ErrorKind::Codec`] when the normalized value cannot be encoded.
pub fn consent_payload_digest(params: &serde_json::Value) -> Result<[u8; 32]> {
    use std::collections::BTreeMap;

    use sha2::{Digest, Sha256};

    let operation: BTreeMap<String, serde_json::Value> = match params {
        serde_json::Value::Null => BTreeMap::new(),
        serde_json::Value::Object(object) => object
            .iter()
            .filter(|(name, _)| name.as_str() != "consent_token" && name.as_str() != "consent")
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        _ => {
            return Err(consent_validation("params", "must be an object, or absent"));
        }
    };
    let encoded = serde_json::to_vec(&operation).map_err(|error| {
        NcError::new(ErrorKind::Codec {
            operation: "security::authz::consent_payload_digest".to_owned(),
            reason: error.to_string(),
        })
    })?;
    Ok(Sha256::digest(encoded).into())
}

/// Validates the bounded identity fields recorded by a consent token.
///
/// # Errors
///
/// Returns [`ErrorKind::Validation`] when the subject or method is blank or too long.
pub fn validate_consent_binding(subject: &str, method: &str) -> Result<()> {
    if subject.trim().is_empty() || subject.len() > MAX_CONSENT_SUBJECT_BYTES {
        return Err(consent_validation(
            "subject",
            format!("must be non-empty and at most {MAX_CONSENT_SUBJECT_BYTES} bytes"),
        ));
    }
    if method.trim().is_empty() || method.len() > MAX_CONSENT_METHOD_BYTES {
        return Err(consent_validation(
            "method",
            format!("must be non-empty and at most {MAX_CONSENT_METHOD_BYTES} bytes"),
        ));
    }
    Ok(())
}

fn consent_validation(field: &str, reason: impl Into<String>) -> NcError {
    NcError::new(ErrorKind::Validation {
        field: format!("consent_token.{field}"),
        reason: reason.into(),
    })
    .operation(OP_CONSENT_PARSE)
}

fn hex_digest(digest: &[u8; 32]) -> String {
    use core::fmt::Write as _;

    digest.iter().fold(String::new(), |mut encoded, byte| {
        let _ = write!(encoded, "{byte:02x}");
        encoded
    })
}

fn parse_digest(text: &str) -> Result<[u8; 32]> {
    let stripped = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    if stripped.len() != 64 || !stripped.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(consent_validation(
            "payload_sha256",
            "must be a 64-digit SHA-256 hex string",
        ));
    }
    let mut digest = [0u8; 32];
    let (pairs, _) = stripped.as_bytes().as_chunks::<2>();
    for (byte, pair) in digest.iter_mut().zip(pairs) {
        let text = core::str::from_utf8(pair)
            .map_err(|error| consent_validation("payload_sha256", error.to_string()))?;
        *byte = u8::from_str_radix(text, 16)
            .map_err(|error| consent_validation("payload_sha256", error.to_string()))?;
    }
    Ok(digest)
}

impl fmt::Debug for ConsentToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConsentToken")
            .field("correlation", &self.correlation)
            .field("subject", &self.subject)
            .field("method", &self.method)
            .field("capability", &self.capability)
            .field("payload_digest", &hex_digest(&self.payload_digest))
            .field("nonce", &" <redacted>")
            .finish()
    }
}

/// A named bundle of standing capability grants (spec 36).
///
/// The role is the *starting point* for authorization, not the authorization itself: a
/// deployment may grant a session a set that is narrower than, equal to, or wider than
/// its role, and [`Authorizer::authorize`] decides on the set it is actually given. What
/// the role fixes is the **default** and, more importantly, the shape of the escalation
/// ladder — each role is exactly the previous role plus the powers its job needs, so a
/// session's authority can be reasoned about by naming a level.
///
/// # Grants and rationale
///
/// The ladder is monotonic: `Observer ⊆ Debugger ⊆ Tester ⊆ Developer ⊆ Administrator`.
/// That property is asserted by a test, because a role table that is not monotonic is
/// indistinguishable from a confused table.
///
/// * **`Observer` — agent-scope diagnostics only.**
///   Grants `read.logs`, `read.metrics`, `inspect.runtime`, `inspect.gpu`. These are the
///   four capabilities that let an agent *explain* what the runtime is doing without
///   being able to change anything. It deliberately does **not** get `filesystem.read`:
///   the diagnostics surface is already the project's structured state, and a blank
///   general read grant would turn "read the panic message" into "read
///   `~/.ssh/id_rsa`". It gets no workspace-scope capability and nothing
///   [`Capability::is_destructive`], which is the property that makes the role safe to
///   auto-provision for an untrusted UI.
///
/// * **`Debugger` — `Observer` plus inspection that needs to see the world as the
///   process sees it.** Adds `filesystem.read` and `debug`. Attaching a debugger and
///   inspecting process state is impossible without reading the files and binaries being
///   debugged, so these two are the minimum coherent grant; the alternative (an agent
///   that can read structured logs but not the source they describe) cannot do its job.
///   `debug` is runtime-scope and not destructive, so this role still holds nothing that
///   can change state, and it still grants no workspace-scope capability.
///
/// * **`Tester` — `Debugger` plus execution of the project's own suites.** Adds
///   `filesystem.write`, `run.tests`, `run.benchmarks`. Writing is granted because a test
///   run produces artifacts (`target/`, snapshots, coverage files); a Tester that could
///   not write would fail every run. `process.spawn` is **not** granted: `run.tests` and
///   `run.benchmarks` are the mediated ways to execute code, and granting raw spawn as
///   well would let a Tester run anything, which is the entire escalation the role
///   ladder exists to prevent. `filesystem.write` and `process.spawn` are both
///   [`Capability::is_destructive`], so every Tester mutation is subject to the per-call
///   consent gate.
///
/// * **`Developer` — `Tester` plus authority over the project source.** Adds
///   `modify.source`, `install.dependencies`, `network`, `notifications`. Source edits
///   and dependency changes are what "do the work" means; `network` is included because
///   `install.dependencies` cannot function without it, and withholding it while granting
///   the capability that needs it would be incoherent. `deploy` and `delete.data` are
///   **withheld**: a Developer is expected to change code, not to publish it or destroy
///   it. Publishing is a separate, higher-consequence decision and is the Administrator's.
///
/// * **`Administrator` — everything.** The full set, including `deploy` and `delete.data`.
///   This is the only role from which irreversible operations are reachable at all, and
///   because `deploy`/`delete.data`/`filesystem.write`/`process.spawn` are destructive,
///   even an Administrator still has to pass the per-call consent gate. "Administrator" is
///   therefore *not* "can delete anything unattended", which is the property that keeps a
///   compromised admin session from being a data-loss incident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum AgentRole {
    /// Read-only diagnostics. See the type documentation.
    Observer,
    /// Diagnostics plus filesystem reads and debugger attachment. See the type
    /// documentation.
    Debugger,
    /// Debugger plus writing artifacts and running the project's suites. See the type
    /// documentation.
    Tester,
    /// Tester plus source, dependency and network authority. See the type documentation.
    Developer,
    /// The complete capability set. See the type documentation.
    Administrator,
}

impl AgentRole {
    /// Every role, from least to most privileged.
    ///
    /// Declaration order is the escalation order, so `ALL.windows(2)` yields the first
    /// three levels and iteration is stable across processes and releases.
    pub const ALL: &'static [AgentRole] = &[
        AgentRole::Observer,
        AgentRole::Debugger,
        AgentRole::Tester,
        AgentRole::Developer,
        AgentRole::Administrator,
    ];

    /// The stable string form used on the wire and in configuration.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observer => "observer",
            Self::Debugger => "debugger",
            Self::Tester => "tester",
            Self::Developer => "developer",
            Self::Administrator => "administrator",
        }
    }

    /// The standing capabilities this role carries.
    ///
    /// Const so the grant table is a compile-time constant: a role's authority cannot be
    /// changed by a runtime misconfiguration, and there is no path by which a partially
    /// initialized global could hand out an empty or widened set.
    #[must_use]
    pub const fn capabilities(self) -> CapabilitySet {
        match self {
            // Agent-scope diagnostics only: explain, never change. No filesystem read,
            // no workspace scope, nothing destructive.
            Self::Observer => CapabilitySet::empty()
                .with(Capability::ReadLogs)
                .with(Capability::ReadMetrics)
                .with(Capability::InspectRuntime)
                .with(Capability::InspectGpu),

            // Adds what inspecting the process as the process sees it requires.
            Self::Debugger => CapabilitySet::empty()
                .with(Capability::ReadLogs)
                .with(Capability::ReadMetrics)
                .with(Capability::InspectRuntime)
                .with(Capability::InspectGpu)
                .with(Capability::FilesystemRead)
                .with(Capability::Debug),

            // Adds artifact writes and mediated execution. `process.spawn` stays out:
            // `run.tests`/`run.benchmarks` are the sanctioned execution path.
            Self::Tester => CapabilitySet::empty()
                .with(Capability::ReadLogs)
                .with(Capability::ReadMetrics)
                .with(Capability::InspectRuntime)
                .with(Capability::InspectGpu)
                .with(Capability::FilesystemRead)
                .with(Capability::Debug)
                .with(Capability::FilesystemWrite)
                .with(Capability::RunTests)
                .with(Capability::RunBenchmarks),

            // Adds the authority to change the project. `deploy`/`delete.data` withheld.
            Self::Developer => CapabilitySet::empty()
                .with(Capability::ReadLogs)
                .with(Capability::ReadMetrics)
                .with(Capability::InspectRuntime)
                .with(Capability::InspectGpu)
                .with(Capability::FilesystemRead)
                .with(Capability::Debug)
                .with(Capability::FilesystemWrite)
                .with(Capability::RunTests)
                .with(Capability::RunBenchmarks)
                .with(Capability::ModifySource)
                .with(Capability::InstallDependencies)
                .with(Capability::Network)
                .with(Capability::Notifications),

            // Everything. Still gated per call for destructive operations.
            Self::Administrator => CapabilitySet::all(),
        }
    }

    /// Whether this role's standing grant includes a destructive capability.
    ///
    /// Convenience for call sites that must decide up front whether the per-call consent
    /// gate in [`Authorizer::require_destructive`] will apply.
    #[must_use]
    pub fn has_destructive(self) -> bool {
        self.capabilities().has_destructive()
    }

    /// The scopes this role's grant spans, for diagnostics and `ncui describe`.
    #[must_use]
    pub fn scopes(self) -> Vec<CapabilityScope> {
        let capabilities = self.capabilities();
        [
            CapabilityScope::Runtime,
            CapabilityScope::Workspace,
            CapabilityScope::Agent,
        ]
        .into_iter()
        .filter(|scope| !capabilities.in_scope(*scope).is_empty())
        .collect()
    }
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Decides whether a subject may exercise a required capability set.
///
/// The `Authorizer` is deliberately stateless and holds no policy: both the required and
/// the granted set arrive as arguments. That makes the decision a pure function of its
/// inputs, which means it can be recorded in an [`AuditEvent`](crate::audit::AuditEvent)
/// and replayed later to prove *why* a call was allowed, and it means there is no mutable
/// global that a bug elsewhere in the runtime can widen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Authorizer;

impl Authorizer {
    /// Creates an authorizer.
    ///
    /// `Authorizer` is a zero-sized decision function; there is no per-instance state to
    /// configure. It exists as a type so the gate has a name in the source and in traces.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Whether `required` is a subset of `granted`, ignoring per-call consent.
    ///
    /// The non-`Result` form of [`Authorizer::authorize`], for callers that need the
    /// predicate itself (a UI deciding whether to render a button, a policy cache). It is
    /// *not* an authorization decision on its own: a `true` here says nothing about the
    /// per-call consent gate.
    #[must_use]
    pub const fn permits(&self, required: CapabilitySet, granted: CapabilitySet) -> bool {
        required.is_subset_of(granted)
    }

    /// Authorizes `required` for `subject` against `granted`.
    ///
    /// Succeeds when `required.is_subset_of(granted)` and fails with
    /// [`ErrorKind::CapabilityDenied`] otherwise. When several capabilities are missing,
    /// the *first in declaration order* is reported: the denial has to name one capability
    /// because that is what the agent API surfaces, and declaration order makes the
    /// choice deterministic rather than dependent on bit layout.
    ///
    /// Note what this does **not** check: nothing here looks at `Capability::is_destructive`.
    /// Use [`Authorizer::require_destructive`] at any call site that can mutate state,
    /// mutate the workspace, publish, or destroy.
    ///
    /// # Errors
    ///
    /// * [`ErrorKind::Validation`] if `subject` is empty. An anonymous subject would make
    ///   the denial unauditable — an `AuditEvent` with no `agent_id` cannot be attributed
    ///   — so the identity is required input, not a formatting detail.
    /// * [`ErrorKind::CapabilityDenied`] if `required` is not a subset of `granted`.
    pub fn authorize(
        &self,
        subject: &str,
        required: CapabilitySet,
        granted: CapabilitySet,
    ) -> Result<()> {
        validate_subject(subject, OP_AUTHORIZE)?;
        if self.permits(required, granted) {
            return Ok(());
        }
        Err(missing_capability(subject, required, granted, OP_AUTHORIZE))
    }

    /// Authorizes `required` for `subject`, additionally requiring per-call consent when
    /// any part of `required` is destructive (spec 88).
    ///
    /// The gates run in the order *capability first, consent second*, and for a reason:
    /// the caller must not be able to distinguish "you do not hold this" from "you hold
    /// this but nobody approved this call" by probing. More practically, a destructive
    /// capability the subject does not hold is denied by [`Authorizer::authorize`]
    /// regardless of `authorized`, so a bug that hardcodes `authorized = true` cannot
    /// manufacture authority that was never granted.
    ///
    /// `authorized` is a plain `bool` and is safe only for low-level callers that already
    /// hold a real approval signal. Runtime request paths use
    /// [`Authorizer::require_destructive_with_token`], which takes the runtime-minted
    /// [`ConsentToken`] and fails closed whenever the token is absent or mismatched. A bare
    /// `consent: true` in a JSON-RPC request authorizes nothing.
    ///
    /// # Errors
    ///
    /// * [`ErrorKind::Validation`] if `subject` is empty.
    /// * [`ErrorKind::CapabilityDenied`] if `required` is not a subset of `granted`, or if
    ///   `required` contains a destructive capability and `authorized` is `false`. In the
    ///   latter case the named capability is the first destructive one in `required`, so
    ///   the message points at the authority that needs consent.
    pub fn require_destructive(
        &self,
        subject: &str,
        required: CapabilitySet,
        granted: CapabilitySet,
        authorized: bool,
    ) -> Result<()> {
        self.authorize(subject, required, granted)?;
        if !authorized && let Some(capability) = first_destructive(required) {
            return Err(NcError::new(ErrorKind::CapabilityDenied {
                capability: capability.as_str().to_owned(),
                subject: subject.to_owned(),
            })
            .operation(OP_REQUIRE_DESTRUCTIVE));
        }
        Ok(())
    }

    /// Authorizes `required` for `subject`, requiring a minted [`ConsentToken`]
    /// whenever any part of `required` is destructive.
    ///
    /// The token must match the authenticated `subject`, logical-operation
    /// `correlation`, `method`, `required` capabilities and exact parameter digest.
    /// When it does not — or when no token is supplied for a destructive capability —
    /// the call is denied. The runtime ledger remains responsible for proving that the
    /// nonce was minted here and has not already been spent.
    ///
    /// # Errors
    ///
    /// * [`ErrorKind::Validation`] if `subject` is empty.
    /// * [`ErrorKind::CapabilityDenied`] if `required` is not a subset of
    ///   `granted`, or if `required` is destructive and `token` is `None` or does
    ///   not match.
    #[allow(
        clippy::too_many_arguments,
        reason = "the authorization boundary takes the complete consent binding explicitly"
    )]
    pub fn require_destructive_with_token(
        &self,
        subject: &str,
        required: CapabilitySet,
        granted: CapabilitySet,
        token: Option<&ConsentToken>,
        correlation: CorrelationId,
        method: &str,
        payload_digest: [u8; 32],
    ) -> Result<()> {
        self.authorize(subject, required, granted)?;
        if let Some(destructive) = first_destructive(required) {
            let valid = token.is_some_and(|token| {
                token.matches(subject, correlation, method, required, payload_digest)
            });
            if !valid {
                return Err(NcError::new(ErrorKind::CapabilityDenied {
                    capability: destructive.as_str().to_owned(),
                    subject: subject.to_owned(),
                })
                .operation(OP_REQUIRE_TOKEN));
            }
        }
        Ok(())
    }
}

/// Rejects an empty subject identifier.
///
/// A denial that cannot name its subject cannot be audited, and an audit trail that
/// cannot name its subject cannot answer "who did this" — which is the only question the
/// trail exists to answer (spec 37).
fn validate_subject(subject: &str, operation: &'static str) -> Result<()> {
    if subject.trim().is_empty() {
        return Err(NcError::new(ErrorKind::Validation {
            field: "subject".to_owned(),
            reason: "the authorizing subject must be a non-empty identifier; an anonymous \
                     denial cannot be audited"
                .to_owned(),
        })
        .operation(operation));
    }
    Ok(())
}

/// Builds the `CapabilityDenied` error for the first capability in `required` that is
/// absent from `granted`.
///
/// Purely declaration order, with no preference for destructive capabilities. A
/// preference would arguably point at the more actionable capability, but it would make
/// the reported capability depend on a classification rather than on the request, and an
/// error message that changes meaning with an unrelated code edit is worse than one that
/// is boringly predictable.
fn missing_capability(
    subject: &str,
    required: CapabilitySet,
    granted: CapabilitySet,
    operation: &'static str,
) -> NcError {
    let capability = first_missing(required, granted).unwrap_or(Capability::Debug);
    NcError::new(ErrorKind::CapabilityDenied {
        capability: capability.as_str().to_owned(),
        subject: subject.to_owned(),
    })
    .operation(operation)
}

/// The first capability in `required` that `granted` does not hold, in declaration order.
fn first_missing(required: CapabilitySet, granted: CapabilitySet) -> Option<Capability> {
    required
        .iter()
        .find(|capability| !granted.contains(*capability))
}

/// The first *destructive* capability in `required`, in declaration order.
fn first_destructive(required: CapabilitySet) -> Option<Capability> {
    required
        .iter()
        .find(|capability| capability.is_destructive())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::ErrorKind;

    /// The observer set is the safety floor: no workspace scope, nothing destructive.
    #[test]
    fn observer_has_no_workspace_scope_and_nothing_destructive() {
        let observer = AgentRole::Observer.capabilities();
        assert!(
            observer.in_scope(CapabilityScope::Workspace).is_empty(),
            "observer must not hold workspace-scope authority: {observer}"
        );
        assert!(
            !observer.has_destructive(),
            "observer must not hold destructive authority: {observer}"
        );
        // It should still be able to do something.
        assert!(observer.contains(Capability::ReadLogs));
        assert_eq!(observer.len(), 4);
    }

    #[test]
    fn role_ladder_is_monotonic() {
        for window in AgentRole::ALL.windows(2) {
            let (lower, higher) = (window[0].capabilities(), window[1].capabilities());
            assert!(
                lower.is_subset_of(higher),
                "{} is not a subset of {}",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn only_administrator_holds_the_irreversible_capabilities() {
        for role in AgentRole::ALL {
            let capabilities = role.capabilities();
            assert_eq!(
                capabilities.contains(Capability::Deploy),
                *role == AgentRole::Administrator,
                "deploy reachable from {role}"
            );
            assert_eq!(
                capabilities.contains(Capability::DeleteData),
                *role == AgentRole::Administrator,
                "delete.data reachable from {role}"
            );
        }
        assert!(
            AgentRole::Administrator
                .capabilities()
                .is_subset_of(CapabilitySet::all())
        );
        assert_eq!(
            AgentRole::Administrator.capabilities(),
            CapabilitySet::all()
        );
    }

    #[test]
    fn tester_can_execute_without_holding_raw_spawn() {
        let tester = AgentRole::Tester.capabilities();
        assert!(tester.contains(Capability::RunTests));
        assert!(tester.contains(Capability::RunBenchmarks));
        assert!(
            !tester.contains(Capability::ProcessSpawn),
            "tester must not hold raw process.spawn; run.tests is the mediated path"
        );
    }

    #[test]
    fn developer_cannot_publish_or_destroy() {
        let developer = AgentRole::Developer.capabilities();
        assert!(developer.contains(Capability::ModifySource));
        assert!(developer.contains(Capability::InstallDependencies));
        assert!(!developer.contains(Capability::Deploy));
        assert!(!developer.contains(Capability::DeleteData));
    }

    #[test]
    fn role_names_are_stable_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for role in AgentRole::ALL {
            assert!(
                seen.insert(role.as_str()),
                "duplicate role name for {role:?}"
            );
            assert_eq!(role.to_string(), role.as_str());
            let name = role.as_str();
            let parsed = AgentRole::ALL
                .iter()
                .copied()
                .find(|candidate| candidate.as_str() == name)
                .expect("a role name must round-trip to its role");
            assert_eq!(parsed, *role);
        }
        assert_eq!(seen.len(), AgentRole::ALL.len());
        assert!("root".parse_role_name().is_err());
        assert_eq!("root".parse_role_name().unwrap_err().code(), "validation");
    }

    /// The name-to-role lookup the `ncui` CLI needs for `--role`.
    trait ParseRoleName {
        fn parse_role_name(&self) -> Result<AgentRole>;
    }
    impl ParseRoleName for &str {
        fn parse_role_name(&self) -> Result<AgentRole> {
            AgentRole::ALL
                .iter()
                .copied()
                .find(|role| role.as_str() == *self)
                .ok_or_else(|| {
                    NcError::new(ErrorKind::Validation {
                        field: "role".to_owned(),
                        reason: format!(
                            "unknown agent role {self:?}; expected one of {:?}",
                            AgentRole::ALL
                        ),
                    })
                    .operation("security::authz::AgentRole::from_str")
                })
        }
    }

    #[test]
    fn authorize_grants_a_subset_and_denies_a_superset() {
        let authorizer = Authorizer::new();
        let granted = CapabilitySet::empty().with(Capability::FilesystemRead);

        assert!(authorizer.authorize("agent-1", granted, granted).is_ok());
        assert!(
            authorizer
                .authorize("agent-1", CapabilitySet::empty(), granted)
                .is_ok()
        );

        let required = CapabilitySet::empty()
            .with(Capability::FilesystemRead)
            .with(Capability::Network);
        let err = authorizer
            .authorize("agent-1", required, granted)
            .expect_err("network is not granted");
        assert!(
            matches!(
                err.kind(),
                ErrorKind::CapabilityDenied { capability, subject }
                    if capability == "network" && subject == "agent-1"
            ),
            "got {err:?}"
        );
        assert_eq!(err.operation_name(), OP_AUTHORIZE);
    }

    #[test]
    fn authorize_reports_the_first_missing_capability_in_declaration_order() {
        let authorizer = Authorizer::new();
        // Declaration order is filesystem.read … network … clipboard … read.logs, so
        // `network` precedes `clipboard` even though the request lists clipboard first,
        // and `read.logs` follows both.
        let required = CapabilitySet::empty()
            .with(Capability::ReadLogs)
            .with(Capability::Clipboard)
            .with(Capability::Network);
        let err = authorizer
            .authorize("agent-1", required, CapabilitySet::empty())
            .expect_err("nothing granted");
        assert!(
            matches!(err.kind(), ErrorKind::CapabilityDenied { capability, .. }
                if capability == "network"),
            "expected the declaration-order-first missing capability, got {err:?}"
        );

        // Narrow the request so a later capability becomes the first missing one: grant
        // everything except read.logs and clipboard.
        let required = CapabilitySet::empty()
            .with(Capability::ReadLogs)
            .with(Capability::Clipboard);
        let granted = CapabilitySet::all()
            .without(Capability::ReadLogs)
            .without(Capability::Clipboard);
        let err = authorizer
            .authorize("agent-1", required, granted)
            .expect_err("neither read.logs nor clipboard is granted");
        assert!(
            matches!(err.kind(), ErrorKind::CapabilityDenied { capability, .. }
                if capability == "clipboard"),
            "clipboard precedes read.logs in declaration order; got {err:?}"
        );
    }

    #[test]
    fn authorize_requires_an_identifiable_subject() {
        let authorizer = Authorizer::new();
        for subject in ["", "   ", "\t\n"] {
            let err = authorizer
                .authorize(subject, CapabilitySet::empty(), CapabilitySet::all())
                .expect_err("anonymous subject must be rejected");
            assert!(
                matches!(err.kind(), ErrorKind::Validation { .. }),
                "got {err:?}"
            );
        }
    }

    /// The load-bearing case from spec 88: holding the capability is not consent.
    #[test]
    fn destructive_capability_without_per_call_authorization_is_denied() {
        let authorizer = Authorizer::new();
        let granted = AgentRole::Administrator.capabilities();
        assert!(granted.contains(Capability::DeleteData));

        let destructive = CapabilitySet::empty().with(Capability::DeleteData);

        let err = authorizer
            .require_destructive("admin-1", destructive, granted, false)
            .expect_err("unauthorized destructive call must be denied");
        assert!(
            matches!(err.kind(), ErrorKind::CapabilityDenied { capability, subject }
                if capability == "delete.data" && subject == "admin-1"),
            "got {err:?}"
        );
        assert_eq!(err.operation_name(), OP_REQUIRE_DESTRUCTIVE);

        // The identical call with consent succeeds: the gate is the flag, not the role.
        assert!(
            authorizer
                .require_destructive("admin-1", destructive, granted, true)
                .is_ok()
        );
    }

    #[test]
    fn non_destructive_calls_do_not_require_the_consent_flag() {
        let authorizer = Authorizer::new();
        let granted = AgentRole::Observer.capabilities();
        let required = CapabilitySet::empty().with(Capability::ReadLogs);
        assert!(
            authorizer
                .require_destructive("observer-1", required, granted, false)
                .is_ok()
        );
    }

    /// Consent must not be able to manufacture authority that was never granted.
    #[test]
    fn consent_flag_cannot_substitute_for_a_missing_capability() {
        let authorizer = Authorizer::new();
        let granted = AgentRole::Observer.capabilities();
        let required = CapabilitySet::empty().with(Capability::DeleteData);
        let err = authorizer
            .require_destructive("observer-1", required, granted, true)
            .expect_err("delete.data is not granted to an observer, consent or not");
        assert!(
            matches!(err.kind(), ErrorKind::CapabilityDenied { capability, .. }
                if capability == "delete.data"),
            "got {err:?}"
        );
    }

    #[test]
    fn mixed_sets_are_gated_on_their_destructive_part_only() {
        let authorizer = Authorizer::new();
        let granted = AgentRole::Tester.capabilities();
        let required = CapabilitySet::empty()
            .with(Capability::ReadLogs)
            .with(Capability::FilesystemWrite);

        // The read alone would pass without consent; adding the write does not.
        let read_only = CapabilitySet::empty().with(Capability::ReadLogs);
        assert!(
            authorizer
                .require_destructive("tester-1", read_only, granted, false)
                .is_ok()
        );
        assert!(
            authorizer
                .require_destructive("tester-1", required, granted, false)
                .is_err()
        );
        assert!(
            authorizer
                .require_destructive("tester-1", required, granted, true)
                .is_ok()
        );
    }

    /// A session granted a *subset* of its role is denied the rest — the role is a
    /// default, never an ambient grant.
    #[test]
    fn a_narrowed_grant_is_enforced_over_the_role_default() {
        let authorizer = Authorizer::new();
        let role = AgentRole::Developer;
        let narrowed = role.capabilities().without(Capability::Network);
        let required = CapabilitySet::empty().with(Capability::InstallDependencies);
        assert!(role.capabilities().contains(Capability::Network));
        assert!(
            authorizer
                .authorize("dev-1", required, role.capabilities())
                .is_ok()
        );
        assert!(authorizer.authorize("dev-1", required, narrowed).is_ok());
        let network = CapabilitySet::empty().with(Capability::Network);
        assert!(authorizer.authorize("dev-1", network, narrowed).is_err());
    }

    #[test]
    fn permits_is_the_non_fallible_predicate() {
        let authorizer = Authorizer::new();
        assert!(authorizer.permits(CapabilitySet::empty(), CapabilitySet::empty()));
        assert!(authorizer.permits(CapabilitySet::all(), CapabilitySet::all()));
        assert!(!authorizer.permits(CapabilitySet::all(), CapabilitySet::empty()));
    }

    #[test]
    fn scopes_reported_per_role_match_the_grant_table() {
        assert_eq!(AgentRole::Observer.scopes(), vec![CapabilityScope::Agent]);
        assert_eq!(
            AgentRole::Debugger.scopes(),
            vec![CapabilityScope::Runtime, CapabilityScope::Agent]
        );
        assert_eq!(
            AgentRole::Tester.scopes(),
            vec![CapabilityScope::Runtime, CapabilityScope::Agent]
        );
        assert_eq!(
            AgentRole::Developer.scopes(),
            vec![
                CapabilityScope::Runtime,
                CapabilityScope::Workspace,
                CapabilityScope::Agent
            ]
        );
        assert_eq!(AgentRole::Administrator.scopes().len(), 3);
    }

    #[test]
    fn has_destructive_agrees_with_the_grant_table() {
        for role in AgentRole::ALL {
            assert_eq!(
                role.has_destructive(),
                role.capabilities().has_destructive(),
                "disagreement for {role}"
            );
        }
        assert!(!AgentRole::Observer.has_destructive());
        assert!(!AgentRole::Debugger.has_destructive());
        assert!(AgentRole::Tester.has_destructive());
        assert!(AgentRole::Administrator.has_destructive());
    }

    #[test]
    fn a_token_for_method_a_does_not_authorize_method_b() {
        let authorizer = Authorizer::new();
        let granted = AgentRole::Administrator.capabilities();
        let required = CapabilitySet::empty().with(Capability::DeleteData);
        let correlation = CorrelationId::new();
        let digest = consent_payload_digest(&serde_json::json!({ "path": "source" }))
            .expect("operation digest");
        let token = ConsentToken::mint(
            correlation,
            "admin-1",
            "ncui.data.purge",
            required,
            digest,
            0x1234_5678_9abc_def0_1234_5678_9abc_def0,
        );
        assert!(
            authorizer
                .require_destructive_with_token(
                    "admin-1",
                    required,
                    granted,
                    Some(&token),
                    correlation,
                    "ncui.data.purge",
                    digest,
                )
                .is_ok()
        );
        let err = authorizer
            .require_destructive_with_token(
                "admin-1",
                required,
                granted,
                Some(&token),
                correlation,
                "ncui.logs.purge",
                digest,
            )
            .expect_err("a token must not authorize a different method");
        assert_eq!(err.operation_name(), OP_REQUIRE_TOKEN);
    }

    #[test]
    fn a_token_bound_to_a_different_correlation_is_refused() {
        let authorizer = Authorizer::new();
        let granted = AgentRole::Administrator.capabilities();
        let required = CapabilitySet::empty().with(Capability::DeleteData);
        let minted = CorrelationId::new();
        let other = CorrelationId::new();
        let digest = consent_payload_digest(&serde_json::json!({})).expect("operation digest");
        let token = ConsentToken::mint(minted, "admin-1", "ncui.data.purge", required, digest, 1);
        assert!(
            authorizer
                .require_destructive_with_token(
                    "admin-1",
                    required,
                    granted,
                    Some(&token),
                    other,
                    "ncui.data.purge",
                    digest,
                )
                .is_err(),
            "a token must not authorize a different correlation"
        );
    }

    #[test]
    fn a_destructive_call_without_a_token_is_refused() {
        let authorizer = Authorizer::new();
        let granted = AgentRole::Administrator.capabilities();
        let required = CapabilitySet::empty().with(Capability::DeleteData);
        let err = authorizer
            .require_destructive_with_token(
                "admin-1",
                required,
                granted,
                None,
                CorrelationId::new(),
                "ncui.data.purge",
                [0; 32],
            )
            .expect_err("a destructive call without a token must be refused");
        assert_eq!(err.operation_name(), OP_REQUIRE_TOKEN);
    }

    #[test]
    fn a_token_cannot_transfer_to_another_subject_or_altered_arguments() {
        let authorizer = Authorizer::new();
        let granted = AgentRole::Administrator.capabilities();
        let required = CapabilitySet::empty().with(Capability::DeleteData);
        let correlation = CorrelationId::new();
        let digest = consent_payload_digest(&serde_json::json!({ "path": "source" }))
            .expect("operation digest");
        let altered = consent_payload_digest(&serde_json::json!({ "path": "home" }))
            .expect("altered operation digest");
        let token = ConsentToken::mint(
            correlation,
            "admin-1",
            "ncui.data.purge",
            required,
            digest,
            7,
        );
        assert!(
            authorizer
                .require_destructive_with_token(
                    "admin-2",
                    required,
                    granted,
                    Some(&token),
                    correlation,
                    "ncui.data.purge",
                    digest,
                )
                .is_err()
        );
        assert!(
            authorizer
                .require_destructive_with_token(
                    "admin-1",
                    required,
                    granted,
                    Some(&token),
                    correlation,
                    "ncui.data.purge",
                    altered,
                )
                .is_err()
        );
    }

    #[test]
    fn operation_digests_are_canonical_and_ignore_consent_transport_fields() {
        let first = consent_payload_digest(&serde_json::json!({
            "b": 2,
            "a": 1,
            "consent": true,
            "consent_token": { "nonce": "ignored" }
        }))
        .expect("first digest");
        let second = consent_payload_digest(&serde_json::json!({
            "a": 1,
            "b": 2
        }))
        .expect("second digest");
        assert_eq!(first, second);
        assert_ne!(
            first,
            consent_payload_digest(&serde_json::json!({ "a": 1, "b": 3 })).expect("altered digest")
        );
        assert!(consent_payload_digest(&serde_json::json!([1, 2])).is_err());
    }

    #[test]
    fn a_consent_token_round_trips_through_its_wire_form() {
        let correlation = CorrelationId::new();
        let required = CapabilitySet::empty()
            .with(Capability::DeleteData)
            .with(Capability::ReadLogs);
        let digest = consent_payload_digest(&serde_json::json!({ "scope": "all" }))
            .expect("operation digest");
        let token = ConsentToken::mint(
            correlation,
            "admin-1",
            "ncui.data.purge",
            required,
            digest,
            0xbeef,
        );
        let value = token.to_value();
        let back = ConsentToken::from_value(&value).expect("a minted token parses back");
        assert_eq!(back.correlation(), correlation);
        assert_eq!(back.subject(), "admin-1");
        assert_eq!(back.method(), "ncui.data.purge");
        assert_eq!(back.capability(), required);
        assert_eq!(back.payload_digest(), digest);
        assert_eq!(back.nonce(), 0xbeef);
    }
}
