//! Core vocabulary for the NC-UI runtime.
//!
//! Every other `ncui-*` crate depends on this one and nothing in this crate depends on
//! any of them. It defines the shared error model, the typed identifier scheme, the
//! capability model, semantic versions, and the resource limits applied across the
//! runtime. It deliberately contains no I/O, no threading, and no platform integration:
//! it is the vocabulary, not the machinery.
//!
//! # Layering
//!
//! This crate sits at the bottom of the dependency graph (ADR-011). It may only depend
//! on external crates, never on another `ncui-*` crate.
//!
//! # Error model
//!
//! All fallible operations in NC-UI return [`Result<T>`], whose error is [`NcError`].
//! An `NcError` always carries the operation that failed and, where the operation was
//! part of a correlated request, the [`CorrelationId`] that ties together every log,
//! span, metric and event emitted while handling it. Errors are never silently dropped;
//! see [`NcError::is_retryable`] for the classification callers should branch on.

// Test code deliberately exercises panicking paths: a failed assertion or an
// `unwrap` in a test must abort so the failure is visible.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

pub mod capability;
pub mod error;
pub mod ids;
pub mod limits;
pub mod version;

pub use capability::{Capability, CapabilityScope, CapabilitySet, ParseCapabilityError};
pub use error::{ErrorKind, NcError, Result};
pub use ids::{ComponentId, CorrelationId, DupIdGenerator, RequestId, TaskId, WindowId, WorkerId};
pub use limits::{Limits, LimitsBuilder};
pub use version::SemVer;
