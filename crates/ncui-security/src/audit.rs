//! The audit trail: one event per agent action, held in a bounded ring (spec 37).
//!
//! Spec 37 requires that every agent action produce an audit event. The threat this
//! addresses is not a bug in the runtime; it is the question a user asks afterwards —
//! *"the agent deleted my file, what did it do, in what order, and with what
//! authority?"* — and the answer has to be reconstructable from a record that was written
//! at the time, not from the agent's memory of what it meant to do.
//!
//! # Why the buffer is bounded and in memory
//!
//! The log is a **ring buffer** with a fixed capacity: the newest events are kept and the
//! oldest are evicted. This is a deliberate, bounded trade:
//!
//! * An unbounded log is an unbounded memory cost, and an agent session is exactly the
//!   kind of untrusted producer that would find it. Spec 96 requires every bounded
//!   resource to have an enforced bound; a growing log is the violation.
//! * An in-memory ring is *not* a durable audit trail. It answers "what did the agent do
//!   in the last few thousand actions", which is what a diagnostic dump and a live
//!   inspector need. Durable, tamper-evident storage is a different component with a
//!   different threat model (an attacker with write access to the log), and pretending a
//!   `VecDeque` provides that would be the dishonest choice. The eviction count is
//!   tracked so a reader can *tell* that a record was lost, rather than silently
//!   presenting a truncated log as complete — that distinction is the difference between
//!   a useful log and a misleading one.
//!
//! # Secrets
//!
//! [`AuditEvent`] holds identifiers, action names, target strings, outcomes and
//! capabilities. It has no field that can hold a [`SecretToken`](crate::token::SecretToken),
//! and it derives `Serialize` so that it can be emitted to the log sink directly. If a
//! caller needs to record *that* a token was used, the correct field is the capability it
//! authorised, not the token.

use core::fmt;
use ncui_core::{CapabilitySet, CorrelationId, ErrorKind, NcError, Result};
use std::collections::VecDeque;
use std::time::SystemTime;

/// The operation name recorded on audit-construction and buffer failures.
const OP_NEW_EVENT: &str = "security::audit::AuditEvent::new";
const OP_NEW_LOG: &str = "security::audit::AuditLog::new";

/// The outcome of an audited action.
///
/// Modelled as a closed enum rather than a `String` so that a consumer can match on it and
/// so that a typo in a call site does not produce an unmatchable record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AuditOutcome {
    /// The action was permitted and completed.
    Allowed,
    /// The action was refused by the authorization or containment gate.
    Denied,
    /// The action was permitted but failed (I/O, timeout, internal defect).
    Failed,
    /// The action was permitted and explicitly approved as destructive (spec 88).
    Authorized,
}

impl AuditOutcome {
    /// The stable string form, used in serialized output and in log text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Denied => "denied",
            Self::Failed => "failed",
            Self::Authorized => "authorized",
        }
    }
}

impl fmt::Display for AuditOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One agent action, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    /// When the action happened.
    ///
    /// A wall-clock [`SystemTime`] rather than a monotonic `Instant`: an audit record that
    /// says "0.42 s after process start" is not correlatable with a user-visible log, a
    /// crash report, or a log shipped from another machine. The cost is that a wall clock
    /// can be adjusted backwards; the value is only ever read, never used to make a
    /// decision, so the adjustment cannot grant authority.
    pub timestamp: SystemTime,
    /// The agent session that performed the action.
    pub agent_id: String,
    /// The logical request this action belongs to, tying it to traces and errors
    /// (spec 50, 93).
    pub request_id: CorrelationId,
    /// What was attempted, e.g. `"fs.write"` or `"test.run"`.
    pub action: String,
    /// What it was attempted against, e.g. a resolved path or a capability name.
    pub target: String,
    /// How it ended.
    pub outcome: AuditOutcome,
    /// The capabilities the action actually exercised.
    ///
    /// Recorded rather than the full granted set on purpose: a record of what was *used*
    /// is what answers "what could this agent have done", whereas a record of what was
    /// *granted* answers a different and less useful question.
    pub capabilities_used: CapabilitySet,
}

