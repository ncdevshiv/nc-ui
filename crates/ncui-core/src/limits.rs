//! Resource limits.
//!
//! Every bounded resource in the runtime has a limit, and every limit is defined here so
//! that it is configurable in one place, validated as a unit, and reported by `ncui
//! status`. A limit that is not expressed in this struct is a defect: it means some
//! subsystem is growing without a bound, which spec 96 forbids.
//!
//! Limits are validated on construction. A configuration that would leave the runtime
//! in a degraded state — a request timeout of zero, a frame limit below the protocol
//! header size, a burst smaller than the sustained rate — is rejected rather than
//! accepted and discovered later.

use serde::{Deserialize, Serialize};

use crate::error::{NcError, Result, validation};

/// The largest legal IPC frame, imposed by the wire format's length field.
///
/// A frame is `[magic:4][version:1][kind:1][flags:2][length:4]` = 12 bytes of header
/// followed by at most `u32::MAX` bytes of payload. The protocol caps the payload well
/// below the field's maximum so that a hostile length can never cause a large
/// allocation before the frame is validated.
pub const PROTOCOL_MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Bounded resources shared across the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Maximum accepted size of a single IPC frame, header included.
    pub max_ipc_frame_bytes: u32,
    /// Maximum number of in-flight requests awaiting a response.
    pub max_in_flight_requests: usize,
    /// How long a request may wait for a response before it fails.
    pub request_timeout_ms: u64,
    /// Maximum number of concurrent native workers.
    pub max_workers: usize,
    /// Maximum queued tasks per worker before backpressure engages.
    pub max_worker_queue_depth: usize,
    /// Maximum size of a single worker task payload.
    pub max_worker_payload_bytes: usize,
    /// How long `WorkerPool::shutdown` waits for in-flight tasks before giving up.
    ///
    /// A task payload is an ordinary closure with no cancellation token, so a task that
    /// blocks forever would otherwise block shutdown forever — and because `Drop` shuts the
    /// pool down, it would block the drop too, including at process exit. Rust cannot kill a
    /// thread, so the honest answer is a deadline: past it, shutdown stops waiting, reports
    /// how many threads did not stop, and leaves them to exit when their task eventually
    /// returns. A task that returns normally still runs to completion; nothing is aborted.
    pub worker_shutdown_timeout_ms: u64,
    /// Entries retained by each in-memory log ring.
    pub log_ring_capacity: usize,
    /// Distinct metric series retained before the oldest are evicted.
    pub max_metric_series: usize,
    /// Samples retained per latency histogram before compaction.
    pub max_histogram_samples: usize,
    /// Maximum number of live windows.
    pub max_windows: usize,
    /// Sustained agent request rate, in requests per second.
    pub agent_rate_per_second: u32,
    /// Burst allowance on top of the sustained agent request rate.
    pub agent_rate_burst: u32,
}

impl Default for Limits {
    /// Conservative defaults chosen for a desktop application.
    ///
    /// These are deliberately not tuned for maximum throughput. The benchmark suite
    /// reports where they bind; raising them is a measured decision, not a guess.
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(8, std::num::NonZeroUsize::get);
        Self {
            max_ipc_frame_bytes: 16 * 1024 * 1024,
            max_in_flight_requests: 4096,
            request_timeout_ms: 30_000,
            max_workers: cpus.clamp(1, 64),
            max_worker_queue_depth: 1024,
            max_worker_payload_bytes: 4 * 1024 * 1024,
            worker_shutdown_timeout_ms: 30_000,
            log_ring_capacity: 8192,
            max_metric_series: 4096,
            max_histogram_samples: 8192,
            max_windows: 64,
            agent_rate_per_second: 200,
            agent_rate_burst: 400,
        }
    }
}

impl Limits {
    /// Validates that the limits are internally consistent and safely bounded.
    ///
    /// Returns the first violation found. Callers that load configuration from disk must
    /// surface the field name so a user can correct it (spec 29).
    pub fn validate(&self) -> Result<()> {
        let check = |condition: bool, field: &str, reason: &'static str| -> Result<()> {
            if condition {
                Ok(())
            } else {
                Err(validation(field, reason))
            }
        };

