//! The structured error model.
//!
//! Every fallible operation in NC-UI returns [`Result`]. The error type, [`NcError`],
//! answers three questions that a bare `String` or a bare `std::error::Error` cannot:
//!
//! 1. **What class of failure was this?** — [`ErrorKind`], a closed set that callers
//!    can match on exhaustively and that maps onto runtime policy.
//! 2. **What operation was running?** — the operation string is attached at the point
//!    the error is created, so it names the step, not just the subsystem.
//! 3. **Which logical operation does this belong to?** — the [`CorrelationId`], so the
//!    error can be joined against traces, metrics and audit events (spec 50, 93).
//!
//! Errors are never swallowed. Library code in this workspace does not contain an empty
//! `catch` equivalent; where an error is recovered from, the recovery is explicit and
//! the error is still recorded.

use core::fmt;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use std::borrow::Cow;
use thiserror::Error;

use crate::ids::CorrelationId;

/// The closed set of failure classes produced by the runtime.
///
/// This enum is `#[non_exhaustive]`: new failure modes will be added, and downstream
/// matches must therefore include a fallback arm. It is *not* an invitation to use
/// `_ =>` inside this workspace; `ncui-runtime` matches every variant explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ErrorKind {
    /// A supplied identifier was structurally invalid.
    #[error("invalid identifier: {reason}")]
    InvalidId {
        /// Explanation of why the identifier was rejected.
        reason: String,
    },

    /// A capability required by the operation was not granted to the subject.
    #[error("capability `{capability}` is not granted to {subject}")]
    CapabilityDenied {
        /// The capability that was required.
        capability: String,
        /// The principal (agent, component, window) that lacked it.
        subject: String,
    },

    /// A caller-supplied value failed validation.
    #[error("invalid {field}: {reason}")]
    Validation {
        /// The field or parameter that was invalid.
        field: String,
        /// Why it was invalid.
        reason: String,
    },

    /// A resource, rate or size limit was exceeded.
    #[error("{resource} limit exceeded: limit {limit}, observed {observed}")]
    LimitExceeded {
        /// The limited resource, e.g. `"ipc.frame.bytes"`.
        resource: String,
        /// The configured limit.
        limit: u64,
        /// The value that exceeded it.
        observed: u64,
    },

    /// A bounded wait elapsed before completion.
    #[error("timed out after {elapsed_ms}ms")]
    Timeout {
        /// How long the operation waited, in milliseconds.
        elapsed_ms: u64,
    },

    /// The operation was cancelled before or during execution.
    #[error("operation cancelled")]
    Cancelled,

    /// A referenced entity does not exist.
    #[error("{kind} not found: {id}")]
    NotFound {
        /// The entity class, e.g. `"component"`.
        kind: String,
        /// The identifier that was looked up.
        id: String,
    },

    /// The operation conflicts with current state.
    #[error("conflict: {reason}")]
    Conflict {
        /// Why the operation conflicts.
        reason: String,
    },

    /// A peer sent a message that violates the wire protocol.
    ///
    /// This is always a bug in the peer or a corrupted stream. It is never the fault of
    /// the payload's semantic content, which is reported as [`ErrorKind::Validation`].
    #[error("protocol violation: {reason}")]
    Protocol {
        /// Which part of the protocol was violated.
        reason: String,
    },

    /// A bounded queue rejected work because it was full.
    #[error("backpressure on {channel}: capacity {capacity} reached")]
    Backpressure {
        /// The channel that is saturated.
        channel: String,
        /// The configured capacity.
        capacity: usize,
    },

    /// An I/O operation failed.
    ///
    /// The OS error is stored decomposed rather than as an `std::io::Error` so that
    /// `ErrorKind` remains `Clone + PartialEq + Eq`. That matters: tests assert on
    /// specific failures, and the agent API compares error values across a process
    /// boundary where only a structured representation survives.
    #[error("i/o failure during {operation}: {message} ({io_kind:?})")]
    Io {
        /// The operation being attempted.
        operation: String,
        /// The `std::io::ErrorKind` classification.
        io_kind: std::io::ErrorKind,
        /// The OS message, preserved verbatim for diagnostics.
        message: String,
    },

    /// Serialization or deserialization failed.
    #[error("codec failure in {operation}: {reason}")]
    Codec {
        /// The encoding direction or stage.
        operation: String,
        /// Why encoding or decoding failed.
        reason: String,
    },

    /// A subsystem failed in a way that indicates a defect rather than bad input.
    ///
    /// This is the correct classification for invariant violations, unreachable branches
    /// reached through corrupted state, and broken platform contracts. It is deliberately
    /// loud: a production build surfaces it as a crash diagnostic (spec 56).
    #[error("internal failure in {operation}: {reason}")]
    Internal {
        /// The operation that failed.
        operation: String,
        /// Detail about the invariant that was violated.
        reason: String,
    },
}