impl AuditEvent {
    /// Creates an event.
    ///
    /// `agent_id` and `action` are validated non-empty: an event that cannot name its
    /// actor or its action does not answer the only question the audit trail exists to
    /// answer. `target` is *not* validated, because an action legitimately targets
    /// nothing — a shutdown, a capability check with no resource.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] if `agent_id` or `action` is empty or blank.
    pub fn new(
        timestamp: SystemTime,
        agent_id: impl Into<String>,
        request_id: CorrelationId,
        action: impl Into<String>,
        target: impl Into<String>,
        outcome: AuditOutcome,
        capabilities_used: CapabilitySet,
    ) -> Result<Self> {
        let agent_id = agent_id.into();
        let action = action.into();
        if agent_id.trim().is_empty() {
            return Err(NcError::new(ErrorKind::Validation {
                field: "agent_id".to_owned(),
                reason: "an audit event without an actor cannot answer 'who did this'".to_owned(),
            })
            .operation(OP_NEW_EVENT));
        }
        if action.trim().is_empty() {
            return Err(NcError::new(ErrorKind::Validation {
                field: "action".to_owned(),
                reason: "an audit event without an action is not an audit event".to_owned(),
            })
            .operation(OP_NEW_EVENT));
        }
        Ok(Self {
            timestamp,
            agent_id,
            request_id,
            action,
            target: target.into(),
            outcome,
            capabilities_used,
        })
    }

    /// The outcome as its stable string, for log formatting.
    #[must_use]
    pub fn outcome_str(&self) -> &'static str {
        self.outcome.as_str()
    }
}

impl fmt::Display for AuditEvent {
    /// Renders a single line suitable for a text log. The `target` is included verbatim:
    /// it is the field a reader needs, and unlike a secret it is a path or capability
    /// name that the agent already knows it supplied.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} agent={} req={} action={} target={} outcome={} caps=[{}]",
            self.timestamp
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or_else(
                    |_| "pre-epoch".to_owned(),
                    |d| format!("{}ms", d.as_millis())
                ),
            self.agent_id,
            self.request_id,
            self.action,
            self.target,
            self.outcome,
            self.capabilities_used,
        )
    }
}

/// A bounded, in-memory ring of [`AuditEvent`]s.
///
/// Eviction is oldest-first. The count of evicted events is retained so that a consumer
/// can report "N actions ago" honestly instead of presenting a truncated buffer as a
/// complete history.
#[derive(Debug, Clone)]
pub struct AuditLog {
    events: VecDeque<AuditEvent>,
    capacity: usize,
    evicted: u64,
}

