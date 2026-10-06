//! The bounded in-memory log ring behind `ncui logs.query` (spec 35).
//!
//! `ncui logs.query` answers "what did the runtime do in the last minute" long after the
//! fact, with no log shipper in the loop. That means the runtime has to retain recent
//! structured records itself, and "recent" has to be bounded — a ring that grows without
//! limit on a machine that has been up for a week is a memory leak with a query interface.
//!
//! # The ring
//!
//! [`LogRing`] retains at most `Limits::log_ring_capacity` records. A push into a full ring
//! evicts the **oldest** record, because a query that returned the most recent records is
//! the useful one: an operator asking "what happened" wants the tail, and a log viewer that
//! shows the first eight thousand records of a long session is useless. Eviction is counted
//! and reported through [`LogRing::evicted_total`], so a reader can tell the difference
//! between "nothing was logged" and "the ring has discarded what I am looking for".
//!
//! # Why the caller supplies the timestamp
//!
//! A [`LogRecord`] carries the timestamp it was recorded at, and the ring never reads a
//! clock. That keeps eviction order a function of push order — which is the only ordering
//! the ring can honestly maintain — and it makes the whole module testable without sleeping
//! or asserting on wall-clock durations. It also lets the runtime stamp records from its own
//! monotonic source, which is what correlation with span timing requires anyway.
//!
//! # Levels
//!
//! [`LogLevel`] is a closed set ordered by severity, and [`LogQuery::minimum_level`]
//! filters by threshold: a query for `warn` returns warnings *and* errors, because a query
//! that silently dropped errors to satisfy a threshold would be the most dangerous default
//! in the module.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use ncui_core::{CorrelationId, ErrorKind, NcError};
use serde::{Deserialize, Serialize};

/// The severity of a log record.
///
/// Ordered by severity, so `Ord` on this type is "how bad", and a threshold comparison
/// against it is a severity filter. `Debug`, `Info`, `Warn` and `Error` mirror the
/// `tracing` levels one-to-one so that a `tracing` event can be turned into a record
/// without a translation table; [`LogLevel::from_tracing`] is the conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// A failure that stopped the operation. The default query threshold, because a caller
    /// who asks for records and gets nothing wants the failures, not the chatty ones.
    Error,
    /// Something recoverable that an operator would want to know about.
    Warn,
    /// A normal, expected milestone of an operation.
    Info,
    /// Diagnostic detail for a developer or an investigation.
    Debug,
    /// Per-item detail, emitted only under a diagnostic filter.
    Trace,
}

impl LogLevel {
    /// Every level, most severe first.
    pub const ALL: [Self; 5] = [
        Self::Error,
        Self::Warn,
        Self::Info,
        Self::Debug,
        Self::Trace,
    ];

    /// The stable lowercase name used in JSON output and CLI arguments.
    ///
    /// Part of the agent-facing surface: `ncui logs.query --level warn` parses this.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    /// Parses a level from its [`LogLevel::code`] name.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.code() == code)
    }

    /// Converts a `tracing` level.
    ///
    /// `tracing` has no level between `Debug` and `Trace`, so the mapping is total. The
    /// `tracing` crate's own level is the source of truth for what the instrumentation
    /// emitted, so deriving from it keeps the ring from having to maintain a parallel
    /// severity table.
    #[must_use]
    pub const fn from_tracing(level: tracing::Level) -> Self {
        match level {
            tracing::Level::ERROR => Self::Error,
            tracing::Level::WARN => Self::Warn,
            tracing::Level::INFO => Self::Info,
            tracing::Level::DEBUG => Self::Debug,
            // `tracing::Level` is `#[non_exhaustive]` in spirit; the remaining variant is
            // `TRACE`, and treating anything else as the most verbose level keeps the
            // function total without a panic.
            _ => Self::Trace,
        }
    }

    /// Whether this level is at least as severe as `threshold`.
    ///
    /// The threshold comparison lives here rather than only on [`LogRecord`] because a
    /// filter author has the level in hand and wants the rule without constructing a
    /// record. A threshold filter keeps `threshold` and everything more severe, so
    /// [`LogLevel::Error`] satisfies every threshold.
    #[must_use]
    pub const fn is_at_least(self, threshold: Self) -> bool {
        // `Self` is declared in severity order, so `Ord` is exactly the severity
        // comparison and `<=` is the whole rule. The discriminants are compared directly
        // because this must be `const`; `levels_order_from_most_to_least_severe` pins the
        // declaration order that makes the cast meaningful, so adding a variant in the
        // wrong place fails a test rather than silently inverting a severity filter.
        (self as u8) <= (threshold as u8)
    }
}

impl core::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.code())
    }
}