impl ErrorKind {
    /// Whether retrying the same operation could plausibly succeed.
    ///
    /// This is a *policy* classification, not a prediction. `Timeout`, `Backpressure`
    /// and `Io` are retryable; `Validation`, `Protocol` and `InvalidId` are not, because
    /// the same input would fail the same way. `Internal` is treated as not retryable:
    /// repeating a run that trips an invariant usually indicates a defect, not a
    /// transient condition, and retrying would mask it.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Timeout { .. } | Self::Backpressure { .. } | Self::Io { .. }
        )
    }

    /// Whether the failure indicates a defect in the runtime rather than bad input.
    #[must_use]
    pub const fn is_defect(&self) -> bool {
        matches!(self, Self::Internal { .. })
    }

    /// A stable, machine-readable discriminant for metrics, audit events and JSON output.
    ///
    /// These strings are part of the agent-facing API surface: they appear in
    /// `ncui` JSON output and in audit records, so changing one is a breaking change.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidId { .. } => "invalid_id",
            Self::CapabilityDenied { .. } => "capability_denied",
            Self::Validation { .. } => "validation",
            Self::LimitExceeded { .. } => "limit_exceeded",
            Self::Timeout { .. } => "timeout",
            Self::Cancelled => "cancelled",
            Self::NotFound { .. } => "not_found",
            Self::Conflict { .. } => "conflict",
            Self::Protocol { .. } => "protocol",
            Self::Backpressure { .. } => "backpressure",
            Self::Io { .. } => "io",
            Self::Codec { .. } => "codec",
            Self::Internal { .. } => "internal",
        }
    }
}

/// A structured runtime error: a failure class, the operation that produced it, and
/// optionally the correlation identifier of the request it belongs to.
#[derive(Debug, Clone)]
pub struct NcError {
    kind: ErrorKind,
    operation: Cow<'static, str>,
    correlation: Option<CorrelationId>,
}

impl NcError {
    /// Creates an error of the given class.
    #[must_use]
    pub fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            operation: Cow::Borrowed("unspecified"),
            correlation: None,
        }
    }

    /// Records the operation that failed.
    ///
    /// Called at the point the error is constructed, not at the point it is returned, so
    /// the recorded operation is the innermost failing step.
    #[must_use]
    pub fn operation(mut self, operation: impl Into<Cow<'static, str>>) -> Self {
        self.operation = operation.into();
        self
    }

    /// Attaches the correlation identifier of the surrounding request.
    #[must_use]
    pub fn correlated(mut self, correlation: CorrelationId) -> Self {
        self.correlation = Some(correlation);
        self
    }

    /// The failure class.
    #[must_use]
    pub const fn kind(&self) -> &ErrorKind {
        &self.kind
    }

    /// The operation that failed.
    #[must_use]
    pub fn operation_name(&self) -> &str {
        &self.operation
    }

    /// The correlation identifier, when the error arose under a correlated request.
    #[must_use]
    pub const fn correlation(&self) -> Option<CorrelationId> {
        self.correlation
    }

    /// Whether retrying could plausibly succeed. See [`ErrorKind::is_retryable`].
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }

    /// Whether the failure indicates a runtime defect. See [`ErrorKind::is_defect`].
    #[must_use]
    pub const fn is_defect(&self) -> bool {
        self.kind.is_defect()
    }

    /// The stable machine-readable error code. See [`ErrorKind::code`].
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.kind.code()
    }
}

impl fmt::Display for NcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {}", self.operation, self.kind)?;
        if let Some(correlation) = self.correlation {
            write!(f, " (correlation {correlation})")?;
        }
        Ok(())
    }
}

impl std::error::Error for NcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

