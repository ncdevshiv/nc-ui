//! Structured logging, metrics with percentiles, and correlation propagation for the
//! NC-UI runtime.
//!
//! This crate is the observability spine of the runtime: it answers "what happened, how
//! often, how slowly, and which user action caused it". It has three jobs and no others.
//!
//! 1. **Install one process-wide `tracing` subscriber** ([`tracing_setup`]) in a shape the
//!    operator chooses — pretty for a developer, JSON for a production log shipper — and
//!    under a verbosity that matches the deployment.
//! 2. **Carry one [`ncui_core::CorrelationId`] through every stage** of a logical operation
//!    ([`crate::trace`]), so a single user action can be followed end to end across threads,
//!    spans, log records and errors (spec 50).
//! 3. **Keep a bounded, queryable record of what happened** ([`metrics`], [`logring`],
//!    [`exporter`]): a metrics registry that evicts rather than grows without limit, an
//!    in-memory log ring backing `ncui logs.query` (spec 35), and a JSON Lines sink.
//!
//! # Boundedness is the contract
//!
//! Every structure in this crate is bounded by a limit from [`ncui_core::Limits`]:
//! [`MetricsRegistry`] by `max_metric_series` and `max_histogram_samples`, [`LogRing`] by
//! `log_ring_capacity`. When a bound is reached this crate **evicts the oldest data and
//! counts the eviction** rather than growing without limit or failing silently. A
//! capacity that is exceeded is a visible event, not an invisible one — the registry
//! maintains `metrics_series_evicted_total` precisely so that pressure on the telemetry
//! path is itself observable.
//!
//! # Layering
//!
//! This crate sits directly above [`ncui_core`] (ADR-011). It depends on `ncui-core`,
//! `serde`, `serde_json`, `tracing` and `tracing-subscriber`, and on nothing else — in
//! particular it performs no filesystem or network I/O of its own. Writing telemetry to a
//! sink is the caller's decision ([`exporter`] takes an arbitrary [`std::io::Write`]), so
//! the runtime binary owns the destination and this crate owns the formatting.
//!
//! # Determinism
//!
//! Nothing here reads a wall clock and nothing sleeps. Ordering is explicit: the log ring
//! and the metric-series recency order are driven by the sequence of operations, so tests
//! and recorded fixtures are reproducible. Timestamps are *supplied by the caller*,
//! because a runtime that cannot be tested deterministically is a runtime whose telemetry
//! cannot be trusted.

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

pub mod exporter;
pub mod logring;
pub mod metrics;
pub mod trace;
pub mod tracing_setup;

pub use exporter::{ExportEvent, JsonLinesExporter};
pub use logring::{LogLevel, LogQuery, LogRecord, LogRing, RecordError};
pub use metrics::{
    EVICTION_COUNTER_NAME, HistogramStats, Labels, MetricKind, MetricsRegistry, SeriesSnapshot,
};
pub use trace::{TraceContext, TraceScope};
pub use tracing_setup::{LogFormat, LogMode, TracingConfig};

/// The name of the environment variable that supplies the log filter.
///
/// The value is an [`tracing_subscriber::EnvFilter`] directive string such as
/// `info,ncui_ipc=debug`. Unset means "use the mode's documented default".
pub const LOG_FILTER_ENV: &str = "NCUI_LOG";

/// The span name used for a traced logical operation.
///
/// Every stage of one user action lives under a span with this name, so a backend can
/// select them as a group, and the correlation identifier recorded on them joins them to
/// the log records and errors produced alongside.
pub const OPERATION_SPAN_NAME: &str = "ncui.operation";

/// The span field name that carries the correlation identifier of the current operation.
///
/// [`TraceContext::enter`] records this field onto the enclosing span, and
/// [`trace::operation_span`] declares it, so the two are deliberately the same string:
/// a field recorded under a name the span did not declare is dropped by `tracing`, and
/// a silent drop is exactly the correlation gap spec 50 exists to prevent.
pub const CORRELATION_FIELD: &str = "correlation";

/// The span field name that names the runtime component performing the operation.
///
/// Used to split a trace by subsystem without relying on the `tracing` target, which is
/// the crate path and therefore changes when the code is refactored.
pub const COMPONENT_FIELD: &str = "component";