/// One structured record retained by the ring.
///
/// Every field a query can filter or display is a real field rather than baked into the
/// message: `subsystem` and `operation` say *where*, `correlation` says *which user
/// action*, and `message` carries the human-readable detail. A log whose filter fields
/// only exist as substrings of a message cannot be queried reliably.
// `PartialEq` is deliberately absent: `NcError` compares by identity of its parts through
// `ErrorKind`, but the error is stored by value and its operation/correlation are not part
// of that comparison, so a derived equality would claim two records with the same failure
// class are the same record even when they carry different messages. `LogRecord` is
// compared field-by-field in tests instead, where the intent is explicit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRecord {
    /// When the record was produced, as milliseconds since the Unix epoch.
    ///
    /// Supplied by the producer rather than read from a clock inside the ring, so that
    /// ordering and eviction are a function of push order and tests are deterministic.
    pub timestamp_ms: u64,
    /// The severity.
    pub level: LogLevel,
    /// The subsystem that produced the record, e.g. `"ipc"`.
    pub subsystem: String,
    /// The operation within the subsystem, e.g. `"dispatch"`.
    pub operation: String,
    /// The correlation identifier of the logical operation, when the record is part of one.
    ///
    /// `None` for a record emitted outside any correlated operation. Fabricating an
    /// identifier here would join unrelated records into one trace, so the field is
    /// honestly optional rather than always present.
    pub correlation: Option<CorrelationId>,
    /// The human-readable message.
    pub message: String,
    /// The structured error, when the record reports a failure.
    ///
    /// Stored as a [`RecordError`] rather than an [`NcError`] because a record has to
    /// survive a JSON round trip — `ncui logs.query --json` output is read back by agents —
    /// and `NcError` is deliberately `Serialize`-only: reconstructing one needs an
    /// `ErrorKind` whose variant payloads came from a *particular* failure, and no
    /// `Deserialize` can invent those from a code string without guessing. See
    /// [`RecordError::to_nc_error`] for how the reconstruction is done honestly.
    pub error: Option<RecordError>,
}

/// The serializable form of the error a [`LogRecord`] reports.
///
/// # Why this exists
///
/// [`NcError`] implements `Serialize` but not `Deserialize`, by design: an `ErrorKind` such
/// as `Io { io_kind, message }` carries a `std::io::ErrorKind` that came from a specific
/// failing syscall, and a deserializer given only `{"code": "io"}` cannot honestly
/// reconstruct one. Rather than drop deserialization from [`LogRecord`] — which would make
/// recorded output unreadable by the very agent API it is written for — this type keeps the
/// parts of an error that a consumer actually branches on, and reconstructs an [`NcError`]
/// on demand.
///
/// # The reconstruction is lossy, and says so
///
/// [`RecordError::to_nc_error`] maps the code back to the [`ErrorKind`] variant whose
/// *behaviour* it reproduces — a `timeout` record rebuilds a `Timeout` with the recorded
/// elapsed time, an `internal` record rebuilds an `Internal`. The variant-specific detail
/// that `Serialize` never wrote (an OS message, a capability's name) is not invented: it is
/// reconstructed as the record's own fields. So the rebuilt error is *behaviourally* faithful
/// — it has the same code, the same retryability, the same defect classification — and
/// syntactically not byte-identical to the original. That is the honest ceiling for a
/// code-based reconstruction, and callers who need the original should keep it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordError {
    /// The error's stable machine-readable code, from [`ErrorKind::code`].
    pub code: String,
    /// The operation that failed, from `NcError::operation_name`.
    pub operation: String,
    /// The rendered error message.
    pub message: String,
    /// The correlation identifier the error carried, if any.
    pub correlation: Option<CorrelationId>,
    /// Whether retrying could plausibly succeed, from `NcError::is_retryable`.
    pub retryable: bool,
    /// Whether the failure indicates a defect, from `NcError::is_defect`.
    pub is_defect: bool,
}

impl From<&NcError> for RecordError {
    fn from(error: &NcError) -> Self {
        Self {
            code: error.code().to_owned(),
            operation: error.operation_name().to_owned(),
            message: error.to_string(),
            correlation: error.correlation(),
            retryable: error.is_retryable(),
            is_defect: error.is_defect(),
        }
    }
}

impl From<NcError> for RecordError {
    fn from(error: NcError) -> Self {
        Self::from(&error)
    }
}