impl Serialize for NcError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let correlation: Option<String> = self.correlation.map(|id| id.to_string());
        let mut state = serializer.serialize_struct("NcError", 5)?;
        state.serialize_field("code", self.code())?;
        state.serialize_field("message", &self.to_string())?;
        state.serialize_field("operation", &self.operation.as_ref())?;
        state.serialize_field("correlation", &correlation)?;
        state.serialize_field("retryable", &self.is_retryable())?;
        state.end()
    }
}

impl From<std::io::Error> for NcError {
    fn from(source: std::io::Error) -> Self {
        Self::new(ErrorKind::Io {
            operation: "unspecified".to_owned(),
            io_kind: source.kind(),
            message: source.to_string(),
        })
    }
}

/// The workspace-wide result alias.
pub type Result<T, E = NcError> = core::result::Result<T, E>;

/// Builds a [`ErrorKind::Validation`] error.
#[must_use]
pub fn validation(field: impl Into<String>, reason: impl Into<String>) -> NcError {
    NcError::new(ErrorKind::Validation {
        field: field.into(),
        reason: reason.into(),
    })
}

/// Builds a [`ErrorKind::NotFound`] error.
#[must_use]
pub fn not_found(kind: impl Into<String>, id: impl fmt::Display) -> NcError {
    NcError::new(ErrorKind::NotFound {
        kind: kind.into(),
        id: id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::CorrelationId;

    #[test]
    fn display_includes_operation_and_correlation() {
        let err = NcError::new(ErrorKind::Cancelled)
            .operation("ipc::dispatch")
            .correlated(CorrelationId::new());
        let text = err.to_string();
        assert!(text.contains("ipc::dispatch"), "got: {text}");
        assert!(text.contains("correlation"), "got: {text}");
    }

    #[test]
    fn retryability_follows_documented_policy() {
        assert!(ErrorKind::Timeout { elapsed_ms: 1 }.is_retryable());
        assert!(
            ErrorKind::Backpressure {
                channel: "c".into(),
                capacity: 1
            }
            .is_retryable()
        );
        assert!(!ErrorKind::Cancelled.is_retryable());
        assert!(
            !ErrorKind::Protocol {
                reason: "bad".into()
            }
            .is_retryable()
        );
        assert!(
            !ErrorKind::Internal {
                operation: "o".into(),
                reason: "r".into()
            }
            .is_retryable()
        );
    }

    #[test]
    fn every_kind_has_a_distinct_stable_code() {
        let kinds = [
            ErrorKind::InvalidId { reason: "r".into() },
            ErrorKind::CapabilityDenied {
                capability: "c".into(),
                subject: "s".into(),
            },
            ErrorKind::Validation {
                field: "f".into(),
                reason: "r".into(),
            },
            ErrorKind::LimitExceeded {
                resource: "r".into(),
                limit: 1,
                observed: 2,
            },
            ErrorKind::Timeout { elapsed_ms: 1 },
            ErrorKind::Cancelled,
            ErrorKind::NotFound {
                kind: "k".into(),
                id: "i".into(),
            },
            ErrorKind::Conflict { reason: "r".into() },
            ErrorKind::Protocol { reason: "r".into() },
            ErrorKind::Backpressure {
                channel: "c".into(),
                capacity: 1,
            },
            ErrorKind::Codec {
                operation: "o".into(),
                reason: "r".into(),
            },
            ErrorKind::Internal {
                operation: "o".into(),
                reason: "r".into(),
            },
        ];
        let mut codes: Vec<_> = kinds.iter().map(ErrorKind::code).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(before, codes.len(), "error codes must be unique");
    }

    #[test]
    fn serializes_to_a_structured_object() {
        let id = CorrelationId::new();
        let err = NcError::new(ErrorKind::Timeout { elapsed_ms: 250 })
            .operation("worker.join")
            .correlated(id);
        let value: serde_json::Value = serde_json::to_value(&err).expect("serializable");
        assert_eq!(value["code"], "timeout");
        assert_eq!(value["operation"], "worker.join");
        assert_eq!(value["retryable"], true);
        assert_eq!(value["correlation"], id.to_string());
    }

    #[test]
    fn io_errors_preserve_their_classification_and_message() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let err: NcError = io.into();
        assert!(matches!(
            err.kind(),
            ErrorKind::Io {
                io_kind: std::io::ErrorKind::NotFound,
                ..
            }
        ));
        assert!(err.to_string().contains("missing"), "got: {err}");
        assert!(err.is_retryable());
    }
}