        check(
            self.max_ipc_frame_bytes > 0,
            "max_ipc_frame_bytes",
            "must be greater than zero",
        )?;
        check(
            self.max_ipc_frame_bytes <= PROTOCOL_MAX_FRAME_BYTES,
            "max_ipc_frame_bytes",
            "exceeds the maximum representable by the protocol length field",
        )?;
        check(
            self.max_in_flight_requests > 0,
            "max_in_flight_requests",
            "must be greater than zero",
        )?;
        check(
            self.request_timeout_ms > 0,
            "request_timeout_ms",
            "must be greater than zero",
        )?;
        check(
            self.max_workers > 0,
            "max_workers",
            "must be greater than zero",
        )?;
        check(
            self.max_workers <= 1024,
            "max_workers",
            "exceeds the supported maximum of 1024 concurrent workers",
        )?;
        check(
            self.max_worker_queue_depth > 0,
            "max_worker_queue_depth",
            "must be greater than zero",
        )?;
        check(
            self.max_worker_payload_bytes > 0,
            "max_worker_payload_bytes",
            "must be greater than zero",
        )?;
        check(
            self.worker_shutdown_timeout_ms > 0,
            "worker_shutdown_timeout_ms",
            "must be greater than zero: a zero deadline would abandon every worker thread \
             that had not already finished, which is not what a shutdown timeout means",
        )?;
        check(
            self.max_worker_payload_bytes as u64 <= u64::from(self.max_ipc_frame_bytes),
            "max_worker_payload_bytes",
            "must not exceed max_ipc_frame_bytes; a task larger than a frame cannot be transported",
        )?;
        check(
            self.log_ring_capacity > 0,
            "log_ring_capacity",
            "must be greater than zero",
        )?;
        check(
            self.max_metric_series > 0,
            "max_metric_series",
            "must be greater than zero",
        )?;
        check(
            self.max_histogram_samples >= 64,
            "max_histogram_samples",
            "must be at least 64 to compute meaningful percentiles",
        )?;
        check(
            self.max_windows > 0,
            "max_windows",
            "must be greater than zero",
        )?;
        check(
            self.agent_rate_per_second > 0,
            "agent_rate_per_second",
            "must be greater than zero",
        )?;
        check(
            self.agent_rate_burst >= self.agent_rate_per_second,
            "agent_rate_burst",
            "must be at least agent_rate_per_second, otherwise the sustained rate is unreachable",
        )?;
        Ok(())
    }
}

/// Builder for [`Limits`], used by tests and by subsystems that need a tighter bound
/// than the default.
#[derive(Debug, Clone, Copy, Default)]
pub struct LimitsBuilder {
    limits: Limits,
}

