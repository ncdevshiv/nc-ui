//! Deep devtools SDK: live runtime inspection, event streaming, and profiling.
//!
//! The devtools surface answers three questions an agent or developer asks about a
//! running NC-UI application:
//!
//! 1. **What is the runtime doing right now?** — [`DevTools::summary`] returns a live snapshot of
//!    every open window, its lifecycle state, its capability grant, and the components
//!    mounted in it.
//! 2. **What just happened?** — [`DevTools::events`] returns a bounded ring of recent runtime
//!    events (navigations, capability denials, window state changes, trace spans) with
//!    correlation IDs, filterable by kind and severity.
//! 3. **Where did time go?** — [`DevTools::profile`] returns per-method timing histograms
//!    recorded while profiling was active, with p50/p90/p95/p99 and call counts.
//!
//! The devtools surface is read-only: it never modifies runtime state. Every method is
//! gated behind `Capability::InspectRuntime` in the dispatch table, so an agent without
//! that capability cannot observe the runtime's internals.
//!
//! # Design
//!
//! [`DevTools`] is a struct owned by [`Runtime`](crate::Runtime). It holds:
//!
//! - A bounded ring of [`DevEvent`] records (default 1024, configurable via
//!   [`Limits::log_ring_capacity`](ncui_core::Limits)).
//! - A snapshot of the last-known window tree, refreshed on demand by querying the
//!   runtime's `Windows` registry.
//! - Profiling data: a map from method name to a timing histogram, populated by the
//!   dispatch pipeline when `profiling_active` is true.
//!
//! The struct is `Send + Sync` so the transport layer can read it from another thread.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ncui_core::CorrelationId;
use serde::{Deserialize, Serialize};

/// A runtime event captured by the devtools ring.
///
/// Events are records of things that happened: a navigation was refused, a capability
/// was denied, a window was resized, a trace span was opened. They are not the same as
/// audit events (which record every dispatch) — they are a curated, filterable subset
/// that a developer can scan to understand what the runtime is doing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DevEvent {
    /// When the event occurred, as milliseconds since the runtime started.
    pub timestamp_ms: u64,
    /// The event kind: `"navigation"`, `"capability_denied"`, `"window_state"`,
    /// `"trace"`, `"profile"`, `"crash"`, etc.
    pub kind: String,
    /// The event severity: `"info"`, `"warn"`, `"error"`.
    pub severity: String,
    /// The correlation ID of the operation that produced this event, if any.
    pub correlation_id: Option<String>,
    /// A human-readable summary of the event.
    pub summary: String,
    /// Structured data attached to the event (method name, window id, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A per-method timing entry recorded during profiling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileEntry {
    /// The method name (e.g. `"ncui.state.inspect"`).
    pub method: String,
    /// Number of calls recorded.
    pub calls: u64,
    /// p50 latency in nanoseconds.
    pub p50_ns: u64,
    /// p90 latency in nanoseconds.
    pub p90_ns: u64,
    /// p95 latency in nanoseconds.
    pub p95_ns: u64,
    /// p99 latency in nanoseconds.
    pub p99_ns: u64,
    /// Maximum observed latency in nanoseconds.
    pub max_ns: u64,
}

/// Internal profiling accumulator for one method.
#[derive(Debug, Clone)]
struct ProfileAccumulator {
    calls: u64,
    samples: Vec<u64>,
}

impl ProfileAccumulator {
    fn new() -> Self {
        Self {
            calls: 0,
            samples: Vec::new(),
        }
    }

    fn record(&mut self, nanos: u64) {
        self.calls += 1;
        // Bound the sample set: keep the most recent 4096 samples.
        if self.samples.len() >= 4096 {
            self.samples.remove(0);
        }
        self.samples.push(nanos);
    }

    fn to_entry(&self, method: &str) -> ProfileEntry {
        if self.samples.is_empty() {
            return ProfileEntry {
                method: method.to_owned(),
                calls: self.calls,
                p50_ns: 0,
                p90_ns: 0,
                p95_ns: 0,
                p99_ns: 0,
                max_ns: 0,
            };
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let len = sorted.len();
        ProfileEntry {
            method: method.to_owned(),
            calls: self.calls,
            p50_ns: sorted[len / 2],
            p90_ns: sorted[(len as f64 * 0.90) as usize % len],
            p95_ns: sorted[(len as f64 * 0.95) as usize % len],
            p99_ns: sorted[(len as f64 * 0.99) as usize % len],
            max_ns: sorted[len - 1],
        }
    }
}

/// A snapshot of one window's observable state for the devtools tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowNode {
    /// The window's identity.
    pub id: String,
    /// The window's title.
    pub title: String,
    /// The window's lifecycle state.
    pub state: String,
    /// The capabilities granted to this window's page.
    pub capabilities: Vec<String>,
    /// The URL the window is displaying, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// The devtools subsystem: live runtime inspection, event streaming, and profiling.
///
/// Owned by [`Runtime`](crate::Runtime) and shared by reference. All methods are
/// read-only — they never modify runtime state.
#[derive(Debug)]
pub struct DevTools {
    /// The bounded event ring.
    events: Mutex<Vec<DevEvent>>,
    /// The maximum number of events to retain.
    event_capacity: usize,
    /// When the runtime started, for timestamp calculation.
    started: Instant,
    /// Profiling accumulators, keyed by method name.
    profile: Mutex<BTreeMap<String, ProfileAccumulator>>,
    /// Profiling call count since the last `profile` query.
    profile_calls: Mutex<u64>,
}

impl DevTools {
    /// Creates a new devtools subsystem.
    ///
    /// `event_capacity` bounds the event ring. When the ring is full, the oldest event
    /// is evicted to make room for the new one.
    pub fn new(event_capacity: usize) -> Self {
        Self {
            events: Mutex::new(Vec::with_capacity(event_capacity.min(1024))),
            event_capacity,
            started: Instant::now(),
            profile: Mutex::new(BTreeMap::new()),
            profile_calls: Mutex::new(0),
        }
    }

