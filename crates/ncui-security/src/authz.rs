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
//! 2. **Per-call consent** — [`Authorizer::require_destructive`] answers "even though the
//!    subject holds the capability, has the *user* approved this specific invocation?".
//!
//! Spec 88 requires the second gate. The reason is that a capability grant is a *standing*
//! authorization: it stays valid for the whole session, including for code paths, agent
//! plans and tool invocations that did not exist when the grant was made. Destructive
//! operations are precisely the ones where that gap is unacceptable — a grant of
//! `delete.data` obtained for "clean up stale build output" would otherwise silently
//! authorize deleting the user's source. Requiring an explicit per-call flag means a
//! destructive call fails *closed* when the surrounding consent plumbing is missing,
//! broken, or bypassed. It converts "we forgot to ask" from silent data loss into a
//! denial.
//!
//! The [`Authorizer`] is a zero-sized, stateless decision function. It holds no policy
//! store and no clock: the caller supplies the granted set and the consent flag, so the
//! decision is reproducible from its arguments alone and can be replayed in an audit.

use core::fmt;
use ncui_core::capability::CapabilityScope;
use ncui_core::{Capability, CapabilitySet, ErrorKind, NcError, Result};

/// The operation name recorded on authorization failures.
const OP_AUTHORIZE: &str = "security::authz::authorize";
/// The operation name recorded when a destructive call lacks per-call consent.
const OP_REQUIRE_DESTRUCTIVE: &str = "security::authz::require_destructive";

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
    /// `authorized` is a plain `bool` and that is a deliberate weakness of the signature:
    /// a caller who writes `true` unconditionally has opted out. The alternative — an
    /// unforgeable consent token minted by the runtime at the point of user
    /// confirmation — is the right long-term design and belongs in the runtime, not in a
    /// pure predicate. Until then the gate is correct for every caller that passes a real
    /// consent signal, and it fails closed (`false` is the default for any code path that
    /// cannot produce a signal at all).
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
}