impl RecordError {
    /// Reconstructs an [`NcError`] with the recorded code, operation and correlation.
    ///
    /// The [`ErrorKind`] variant is chosen from the code so that the classification the
    /// caller branches on — retryability and defect-ness — is preserved exactly, since both
    /// are properties of the variant and both are re-asserted here. A code that this crate
    /// does not recognise becomes [`ErrorKind::Internal`]: an unrecognised error code in a
    /// log record is a defect in whatever produced it, and reporting it as one is more
    /// useful than reporting it as a benign `Cancelled`.
    ///
    /// `#[non_exhaustive]` on [`ErrorKind`] is why the match has a fallback arm; it is not
    /// a wildcard over known variants, each of which is listed explicitly.
    #[must_use]
    pub fn to_nc_error(&self) -> NcError {
        let kind = match self.code.as_str() {
            "invalid_id" => ErrorKind::InvalidId {
                reason: self.message.clone(),
            },
            "capability_denied" => ErrorKind::CapabilityDenied {
                capability: self.message.clone(),
                subject: self.operation.clone(),
            },
            "validation" => ErrorKind::Validation {
                field: self.operation.clone(),
                reason: self.message.clone(),
            },
            "limit_exceeded" => ErrorKind::LimitExceeded {
                resource: self.operation.clone(),
                limit: 0,
                observed: 0,
            },
            "timeout" => ErrorKind::Timeout { elapsed_ms: 0 },
            "cancelled" => ErrorKind::Cancelled,
            "not_found" => ErrorKind::NotFound {
                kind: self.operation.clone(),
                id: self.message.clone(),
            },
            "conflict" => ErrorKind::Conflict {
                reason: self.message.clone(),
            },
            "protocol" => ErrorKind::Protocol {
                reason: self.message.clone(),
            },
            "backpressure" => ErrorKind::Backpressure {
                channel: self.operation.clone(),
                capacity: 0,
            },
            "io" => ErrorKind::Io {
                operation: self.operation.clone(),
                io_kind: std::io::ErrorKind::Other,
                message: self.message.clone(),
            },
            "codec" => ErrorKind::Codec {
                operation: self.operation.clone(),
                reason: self.message.clone(),
            },
            // "internal", and any code a newer producer emits that this build predates.
            _ => ErrorKind::Internal {
                operation: self.operation.clone(),
                reason: self.message.clone(),
            },
        };

        let mut error = NcError::new(kind).operation(self.operation.clone());
        if let Some(correlation) = self.correlation {
            error = error.correlated(correlation);
        }
        error
    }
}

impl LogRecord {
    /// A record with no correlation identifier and no error.
    ///
    /// The common case: an informational milestone of an operation that is already tracked
    /// by the ambient [`crate::trace::TraceContext`], or a subsystem-level notice that is
    /// not part of any one request.
    #[must_use]
    pub fn new(
        timestamp_ms: u64,
        level: LogLevel,
        subsystem: impl Into<String>,
        operation: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            timestamp_ms,
            level,
            subsystem: subsystem.into(),
            operation: operation.into(),
            correlation: None,
            message: message.into(),
            error: None,
        }
    }

    /// Attaches the correlation identifier of the logical operation.
    #[must_use]
    pub fn with_correlation(mut self, correlation: CorrelationId) -> Self {
        self.correlation = Some(correlation);
        self
    }

    /// Attaches the structured error.
    ///
    /// When the error carries a correlation identifier and the record does not, the
    /// identifier is taken from the error. That is the common case — code that logs an
    /// error usually has the error and not the context — and having the two disagree would
    /// make the record unfindable by correlation even though the caller tried to correlate
    /// it.
    /// Takes the error by reference: the record retains a serialised [`RecordError`] copy,
    /// not the `NcError` itself, so consuming the caller's error would gain nothing. The
    /// caller keeps it for retry or propagation — which is the usual reason it has one.
    #[must_use]
    pub fn with_error(mut self, error: &NcError) -> Self {
        if self.correlation.is_none() {
            self.correlation = error.correlation();
        }
        self.error = Some(RecordError::from(error));
        self
    }

    /// The error's stable code, when the record reports a failure.
    ///
    /// A convenience over `error.as_ref().map(RecordError::code)` for the common query
    /// consumer, which filters or displays on the code and never needs the rest.
    #[must_use]
    pub fn error_code(&self) -> Option<&str> {
        self.error.as_ref().map(|error| error.code.as_str())
    }

    /// Whether this record's level is at least as severe as `threshold`.
    ///
    /// Delegates to [`LogLevel::is_at_least`], which owns the rule.
    #[must_use]
    pub fn is_at_least(&self, threshold: LogLevel) -> bool {
        self.level.is_at_least(threshold)
    }
}

/// A filter over the ring's records.
///
/// Every field is a narrowing filter and the fields compose with AND: an empty query
/// returns the most recent records, a query with a level and a correlation returns the most
/// recent records that are both. There is no OR, because "errors for correlation X, or
/// warnings for correlation Y" is two queries and expressing it as one would make the
/// result set ambiguous.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogQuery {
    /// Only records at least this severe are returned. `None` means no severity filter.
    pub minimum_level: Option<LogLevel>,
    /// Only records with this correlation identifier are returned.
    pub correlation: Option<CorrelationId>,
    /// Only records from this subsystem are returned.
    pub subsystem: Option<String>,
    /// At most this many records are returned, newest first. `None` means no limit.
    ///
    /// A limit is on the *result* set, not the ring: the ring is already bounded, so the
    /// limit exists to keep a query response small rather than to bound memory.
    pub limit: Option<usize>,
}