impl LimitsBuilder {
    /// Starts from the default limits.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limits: Limits::default(),
        }
    }

    /// Overrides the maximum IPC frame size.
    #[must_use]
    pub const fn max_ipc_frame_bytes(mut self, value: u32) -> Self {
        self.limits.max_ipc_frame_bytes = value;
        self
    }

    /// Overrides the in-flight request ceiling.
    #[must_use]
    pub const fn max_in_flight_requests(mut self, value: usize) -> Self {
        self.limits.max_in_flight_requests = value;
        self
    }

    /// Overrides the request timeout.
    #[must_use]
    pub const fn request_timeout_ms(mut self, value: u64) -> Self {
        self.limits.request_timeout_ms = value;
        self
    }

    /// Overrides the worker count ceiling.
    #[must_use]
    pub const fn max_workers(mut self, value: usize) -> Self {
        self.limits.max_workers = value;
        self
    }

    /// Overrides the per-worker queue depth.
    #[must_use]
    pub const fn max_worker_queue_depth(mut self, value: usize) -> Self {
        self.limits.max_worker_queue_depth = value;
        self
    }

    /// Overrides the worker payload ceiling.
    #[must_use]
    pub const fn max_worker_payload_bytes(mut self, value: usize) -> Self {
        self.limits.max_worker_payload_bytes = value;
        self
    }

    /// Sets how long shutdown waits for in-flight tasks.
    #[must_use]
    pub const fn worker_shutdown_timeout_ms(mut self, value: u64) -> Self {
        self.limits.worker_shutdown_timeout_ms = value;
        self
    }

    /// Overrides the log ring capacity.
    #[must_use]
    pub const fn log_ring_capacity(mut self, value: usize) -> Self {
        self.limits.log_ring_capacity = value;
        self
    }

    /// Overrides the metric series ceiling.
    #[must_use]
    pub const fn max_metric_series(mut self, value: usize) -> Self {
        self.limits.max_metric_series = value;
        self
    }

    /// Overrides the histogram sample ceiling.
    #[must_use]
    pub const fn max_histogram_samples(mut self, value: usize) -> Self {
        self.limits.max_histogram_samples = value;
        self
    }

    /// Overrides the window ceiling.
    #[must_use]
    pub const fn max_windows(mut self, value: usize) -> Self {
        self.limits.max_windows = value;
        self
    }

    /// Overrides the agent rate limit.
    #[must_use]
    pub const fn agent_rate(mut self, per_second: u32, burst: u32) -> Self {
        self.limits.agent_rate_per_second = per_second;
        self.limits.agent_rate_burst = burst;
        self
    }

    /// Validates and returns the limits.
    pub fn build(self) -> Result<Limits> {
        self.limits.validate()?;
        Ok(self.limits)
    }
}

/// Verifies that [`Limits::default`] is itself valid, so that a subsystem which never
/// loads configuration still starts in a coherent state.
pub fn validate_defaults() -> Result<()> {
    Limits::default().validate()
}

/// Re-export so callers can construct a limit violation without importing two modules.
#[must_use]
pub fn limit_exceeded(resource: &str, limit: u64, observed: u64) -> NcError {
    NcError::new(crate::error::ErrorKind::LimitExceeded {
        resource: resource.to_owned(),
        limit,
        observed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        validate_defaults().expect("default limits must be coherent");
    }

    #[test]
    fn builder_round_trips_and_validates() {
        let limits = LimitsBuilder::new()
            .max_workers(2)
            .max_worker_queue_depth(4)
            .max_in_flight_requests(8)
            .request_timeout_ms(1_000)
            .build()
            .expect("valid limits");
        assert_eq!(limits.max_workers, 2);
        assert_eq!(limits.max_worker_queue_depth, 4);
    }

    #[test]
    fn zero_values_are_rejected_with_the_field_name() {
        let err = LimitsBuilder::new()
            .request_timeout_ms(0)
            .build()
            .expect_err("zero timeout must be rejected");
        assert!(err.to_string().contains("request_timeout_ms"), "got: {err}");
    }

    #[test]
    fn payload_larger_than_a_frame_is_rejected() {
        let err = LimitsBuilder::new()
            .max_ipc_frame_bytes(1024)
            .max_worker_payload_bytes(4096)
            .build()
            .expect_err("payload exceeding frame limit must be rejected");
        assert!(
            err.to_string().contains("max_worker_payload_bytes"),
            "got: {err}"
        );
    }

    #[test]
    fn burst_below_sustained_rate_is_rejected() {
        let err = LimitsBuilder::new()
            .agent_rate(100, 50)
            .build()
            .expect_err("burst below sustained rate must be rejected");
        assert!(err.to_string().contains("agent_rate_burst"), "got: {err}");
    }

    #[test]
    fn unknown_configuration_fields_are_rejected() {
        // Schema validation must reject unknown fields rather than silently ignoring
        // them, so a typo in a config file surfaces immediately (spec 29).
        let json = r#"{"max_workers":2,"max_windos":4}"#;
        assert!(serde_json::from_str::<Limits>(json).is_err());
    }

    #[test]
    fn defaults_round_trip_through_json() {
        let defaults = Limits::default();
        let json = serde_json::to_string(&defaults).expect("serializable");
        let back: Limits = serde_json::from_str(&json).expect("deserializable");
        assert_eq!(back, defaults);
    }
}