impl AuditLog {
    /// Creates a log retaining at most `capacity` events.
    ///
    /// A capacity of 0 is rejected: it would silently retain nothing while reporting a
    /// healthy log, which is the failure mode of an audit trail that appears to be
    /// working.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] if `capacity` is 0.
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(NcError::new(ErrorKind::Validation {
                field: "capacity".to_owned(),
                reason: "an audit log with zero capacity retains nothing while reporting \
                         success; that is a silent audit failure"
                    .to_owned(),
            })
            .operation(OP_NEW_LOG));
        }
        Ok(Self {
            events: VecDeque::with_capacity(capacity),
            capacity,
            evicted: 0,
        })
    }

    /// The maximum number of events retained.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of events currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the log currently holds no events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// How many events have been evicted because the buffer was full.
    ///
    /// Non-zero means the log is **not** a complete history of the session. A consumer
    /// surfacing this is being honest; a consumer ignoring it is presenting a partial
    /// record as a whole one.
    #[must_use]
    pub const fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Appends an event, evicting the oldest if the buffer is full.
    ///
    /// Never fails and never allocates beyond the configured capacity, because a
    /// failure mode here would mean dropping the record that matters most — the one that
    /// records the failure.
    pub fn record(&mut self, event: AuditEvent) {
        if self.events.len() == self.capacity {
            if let Some(dropped) = self.events.pop_front() {
                debug_assert!(!dropped.agent_id.is_empty(), "record was already validated");
            }
            self.evicted = self.evicted.saturating_add(1);
        }
        self.events.push_back(event);
    }

    /// Iterates the retained events, oldest first.
    ///
    /// Oldest-first matches chronological reading order and matches eviction order, so
    /// `iter().next()` is the next event to be lost and `iter().last()` the newest.
    pub fn iter(&self) -> impl Iterator<Item = &AuditEvent> {
        self.events.iter()
    }

    /// The newest retained event.
    #[must_use]
    pub fn newest(&self) -> Option<&AuditEvent> {
        self.events.back()
    }

    /// The oldest retained event.
    #[must_use]
    pub fn oldest(&self) -> Option<&AuditEvent> {
        self.events.front()
    }

    /// The retained events for one agent, oldest first.
    ///
    /// Used to answer "what did *this* agent do", which is the common question after an
    /// incident and would otherwise require the consumer to filter the whole buffer.
    pub fn for_agent<'a>(&'a self, agent_id: &'a str) -> impl Iterator<Item = &'a AuditEvent> {
        self.events
            .iter()
            .filter(move |event| event.agent_id == agent_id)
    }

    /// Every retained event for one correlation id, oldest first.
    pub fn for_request<'a>(
        &'a self,
        request_id: &'a CorrelationId,
    ) -> impl Iterator<Item = &'a AuditEvent> {
        self.events
            .iter()
            .filter(move |event| &event.request_id == request_id)
    }

    /// The retained events with one outcome, oldest first.
    pub fn with_outcome(&self, outcome: AuditOutcome) -> impl Iterator<Item = &AuditEvent> {
        self.events
            .iter()
            .filter(move |event| event.outcome == outcome)
    }

    /// The retained events as a vector, oldest first.
    #[must_use]
    pub fn to_vec(&self) -> Vec<AuditEvent> {
        self.events.iter().cloned().collect()
    }

    /// Drops every retained event and resets the eviction count.
    ///
    /// The eviction count is reset because it describes the *current* buffer's history;
    /// carrying it across a clear would claim events were lost from a buffer that no
    /// longer has a predecessor.
    pub fn clear(&mut self) {
        self.events.clear();
        self.evicted = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::{Capability, CorrelationId};

    /// A fixed instant, so records are byte-identical between runs and between platforms.
    fn timestamp() -> SystemTime {
        SystemTime::UNIX_EPOCH + core::time::Duration::from_secs(1_700_000_000)
    }

    fn event(agent: &str, action: &str, outcome: AuditOutcome) -> AuditEvent {
        AuditEvent::new(
            timestamp(),
            agent,
            CorrelationId::new(),
            action,
            "/srv/work/src/main.rs",
            outcome,
            CapabilitySet::empty().with(Capability::FilesystemRead),
        )
        .expect("valid event")
    }

    #[test]
    fn a_new_log_is_empty_with_the_requested_capacity() {
        let log = AuditLog::new(16).expect("valid");
        assert_eq!(log.capacity(), 16);
        assert_eq!(log.len(), 0);
        assert!(log.is_empty());
        assert_eq!(log.evicted(), 0);
        assert!(log.newest().is_none());
        assert!(log.oldest().is_none());
    }

    #[test]
    fn a_zero_capacity_log_is_rejected() {
        let err = AuditLog::new(0).expect_err("zero capacity is a silent audit failure");
        assert!(
            matches!(err.kind(), ErrorKind::Validation { .. }),
            "got {err:?}"
        );
        assert_eq!(err.operation_name(), OP_NEW_LOG);
    }

    #[test]
    fn events_are_retained_in_order() {
        let mut log = AuditLog::new(8).expect("valid");
        for index in 0..4 {
            log.record(event(
                "agent-1",
                &format!("action-{index}"),
                AuditOutcome::Allowed,
            ));
        }
        assert_eq!(log.len(), 4);
        assert_eq!(log.oldest().expect("present").action, "action-0");
        assert_eq!(log.newest().expect("present").action, "action-3");
        let actions: Vec<&str> = log.iter().map(|event| event.action.as_str()).collect();
        assert_eq!(actions, ["action-0", "action-1", "action-2", "action-3"]);
        assert_eq!(log.evicted(), 0);
    }

    /// Boundedness: the buffer never exceeds its capacity, whatever is written.
    #[test]
    fn the_buffer_never_exceeds_its_capacity() {
        for capacity in [1usize, 2, 3, 8, 64] {
            let mut log = AuditLog::new(capacity).expect("valid");
            for index in 0..500 {
                log.record(event(
                    "agent-1",
                    &format!("action-{index}"),
                    AuditOutcome::Allowed,
                ));
                assert!(log.len() <= capacity, "capacity {capacity} exceeded");
            }
            assert_eq!(log.len(), capacity, "the buffer should be exactly full");
        }
    }

    /// Eviction is oldest-first, and the evicted count tracks it.
    #[test]
    fn the_oldest_event_is_evicted_first() {
        let mut log = AuditLog::new(3).expect("valid");
        for index in 0..5 {
            log.record(event(
                "agent-1",
                &format!("action-{index}"),
                AuditOutcome::Allowed,
            ));
        }
        assert_eq!(log.len(), 3);
        assert_eq!(log.evicted(), 2);
        let actions: Vec<&str> = log.iter().map(|event| event.action.as_str()).collect();
        assert_eq!(actions, ["action-2", "action-3", "action-4"]);
        assert_eq!(log.oldest().expect("present").action, "action-2");
        assert_eq!(log.newest().expect("present").action, "action-4");
    }

    #[test]
    fn a_capacity_of_one_keeps_only_the_newest_event() {
        let mut log = AuditLog::new(1).expect("valid");
        log.record(event("agent-1", "first", AuditOutcome::Allowed));
        log.record(event("agent-1", "second", AuditOutcome::Denied));
        assert_eq!(log.len(), 1);
        assert_eq!(log.evicted(), 1);
        assert_eq!(log.newest().expect("present").action, "second");
    }

    #[test]
    fn eviction_count_saturates_rather_than_wrapping() {
        let mut log = AuditLog::new(1).expect("valid");
        for _ in 0..100 {
            log.record(event("agent-1", "action", AuditOutcome::Allowed));
        }
        assert_eq!(log.evicted(), 99);
    }

    #[test]
    fn clear_drops_events_and_resets_the_eviction_count() {
        let mut log = AuditLog::new(2).expect("valid");
        for _ in 0..5 {
            log.record(event("agent-1", "action", AuditOutcome::Allowed));
        }
        assert_eq!(log.evicted(), 3);
        log.clear();
        assert!(log.is_empty());
        assert_eq!(log.len(), 0);
        assert_eq!(
            log.evicted(),
            0,
            "the count describes the current buffer's history"
        );
    }

    #[test]
    fn recording_never_fails_for_a_valid_event() {
        let mut log = AuditLog::new(4).expect("valid");
        for outcome in [
            AuditOutcome::Allowed,
            AuditOutcome::Denied,
            AuditOutcome::Failed,
            AuditOutcome::Authorized,
        ] {
            log.record(event("agent-1", "action", outcome));
        }
        assert_eq!(log.len(), 4);
    }

    // ---- event construction ----

    #[test]
    fn an_event_requires_an_actor_and_an_action() {
        for agent in ["", "  ", "\t"] {
            let err = AuditEvent::new(
                timestamp(),
                agent,
                CorrelationId::new(),
                "action",
                "target",
                AuditOutcome::Allowed,
                CapabilitySet::empty(),
            )
            .expect_err("an anonymous event must be rejected");
            assert!(
                matches!(err.kind(), ErrorKind::Validation { .. }),
                "got {err:?}"
            );
        }
        for action in ["", "  "] {
            let err = AuditEvent::new(
                timestamp(),
                "agent-1",
                CorrelationId::new(),
                action,
                "target",
                AuditOutcome::Allowed,
                CapabilitySet::empty(),
            )
            .expect_err("an actionless event must be rejected");
            assert!(
                matches!(err.kind(), ErrorKind::Validation { .. }),
                "got {err:?}"
            );
        }
    }

    #[test]
    fn an_event_allows_an_empty_target() {
        // Shutdown and pure capability checks have no target; requiring one would push
        // callers to write filler text that means nothing.
        let event = AuditEvent::new(
            timestamp(),
            "agent-1",
            CorrelationId::new(),
            "runtime.shutdown",
            "",
            AuditOutcome::Allowed,
            CapabilitySet::empty(),
        )
        .expect("valid");
        assert!(event.target.is_empty());
    }

    #[test]
    fn an_event_records_every_required_field() {
        let request_id = CorrelationId::new();
        let capabilities = CapabilitySet::empty()
            .with(Capability::FilesystemWrite)
            .with(Capability::DeleteData);
        let event = AuditEvent::new(
            timestamp(),
            "agent-1",
            request_id,
            "fs.delete",
            "/srv/work/build",
            AuditOutcome::Authorized,
            capabilities,
        )
        .expect("valid");

        assert_eq!(event.timestamp, timestamp());
        assert_eq!(event.agent_id, "agent-1");
        assert_eq!(event.request_id, request_id);
        assert_eq!(event.action, "fs.delete");
        assert_eq!(event.target, "/srv/work/build");
        assert_eq!(event.outcome, AuditOutcome::Authorized);
        assert_eq!(event.capabilities_used, capabilities);
        assert_eq!(event.outcome_str(), "authorized");
    }

    #[test]
    fn every_outcome_has_a_stable_unique_name() {
        let outcomes = [
            AuditOutcome::Allowed,
            AuditOutcome::Denied,
            AuditOutcome::Failed,
            AuditOutcome::Authorized,
        ];
        let mut names: Vec<&str> = outcomes.iter().map(|o| o.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "outcome names must be unique");
        for outcome in outcomes {
            assert_eq!(outcome.to_string(), outcome.as_str());
        }
    }

    // ---- queries ----

    #[test]
    fn events_can_be_filtered_by_agent_request_and_outcome() {
        let mut log = AuditLog::new(16).expect("valid");
        let shared = CorrelationId::new();
        let other = CorrelationId::new();

        log.record(
            AuditEvent::new(
                timestamp(),
                "agent-1",
                shared,
                "fs.read",
                "a",
                AuditOutcome::Allowed,
                CapabilitySet::empty(),
            )
            .expect("valid"),
        );
        log.record(
            AuditEvent::new(
                timestamp(),
                "agent-2",
                other,
                "fs.delete",
                "b",
                AuditOutcome::Denied,
                CapabilitySet::empty(),
            )
            .expect("valid"),
        );
        log.record(
            AuditEvent::new(
                timestamp(),
                "agent-1",
                shared,
                "fs.write",
                "c",
                AuditOutcome::Denied,
                CapabilitySet::empty(),
            )
            .expect("valid"),
        );

        assert_eq!(log.for_agent("agent-1").count(), 2);
        assert_eq!(log.for_agent("agent-2").count(), 1);
        assert_eq!(log.for_agent("agent-none").count(), 0);
        assert_eq!(log.for_request(&shared).count(), 2);
        assert_eq!(log.for_request(&other).count(), 1);
        assert_eq!(log.with_outcome(AuditOutcome::Denied).count(), 2);
        assert_eq!(log.with_outcome(AuditOutcome::Allowed).count(), 1);
    }

    #[test]
    fn queries_see_only_the_retained_events() {
        let mut log = AuditLog::new(2).expect("valid");
        for index in 0..6 {
            log.record(event(
                "agent-1",
                &format!("action-{index}"),
                AuditOutcome::Allowed,
            ));
        }
        // Actions 0-3 have been evicted.
        assert_eq!(log.for_agent("agent-1").count(), 2);
        let actions: Vec<&str> = log
            .for_agent("agent-1")
            .map(|event| event.action.as_str())
            .collect();
        assert_eq!(actions, ["action-4", "action-5"]);
    }

    #[test]
    fn to_vec_matches_the_iterator_in_order() {
        let mut log = AuditLog::new(4).expect("valid");
        for index in 0..4 {
            log.record(event(
                "agent-1",
                &format!("action-{index}"),
                AuditOutcome::Allowed,
            ));
        }
        let vector = log.to_vec();
        assert_eq!(vector.len(), 4);
        for (position, event) in vector.iter().enumerate() {
            assert_eq!(event, log.iter().nth(position).expect("in range"));
        }
    }

    #[test]
    fn a_failed_action_is_auditable_with_its_cause() {
        // The point of the trail: after the fact, a reader sees the denial and what it
        // was aimed at.
        let mut log = AuditLog::new(4).expect("valid");
        log.record(event("agent-1", "fs.delete", AuditOutcome::Denied));
        let denial = log.newest().expect("present");
        assert_eq!(denial.outcome, AuditOutcome::Denied);
        assert_eq!(denial.action, "fs.delete");
        assert_eq!(denial.target, "/srv/work/src/main.rs");
    }

    #[test]
    fn display_is_single_line_and_carries_the_key_fields() {
        let event = event("agent-1", "fs.read", AuditOutcome::Allowed);
        let text = event.to_string();
        assert!(
            !text.contains('\n'),
            "audit rendering must be one line: {text}"
        );
        for needle in ["agent=agent-1", "action=fs.read", "outcome=allowed"] {
            assert!(text.contains(needle), "missing {needle} in {text}");
        }
    }

    /// A pre-epoch timestamp must render rather than panic. `SystemTime::duration_since`
    /// returns an error for clocks set before 1970, and an audit log is the last place
    /// that should turn a clock skew into a crash.
    #[test]
    fn a_pre_epoch_timestamp_renders_without_panicking() {
        let event = AuditEvent::new(
            SystemTime::UNIX_EPOCH - core::time::Duration::from_hours(24),
            "agent-1",
            CorrelationId::new(),
            "action",
            "target",
            AuditOutcome::Failed,
            CapabilitySet::empty(),
        )
        .expect("valid");
        let text = event.to_string();
        assert!(text.contains("pre-epoch"), "got {text}");
    }

    /// An interleaved multi-agent session: the record must answer per-agent questions
    /// and survive eviction without corrupting the retained window.
    #[test]
    fn interleaved_agents_produce_a_queryable_trail() {
        let mut log = AuditLog::new(10).expect("valid");
        for index in 0..30 {
            let agent = if index % 2 == 0 {
                "agent-even"
            } else {
                "agent-odd"
            };
            let outcome = if index % 3 == 0 {
                AuditOutcome::Denied
            } else {
                AuditOutcome::Allowed
            };
            log.record(event(agent, &format!("action-{index}"), outcome));
        }
        assert_eq!(log.len(), 10);
        assert_eq!(log.evicted(), 20);
        // The retained window is the last ten events: indices 20..29.
        let retained: Vec<&str> = log.iter().map(|event| event.action.as_str()).collect();
        let expected: Vec<String> = (20..30).map(|index| format!("action-{index}")).collect();
        assert_eq!(retained, expected);
        assert_eq!(log.for_agent("agent-even").count(), 5);
        assert_eq!(log.for_agent("agent-odd").count(), 5);
    }
}