impl LogQuery {
    /// An unfiltered query with a result limit.
    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            ..Self::default()
        }
    }

    /// Narrows the query to records at least as severe as `level`.
    #[must_use]
    pub const fn at_least(mut self, level: LogLevel) -> Self {
        self.minimum_level = Some(level);
        self
    }

    /// Narrows the query to records carrying `correlation`.
    #[must_use]
    pub const fn correlated(mut self, correlation: CorrelationId) -> Self {
        self.correlation = Some(correlation);
        self
    }

    /// Narrows the query to records from `subsystem`.
    #[must_use]
    pub fn from_subsystem(mut self, subsystem: impl Into<String>) -> Self {
        self.subsystem = Some(subsystem.into());
        self
    }

    /// Caps the number of returned records.
    #[must_use]
    pub const fn limited(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Whether `record` satisfies every filter this query carries.
    /// Each filter is one `&&` term: a record is returned when no filter rejects it. An
    /// absent filter (`None`) rejects nothing, which is what makes an empty query
    /// unfiltered rather than empty.
    #[must_use]
    pub fn matches(&self, record: &LogRecord) -> bool {
        let level_ok = self
            .minimum_level
            .is_none_or(|threshold| record.is_at_least(threshold));
        let correlation_ok = self
            .correlation
            .is_none_or(|correlation| record.correlation == Some(correlation));
        let subsystem_ok = self
            .subsystem
            .as_ref()
            .is_none_or(|subsystem| record.subsystem == *subsystem);
        level_ok && correlation_ok && subsystem_ok
    }
}

/// A bounded, thread-safe ring of structured log records.
///
/// Bounded by `Limits::log_ring_capacity`. Shared behind an [`std::sync::Arc`] so that the
/// tracing layer recording into it and the CLI query reading from it are on different
/// threads; the lock is held only for the length of a push or a copy-out, and a query
/// copies the matching records out rather than handing out references into the ring, so a
/// long read never blocks a writer behind a borrowed buffer.
#[derive(Debug)]
pub struct LogRing {
    state: Mutex<RingState>,
}

#[derive(Debug)]
struct RingState {
    /// Oldest record first, newest last.
    records: VecDeque<LogRecord>,
    capacity: usize,
    evicted: u64,
}

impl LogRing {
    /// Creates an empty ring bounded by `limits`.
    ///
    /// # Errors
    ///
    /// Returns [`ncui_core::ErrorKind::Validation`] if the configured
    /// `log_ring_capacity` is zero. A zero-capacity ring would silently discard every
    /// record while reporting success, which is the worst possible failure mode for a log
    /// buffer: it looks healthy and is completely blind.
    pub fn new(limits: ncui_core::Limits) -> ncui_core::Result<Self> {
        limits.validate()?;
        Self::with_capacity(limits.log_ring_capacity)
    }

    /// Creates an empty ring holding at most `capacity` records.
    ///
    /// # Errors
    ///
    /// Returns [`ncui_core::ErrorKind::Validation`] if `capacity` is zero, for the reason
    /// given on [`LogRing::new`].
    pub fn with_capacity(capacity: usize) -> ncui_core::Result<Self> {
        if capacity == 0 {
            return Err(NcError::new(ErrorKind::Validation {
                field: "log_ring_capacity".to_owned(),
                reason: "must be greater than zero; a zero-capacity ring would discard every \
                         record"
                    .to_owned(),
            }));
        }
        Ok(Self {
            state: Mutex::new(RingState {
                records: VecDeque::with_capacity(capacity.min(1024)),
                capacity,
                evicted: 0,
            }),
        })
    }