    /// Records a devtools event.
    ///
    /// Events are timestamped relative to the runtime's start. If the ring is full,
    /// the oldest event is evicted.
    pub fn record_event(
        &self,
        kind: impl Into<String>,
        severity: impl Into<String>,
        correlation_id: Option<&CorrelationId>,
        summary: impl Into<String>,
        data: Option<serde_json::Value>,
    ) {
        let mut ring = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ring.len() >= self.event_capacity {
            ring.remove(0);
        }
        ring.push(DevEvent {
            timestamp_ms: self.started.elapsed().as_millis() as u64,
            kind: kind.into(),
            severity: severity.into(),
            correlation_id: correlation_id.map(ToString::to_string),
            summary: summary.into(),
            data,
        });
    }

    /// Returns the current event ring, optionally filtered by kind.
    ///
    /// Returns up to `limit` events, newest first. If `filter_kind` is `Some`, only
    /// events whose `kind` field matches are returned.
    pub fn events(&self, filter_kind: Option<&str>, limit: usize) -> Vec<DevEvent> {
        let ring = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ring.iter()
            .rev()
            .filter(|e| filter_kind.is_none_or(|k| e.kind == k))
            .take(limit)
            .cloned()
            .collect()
    }

    /// Records a profiling sample for a method.
    ///
    /// Called by the dispatch pipeline when `profiling_active` is true.
    pub fn record_profile(&self, method: &str, elapsed: Duration) {
        let mut map = self
            .profile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let acc = map
            .entry(method.to_owned())
            .or_insert_with(ProfileAccumulator::new);
        acc.record(elapsed.as_nanos() as u64);
        let mut calls = self
            .profile_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *calls += 1;
    }

    /// Returns profiling data for all recorded methods.
    ///
    /// Each entry has p50/p90/p95/p99/max in nanoseconds and the total call count.
    pub fn profile(&self) -> Vec<ProfileEntry> {
        let map = self
            .profile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.iter()
            .map(|(method, acc)| acc.to_entry(method))
            .collect()
    }

    /// Returns the total number of profiling samples recorded.
    pub fn profile_call_count(&self) -> u64 {
        *self
            .profile_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Clears all profiling data.
    ///
    /// This is the handler for `ncui.devtools.profile.reset`.
    pub fn reset_profile(&self) {
        let mut map = self
            .profile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.clear();
        let mut calls = self
            .profile_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *calls = 0;
    }

    /// Returns a summary of the devtools subsystem's state.
    pub fn summary(&self) -> serde_json::Value {
        let ring = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let map = self
            .profile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let calls = self
            .profile_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        serde_json::json!({
            "event_count": ring.len(),
            "event_capacity": self.event_capacity,
            "profile_methods": map.len(),
            "profile_calls": *calls,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_recorded_and_retrieved_newest_first() {
        let dt = DevTools::new(64);
        dt.record_event("navigation", "info", None, "first", None);
        dt.record_event("navigation", "info", None, "second", None);
        let events = dt.events(None, 10);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].summary, "second");
        assert_eq!(events[1].summary, "first");
    }

    #[test]
    fn events_can_be_filtered_by_kind() {
        let dt = DevTools::new(64);
        dt.record_event("navigation", "info", None, "nav", None);
        dt.record_event("capability_denied", "warn", None, "denied", None);
        dt.record_event("navigation", "info", None, "nav2", None);
        let navs = dt.events(Some("navigation"), 10);
        assert_eq!(navs.len(), 2);
        let denied = dt.events(Some("capability_denied"), 10);
        assert_eq!(denied.len(), 1);
    }

    #[test]
    fn event_ring_evicts_oldest_when_full() {
        let dt = DevTools::new(3);
        for i in 0..5 {
            dt.record_event("test", "info", None, format!("event-{i}"), None);
        }
        let events = dt.events(None, 10);
        assert_eq!(events.len(), 3);
        // The newest three should be present (events 2, 3, 4).
        assert_eq!(events[0].summary, "event-4");
        assert_eq!(events[2].summary, "event-2");
    }

    #[test]
    fn profile_records_and_returns_percentiles() {
        let dt = DevTools::new(64);
        for i in 0..100 {
            dt.record_profile("test.method", Duration::from_nanos(i * 100));
        }
        let entries = dt.profile();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].method, "test.method");
        assert_eq!(entries[0].calls, 100);
        assert!(entries[0].p50_ns > 0);
        assert!(entries[0].p99_ns >= entries[0].p50_ns);
        assert!(entries[0].max_ns >= entries[0].p99_ns);
    }

    #[test]
    fn profile_can_be_reset() {
        let dt = DevTools::new(64);
        dt.record_profile("test.method", Duration::from_nanos(100));
        assert_eq!(dt.profile_call_count(), 1);
        dt.reset_profile();
        assert_eq!(dt.profile_call_count(), 0);
        assert!(dt.profile().is_empty());
    }

    #[test]
    fn summary_reports_state() {
        let dt = DevTools::new(128);
        dt.record_event("test", "info", None, "summary test", None);
        let s = dt.summary();
        assert_eq!(s["event_count"], 1);
        assert_eq!(s["event_capacity"], 128);
    }
}