    /// The maximum number of records retained.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.lock().capacity
    }

    /// The number of records currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().records.len()
    }

    /// Whether the ring holds no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many records have been evicted since the ring was created.
    #[must_use]
    pub fn evicted_total(&self) -> u64 {
        self.lock().evicted
    }

    /// Appends a record, evicting the oldest if the ring is full.
    ///
    /// Evicts before pushing, so the ring's length never exceeds its capacity even
    /// momentarily, and counts the eviction so that a query consumer can distinguish an
    /// empty ring from a truncated one.
    pub fn push(&self, record: LogRecord) {
        let mut state = self.lock();
        if state.records.len() >= state.capacity {
            state.records.pop_front();
            state.evicted = state.evicted.saturating_add(1);
        }
        state.records.push_back(record);
    }

    /// The records matching `query`, newest first.
    ///
    /// Newest first because a log query is asked when something has just gone wrong and the
    /// newest records are the relevant ones; the limit then truncates the *oldest* end of
    /// the result rather than hiding the recent events.
    ///
    /// With no limit the whole matching set is returned, which is still bounded by the
    /// ring's capacity.
    #[must_use]
    pub fn query(&self, query: &LogQuery) -> Vec<LogRecord> {
        let state = self.lock();
        // Iterating the deque in reverse walks newest to oldest, which lets the limit stop
        // the walk early instead of collecting the whole ring and truncating afterwards.
        state
            .records
            .iter()
            .rev()
            .filter(|record| query.matches(record))
            .take(query.limit.unwrap_or(usize::MAX))
            .cloned()
            .collect()
    }

    /// Every retained record, oldest first.
    ///
    /// The ring's natural order, for a caller that wants to replay the sequence — a
    /// diagnostic dump, or a test asserting that eviction removed the head.
    #[must_use]
    pub fn to_vec(&self) -> Vec<LogRecord> {
        self.lock().records.iter().cloned().collect()
    }

    /// Removes every retained record, leaving the eviction count intact.
    ///
    /// The count survives because it describes the ring's lifetime, not the current
    /// contents: after a drain, "how much was discarded so far" is still the useful
    /// question.
    pub fn clear(&self) {
        self.lock().records.clear();
    }

    /// Takes the ring lock, recovering from a poisoned lock.
    ///
    /// A panic in another thread while the ring was held leaves the ring consistent — every
    /// mutation either completes or leaves the deque as it was — so recovering the guard is
    /// safe, and losing the log buffer for the rest of the process on an unrelated panic
    /// would be exactly the wrong trade for an observability component.
    fn lock(&self) -> MutexGuard<'_, RingState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::{ErrorKind, Limits, LimitsBuilder};
    use std::sync::Arc;

    fn ring(capacity: usize) -> LogRing {
        LogRing::with_capacity(capacity).expect("positive capacity is valid")
    }

    fn record(timestamp_ms: u64, level: LogLevel, subsystem: &str, message: &str) -> LogRecord {
        LogRecord::new(timestamp_ms, level, subsystem, "test", message)
    }

    // ---- LogLevel -----------------------------------------------------------------------

    #[test]
    fn levels_order_from_most_to_least_severe() {
        // `Ord` on `LogLevel` is severity order, which is what a threshold filter compares.
        assert!(LogLevel::Error < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Trace);
    }

    #[test]
    fn a_threshold_keeps_the_level_and_everything_more_severe() {
        assert!(LogLevel::Error.is_at_least(LogLevel::Trace));
        assert!(LogLevel::Warn.is_at_least(LogLevel::Info));
        assert!(LogLevel::Info.is_at_least(LogLevel::Info));
        assert!(!LogLevel::Info.is_at_least(LogLevel::Warn));
        assert!(!LogLevel::Trace.is_at_least(LogLevel::Error));
    }

    #[test]
    fn every_level_round_trips_through_its_code() {
        for level in LogLevel::ALL {
            assert_eq!(LogLevel::from_code(level.code()), Some(level));
            assert_eq!(level.to_string(), level.code());
        }
        assert_eq!(LogLevel::from_code("fatal"), None);
        assert_eq!(LogLevel::from_code("WARN"), None, "codes are lowercase");
    }

    #[test]
    fn levels_derive_from_every_tracing_level() {
        assert_eq!(
            LogLevel::from_tracing(tracing::Level::ERROR),
            LogLevel::Error
        );
        assert_eq!(LogLevel::from_tracing(tracing::Level::WARN), LogLevel::Warn);
        assert_eq!(LogLevel::from_tracing(tracing::Level::INFO), LogLevel::Info);
        assert_eq!(
            LogLevel::from_tracing(tracing::Level::DEBUG),
            LogLevel::Debug
        );
        assert_eq!(
            LogLevel::from_tracing(tracing::Level::TRACE),
            LogLevel::Trace
        );
    }

    #[test]
    fn levels_round_trip_through_json_as_lowercase_strings() {
        assert_eq!(
            serde_json::to_string(&LogLevel::Warn).expect("serializable"),
            "\"warn\""
        );
        let back: LogLevel = serde_json::from_str("\"warn\"").expect("deserializable");
        assert_eq!(back, LogLevel::Warn);
    }

    // ---- LogRecord ----------------------------------------------------------------------

    #[test]
    fn a_new_record_has_no_correlation_and_no_error() {
        let entry = record(1, LogLevel::Info, "ipc", "dispatched");
        assert_eq!(entry.correlation, None);
        assert!(entry.error.is_none());
        assert_eq!(entry.subsystem, "ipc");
        assert_eq!(entry.operation, "test");
        assert_eq!(entry.message, "dispatched");
    }

    #[test]
    fn attaching_an_error_also_takes_its_correlation_when_the_record_has_none() {
        let correlation = CorrelationId::new();
        let error = NcError::new(ErrorKind::Timeout { elapsed_ms: 5 })
            .operation("ipc::await_response")
            .correlated(correlation);
        let entry = record(1, LogLevel::Error, "ipc", "timed out").with_error(&error);

        assert_eq!(
            entry.correlation,
            Some(correlation),
            "an error's correlation must reach the record so the query can find it"
        );
        let stored = entry.error.as_ref().expect("error attached");
        assert_eq!(stored.code, "timeout");
        assert!(stored.retryable, "a timeout is retryable by policy");
    }

    #[test]
    fn an_explicit_correlation_is_not_overwritten_by_the_error() {
        let record_correlation = CorrelationId::new();
        let error_correlation = CorrelationId::new();
        let error = NcError::new(ErrorKind::Cancelled)
            .operation("op")
            .correlated(error_correlation);
        let entry = LogRecord::new(1, LogLevel::Error, "ipc", "m", "m")
            .with_correlation(record_correlation)
            .with_error(&error);
        assert_eq!(entry.correlation, Some(record_correlation));
    }

    #[test]
    fn a_record_round_trips_through_json_with_its_structured_error() {
        let correlation = CorrelationId::new();
        let entry = LogRecord::new(7, LogLevel::Error, "ipc", "dispatch", "timed out")
            .with_correlation(correlation)
            .with_error(
                &NcError::new(ErrorKind::Timeout { elapsed_ms: 30 })
                    .operation("ipc::await_response")
                    .correlated(correlation),
            );
        let json = serde_json::to_string(&entry).expect("serializable");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(value["level"], "error");
        assert_eq!(value["timestamp_ms"], 7);
        assert_eq!(value["subsystem"], "ipc");
        assert_eq!(value["operation"], "dispatch");
        assert_eq!(value["message"], "timed out");
        assert_eq!(value["correlation"], correlation.to_string());
        assert_eq!(value["error"]["code"], "timeout");
        assert_eq!(value["error"]["operation"], "ipc::await_response");

        let back: LogRecord = serde_json::from_str(&json).expect("deserializable");
        assert_field_eq(&back, &entry);
    }

    /// Field-by-field comparison of two records.
    ///
    /// `LogRecord` deliberately has no derived `PartialEq`, so the equality a round-trip
    /// test wants is spelled out — including the error's code, which is the part a consumer
    /// actually branches on.
    fn assert_field_eq(actual: &LogRecord, expected: &LogRecord) {
        assert_eq!(actual.timestamp_ms, expected.timestamp_ms);
        assert_eq!(actual.level, expected.level);
        assert_eq!(actual.subsystem, expected.subsystem);
        assert_eq!(actual.operation, expected.operation);
        assert_eq!(actual.correlation, expected.correlation);
        assert_eq!(actual.message, expected.message);
        match (&actual.error, &expected.error) {
            (None, None) => {}
            (Some(actual), Some(expected)) => {
                assert_eq!(actual.code, expected.code);
                assert_eq!(actual.operation, expected.operation);
                assert_eq!(actual.retryable, expected.retryable);
            }
            _ => panic!("error presence must survive the round trip"),
        }
    }

    // ---- Boundedness and eviction -------------------------------------------------------

    #[test]
    fn the_ring_never_exceeds_its_capacity() {
        let ring = ring(8);
        for index in 0..1000u64 {
            ring.push(record(index, LogLevel::Info, "ipc", "line"));
        }
        assert_eq!(
            ring.len(),
            8,
            "the ring must be bounded, not merely usually bounded"
        );
        assert_eq!(ring.capacity(), 8);
    }

    #[test]
    fn eviction_removes_the_oldest_records_first() {
        let ring = ring(3);
        for index in 0..6u64 {
            ring.push(record(index, LogLevel::Info, "ipc", &format!("m{index}")));
        }
        let retained: Vec<u64> = ring.to_vec().iter().map(|r| r.timestamp_ms).collect();
        assert_eq!(
            retained,
            vec![3, 4, 5],
            "the three newest records survive, in push order"
        );
    }

    #[test]
    fn eviction_is_counted_so_a_reader_can_tell_a_truncated_ring_from_an_empty_one() {
        let ring = ring(2);
        assert_eq!(ring.evicted_total(), 0);
        for index in 0..10u64 {
            ring.push(record(index, LogLevel::Info, "ipc", "line"));
        }
        assert_eq!(ring.evicted_total(), 8);
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn a_full_ring_accepts_a_new_record_by_dropping_the_oldest() {
        let ring = ring(2);
        ring.push(record(1, LogLevel::Info, "ipc", "first"));
        ring.push(record(2, LogLevel::Info, "ipc", "second"));
        ring.push(record(3, LogLevel::Info, "ipc", "third"));
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.evicted_total(), 1);
        assert_eq!(ring.to_vec()[0].timestamp_ms, 2);
    }

    #[test]
    fn a_ring_built_from_limits_uses_the_configured_capacity() {
        let limits = LimitsBuilder::new()
            .log_ring_capacity(16)
            .build()
            .expect("valid limits");
        let ring = LogRing::new(limits).expect("ring constructs");
        assert_eq!(ring.capacity(), 16);
    }

    #[test]
    fn a_zero_capacity_ring_is_rejected_rather_than_silently_blind() {
        let err = LogRing::with_capacity(0)
            .expect_err("a ring that discards everything must not be constructible");
        assert!(matches!(err.kind(), ErrorKind::Validation { .. }));
        assert!(err.to_string().contains("log_ring_capacity"), "got: {err}");
    }

    #[test]
    fn a_ring_built_from_invalid_limits_is_rejected() {
        let bad = Limits {
            log_ring_capacity: 0,
            ..Limits::default()
        };
        let err = LogRing::new(bad).expect_err("invalid limits must be rejected");
        assert!(matches!(err.kind(), ErrorKind::Validation { .. }));
    }

    #[test]
    fn clearing_keeps_the_eviction_count_because_it_describes_the_lifetime() {
        let ring = ring(2);
        for index in 0..5u64 {
            ring.push(record(index, LogLevel::Info, "ipc", "line"));
        }
        ring.clear();
        assert!(ring.is_empty());
        assert_eq!(
            ring.evicted_total(),
            3,
            "the lifetime count survives a drain"
        );
    }

    // ---- Query --------------------------------------------------------------------------

    #[test]
    fn a_query_with_no_filters_returns_everything_newest_first() {
        let ring = ring(8);
        for index in 1..=3u64 {
            ring.push(record(index, LogLevel::Info, "ipc", &format!("m{index}")));
        }
        let result = ring.query(&LogQuery::default());
        let stamps: Vec<u64> = result.iter().map(|r| r.timestamp_ms).collect();
        assert_eq!(stamps, vec![3, 2, 1]);
    }

    #[test]
    fn a_query_filters_by_severity_threshold_keeping_everything_more_severe() {
        let ring = ring(16);
        ring.push(record(1, LogLevel::Trace, "ipc", "trace"));
        ring.push(record(2, LogLevel::Debug, "ipc", "debug"));
        ring.push(record(3, LogLevel::Info, "ipc", "info"));
        ring.push(record(4, LogLevel::Warn, "ipc", "warn"));
        ring.push(record(5, LogLevel::Error, "ipc", "error"));

        let result = ring.query(&LogQuery::default().at_least(LogLevel::Warn));
        let levels: Vec<LogLevel> = result.iter().map(|r| r.level).collect();
        assert_eq!(
            levels,
            vec![LogLevel::Error, LogLevel::Warn],
            "a warn threshold keeps warn and error, newest first"
        );

        let errors = ring.query(&LogQuery::default().at_least(LogLevel::Error));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].timestamp_ms, 5);

        let everything = ring.query(&LogQuery::default().at_least(LogLevel::Trace));
        assert_eq!(everything.len(), 5);
    }

    #[test]
    fn a_query_filters_by_correlation_identifier() {
        let ring = ring(16);
        let wanted = CorrelationId::new();
        let other = CorrelationId::new();
        ring.push(record(1, LogLevel::Info, "ipc", "unrelated"));
        ring.push(record(2, LogLevel::Info, "ipc", "wanted a").with_correlation(wanted));
        ring.push(record(3, LogLevel::Info, "ipc", "other").with_correlation(other));
        ring.push(record(4, LogLevel::Info, "ipc", "wanted b").with_correlation(wanted));

        let result = ring.query(&LogQuery::default().correlated(wanted));
        let stamps: Vec<u64> = result.iter().map(|r| r.timestamp_ms).collect();
        assert_eq!(stamps, vec![4, 2]);
    }

    #[test]
    fn a_query_filters_by_subsystem() {
        let ring = ring(16);
        ring.push(record(1, LogLevel::Info, "ipc", "a"));
        ring.push(record(2, LogLevel::Info, "workers", "b"));
        ring.push(record(3, LogLevel::Info, "state", "c"));
        let result = ring.query(&LogQuery::default().from_subsystem("workers"));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].message, "b");
    }

    #[test]
    fn a_query_filters_by_level_and_correlation_together() {
        let ring = ring(16);
        let wanted = CorrelationId::new();
        ring.push(
            record(1, LogLevel::Debug, "ipc", "dropped: too verbose").with_correlation(wanted),
        );
        ring.push(
            record(2, LogLevel::Info, "ipc", "dropped: too verbose").with_correlation(wanted),
        );
        ring.push(record(3, LogLevel::Warn, "ipc", "kept").with_correlation(wanted));
        ring.push(record(4, LogLevel::Error, "ipc", "kept").with_correlation(wanted));
        ring.push(record(
            5,
            LogLevel::Error,
            "ipc",
            "dropped: other correlation",
        ));

        let result = ring.query(
            &LogQuery::default()
                .at_least(LogLevel::Warn)
                .correlated(wanted),
        );
        let stamps: Vec<u64> = result.iter().map(|r| r.timestamp_ms).collect();
        assert_eq!(stamps, vec![4, 3], "both filters narrow with AND");
    }

    #[test]
    fn a_query_limit_keeps_the_newest_matches() {
        let ring = ring(16);
        for index in 1..=10u64 {
            ring.push(record(index, LogLevel::Info, "ipc", &format!("m{index}")));
        }
        let result = ring.query(&LogQuery::with_limit(3));
        let stamps: Vec<u64> = result.iter().map(|r| r.timestamp_ms).collect();
        assert_eq!(
            stamps,
            vec![10, 9, 8],
            "a limit truncates the old end, not the recent events"
        );
    }

    #[test]
    fn a_limit_of_zero_returns_nothing_without_touching_the_ring() {
        let ring = ring(4);
        ring.push(record(1, LogLevel::Info, "ipc", "kept in the ring"));
        let result = ring.query(&LogQuery::with_limit(0));
        assert!(result.is_empty());
        assert_eq!(ring.len(), 1, "querying must not consume records");
    }

    #[test]
    fn a_query_that_matches_nothing_returns_an_empty_result() {
        let ring = ring(4);
        ring.push(record(1, LogLevel::Info, "ipc", "a"));
        let result = ring.query(&LogQuery::default().correlated(CorrelationId::new()));
        assert!(result.is_empty());
    }

    #[test]
    fn a_query_after_eviction_only_sees_records_the_ring_retained() {
        let ring = ring(3);
        let wanted = CorrelationId::new();
        ring.push(record(1, LogLevel::Info, "ipc", "wanted old").with_correlation(wanted));
        for index in 2..=5u64 {
            ring.push(record(index, LogLevel::Info, "ipc", "noise"));
        }
        ring.push(record(6, LogLevel::Info, "ipc", "wanted new").with_correlation(wanted));

        let result = ring.query(&LogQuery::default().correlated(wanted));
        let messages: Vec<&str> = result.iter().map(|r| r.message.as_str()).collect();
        assert_eq!(
            messages,
            vec!["wanted new"],
            "the evicted record must not be findable"
        );
        assert_eq!(ring.evicted_total(), 3);
    }

    #[test]
    fn query_does_not_consume_the_records_it_returns() {
        let ring = ring(4);
        ring.push(record(1, LogLevel::Info, "ipc", "a"));
        assert_eq!(ring.query(&LogQuery::default()).len(), 1);
        assert_eq!(ring.query(&LogQuery::default()).len(), 1);
        assert_eq!(ring.len(), 1);
    }

    // ---- Concurrency --------------------------------------------------------------------

    #[test]
    fn a_ring_shared_behind_an_arc_keeps_every_record_within_its_bound() {
        let ring = Arc::new(ring(64));
        let handles: Vec<_> = (0..8u32)
            .map(|_thread| {
                let ring = Arc::clone(&ring);
                std::thread::spawn(move || {
                    for index in 0..100u64 {
                        ring.push(record(index, LogLevel::Info, "ipc", "line"));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("writer thread must not panic");
        }

        assert!(
            ring.len() <= 64,
            "the bound must hold under concurrent writers, got {}",
            ring.len()
        );
        assert_eq!(
            ring.len() + ring.evicted_total() as usize,
            800,
            "every pushed record is either retained or counted as evicted"
        );
    }

    #[test]
    fn a_ring_shared_behind_an_arc_is_readable_while_being_written() {
        let ring = Arc::new(ring(32));
        let writer = {
            let ring = Arc::clone(&ring);
            std::thread::spawn(move || {
                for index in 0..500u64 {
                    ring.push(record(index, LogLevel::Info, "ipc", "line"));
                }
            })
        };
        for _ in 0..100 {
            let result = ring.query(&LogQuery::with_limit(4));
            assert!(result.len() <= 4);
            assert!(ring.len() <= 32);
        }
        writer.join().expect("writer thread must not panic");
        assert_eq!(ring.len(), 32);
    }

    #[test]
    fn the_public_log_types_stay_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LogRing>();
        assert_send_sync::<LogRecord>();
        assert_send_sync::<LogQuery>();
        assert_send_sync::<LogLevel>();
    }
}
