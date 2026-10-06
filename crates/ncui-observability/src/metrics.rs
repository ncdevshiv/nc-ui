//! The metrics registry (spec 72).
//!
//! A metrics registry is easy to write and easy to write wrongly. The two failure modes
//! that matter in a long-running runtime are:
//!
//! 1. **Unbounded cardinality.** A metric with a user-controlled label — a window id, a
//!    component type, a request path — creates a new series every time the value changes.
//!    A registry that never forgets is a memory leak with a dashboard on top.
//! 2. **Summaries that hide the tail.** Reporting only a mean for a latency histogram
//!    makes a p99 that is a hundred times the mean look like normal variance. Spec 72
//!    requires the percentiles, so this module computes them for real.
//!
//! # Series identity
//!
//! A series is identified by its **name and its label set** — never its name alone. Two
//! series with different label sets are different series even if they share a name, and
//! [`Labels`] normalises and orders its pairs so that identity is a total order, not
//! "whatever order the caller happened to insert them in". That is what makes
//! [`MetricsRegistry::snapshot`] deterministic and what makes eviction fair.
//!
//! The kind is deliberately *not* part of the identity, so a counter that starts receiving
//! gauge writes is one series with a conflict, not two series that look unrelated on a
//! dashboard. See [`MetricKind`].
//!
//! # Eviction
//!
//! The registry holds at most `Limits::max_metric_series` series. Inserting past the cap
//! evicts the **least recently updated** series and increments [`EVICTION_COUNTER_NAME`].
//! Eviction is never silent: a registry that silently discards series produces a dashboard
//! that is confidently wrong, which is worse than one that reports pressure. Recency is a
//! monotonic tick counter rather than a wall clock, so eviction order is a pure function of
//! the operation sequence and the crate contains no clock reads at all.
//!
//! # Percentiles
//!
//! A histogram retains up to `Limits::max_histogram_samples` observations in a bounded
//! reservoir and computes its percentiles **exactly** from those retained values, by
//! sorting a copy and selecting by nearest rank. There is no interpolation, no bucket
//! boundaries and no estimation: for a given sample set the reported p99 *is* an observed
//! value.
//!
//! The accuracy caveat is therefore about **coverage**, not arithmetic:
//!
//! * While the reservoir has not filled, every percentile describes every observation ever
//!   made. The values are exact and `compacted_samples` is zero, so a consumer can tell.
//! * Once it is full, a new observation drops the **oldest** retained sample. The
//!   percentiles then describe a sliding window of the most recent
//!   `max_histogram_samples` observations, not all of history.
//! * A long-lived metric that stops receiving observations keeps reporting its last
//!   window's percentiles indefinitely. Nothing decays: the window is a tail of events,
//!   not a time interval.
//! * A slow burst followed by more than `max_histogram_samples` fast observations stops
//!   appearing in p99, because the burst has been compacted out. Raising the limit is the
//!   only way to widen the window; there is no correctness-preserving alternative once the
//!   cap binds.
//!
//! `count` and `sum` are **not** windowed: they are lifetime totals maintained
//! incrementally, so `sum / count` is a true lifetime mean even after compaction. Every
//! [`HistogramStats`] also reports `retained_samples` and `compacted_samples`, so a
//! consumer never has to guess whether a percentile is a whole-life or windowed figure.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use ncui_core::{ErrorKind, Limits, NcError, Result};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The name of the counter that counts metric series evicted by the registry cap.
///
/// Deliberately not a series in the registry itself: it must survive eviction of the series
/// that caused it, and it must not consume a slot of the budget it is reporting on.
pub const EVICTION_COUNTER_NAME: &str = "metrics_series_evicted_total";

/// What kind of aggregation a series performs.
///
/// The kind is fixed at the first update to a `(name, labels)` pair. A later update with a
/// different kind is rejected with [`ErrorKind::Conflict`] rather than silently converted:
/// a counter that starts receiving `set` calls, or a gauge that starts receiving `inc`
/// calls, is a bug in the caller, and quietly reinterpreting it would corrupt the dashboard
/// instead of surfacing the bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    /// A monotonically increasing total, e.g. requests completed, bytes written.
    Counter,
    /// A value that replaces its predecessor, e.g. queue depth, connection count.
    Gauge,
    /// A distribution of observations, e.g. request latency in milliseconds.
    Histogram,
}

impl MetricKind {
    /// The stable machine-readable name used in JSON output and metric help text.
    ///
    /// Part of the agent-facing surface: `ncui metrics --json` emits this, so changing one
    /// is a breaking change.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// An owned, ordered set of label key/value pairs identifying a series.
///
/// # Why a sorted set and not a `HashMap`
///
/// Series identity has to be a **total order**, not merely an equality, for three reasons:
/// the registry enumerates series to find the least recently updated one when evicting;
/// [`MetricsRegistry::snapshot`] must produce a stable ordering so that a diff of two
/// snapshots is meaningful; and tie-breaking an eviction must not depend on hash
/// randomisation. A `HashMap` gives none of those — its iteration order would vary with
/// the process's address space — so the pairs are stored sorted and the derived
/// [`PartialOrd`]/[`Ord`]/[`Hash`] implementations agree with [`PartialEq`].
///
/// Duplicate keys are rejected by [`Labels::insert`] rather than resolved by "last write
/// wins": two values for one key is a caller bug, and picking one silently would make the
/// registry's identity differ from the identity the caller believes they wrote to.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Labels {
    pairs: Vec<(String, String)>,
}

impl Labels {
    /// An empty label set — a series identified by its name alone.
    #[must_use]
    pub fn new() -> Self {
        Self { pairs: Vec::new() }
    }

    /// A label set built from an iterator of pairs, rejecting a duplicate key.
    ///
    /// This is the constructor callers should reach for: it is the fallible one, and the
    /// error names the offending key.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Validation`] naming the duplicated or empty key.
    pub fn from_pairs<I, K, V>(pairs: I) -> Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut labels = Self::new();
        for (key, value) in pairs {
            labels.insert(key, value)?;
        }
        Ok(labels)
    }

    /// Inserts a key/value pair, keeping the pairs sorted by key.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Validation`] if the key is already present, or if the key is
    /// empty — an empty label key is unaddressable in every text format and is always a
    /// mistake.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) -> Result<()> {
        let pair = (key.into(), value.into());
        if pair.0.is_empty() {
            return Err(NcError::new(ErrorKind::Validation {
                field: "label.key".to_owned(),
                reason: "a label key must not be empty".to_owned(),
            }));
        }
        // The search is by **key alone**. Searching by the whole `(key, value)` tuple would
        // treat `("a", "1")` and `("a", "2")` as distinct and insert a second pair for `a`,
        // leaving a set with two values for one key — the exact state this type's identity
        // rules exist to forbid. Comparing only the key makes `Ok` mean "this key is taken",
        // so the duplicate is caught whether or not the values happen to match.
        match self
            .pairs
            .binary_search_by(|(existing, _)| existing.cmp(&pair.0))
        {
            Ok(_) => Err(NcError::new(ErrorKind::Validation {
                field: "label.key".to_owned(),
                reason: format!("duplicate label key `{}`", pair.0),
            })),
            Err(index) => {
                self.pairs.insert(index, pair);
                Ok(())
            }
        }
    }

    /// The value for `key`, if present.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        let probe = key.to_owned();
        self.pairs
            .binary_search_by(|(existing, _)| existing.cmp(&probe))
            .ok()
            .map(|index| self.pairs[index].1.as_str())
    }

    /// Whether `key` is present.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// The number of pairs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// Whether the set carries no pairs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// The pairs, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.pairs
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }
}

impl<K, V> TryFrom<Vec<(K, V)>> for Labels
where
    K: Into<String>,
    V: Into<String>,
{
    type Error = NcError;

    fn try_from(pairs: Vec<(K, V)>) -> Result<Self> {
        Self::from_pairs(pairs)
    }
}

impl<K, V> FromIterator<(K, V)> for Labels
where
    K: Into<String>,
    V: Into<String>,
{
    /// Ignores a repeated key, keeping the value already stored — **first** write wins.
    ///
    /// `FromIterator` has no error channel, and [`Labels::insert`] rejects a duplicate key,
    /// so a repeat here is dropped and the first value stands. The asymmetry with
    /// [`Labels::from_pairs`], which rejects, is deliberate and is the reason to prefer
    /// `from_pairs` anywhere a duplicate is meaningful. This form exists for the ergonomic
    /// builder style in tests and fixtures, where the pairs are literal and a duplicate is
    /// a typo rather than a decision.
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut labels = Self::new();
        for (key, value) in iter {
            let _ = labels.insert(key, value);
        }
        labels
    }
}

impl Serialize for Labels {
    /// Serializes as a JSON object of key/value pairs.
    ///
    /// An object rather than an array of two-element arrays because that is the form a
    /// consumer of `ncui metrics --json` expects, and because the sorted order makes the
    /// rendering stable across runs.
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.pairs.len()))?;
        for (key, value) in &self.pairs {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Labels {
    /// Deserializes from a JSON object, rejecting a duplicate key.
    ///
    /// Fallible on the same condition as [`Labels::from_pairs`] for a concrete reason:
    /// `serde_json` keeps the last of two duplicate object keys silently, so inheriting
    /// the parser's behaviour would make a round trip lossy in a way nothing reports.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        use serde::de::{Error, MapAccess, Visitor};

        struct LabelsVisitor;

        impl<'de> Visitor<'de> for LabelsVisitor {
            type Value = Labels;

            fn expecting(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                formatter.write_str("a JSON object of string label pairs")
            }

            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> core::result::Result<Labels, M::Error> {
                let mut labels = Labels::new();
                while let Some((key, value)) = map.next_entry::<String, String>()? {
                    labels
                        .insert(key, value)
                        .map_err(|err| M::Error::custom(err.to_string()))?;
                }
                Ok(labels)
            }
        }

        deserializer.deserialize_map(LabelsVisitor)
    }
}

/// The stored state of a counter series.
#[derive(Debug, Clone, Default)]
struct CounterState {
    value: u64,
}

/// The stored state of a histogram series.
#[derive(Debug, Clone)]
struct HistogramState {
    /// Lifetime observations. Maintained incrementally, never windowed.
    count: u64,
    /// Lifetime sum of observations. Maintained incrementally, never windowed.
    sum: f64,
    /// The bounded reservoir of retained observations, oldest first.
    reservoir: Vec<f64>,
    /// The configured reservoir capacity.
    capacity: usize,
    /// Lifetime observations dropped from the reservoir by compaction.
    compacted: u64,
}

impl HistogramState {
    fn new(capacity: usize) -> Self {
        // Pre-allocating the full capacity would let one histogram reserve
        // `max_histogram_samples` up front; the reservoir is capped well below that in
        // practice, so the allocation starts small and grows. The cap is still enforced
        // exactly, it is just not pre-committed.
        Self {
            count: 0,
            sum: 0.0,
            reservoir: Vec::with_capacity(capacity.min(64)),
            capacity,
            compacted: 0,
        }
    }

    fn observe(&mut self, value: f64) {
        self.count = self.count.saturating_add(1);
        self.sum += value;
        if self.reservoir.len() >= self.capacity {
            // Compaction: drop the oldest retained sample to make room, so the window
            // slides forward. This is what makes the module's accuracy caveat precise —
            // the reservoir always holds the most recent `capacity` observations.
            self.reservoir.remove(0);
            self.compacted = self.compacted.saturating_add(1);
        }
        self.reservoir.push(value);
    }

    /// The statistics computed over the retained reservoir.
    fn stats(&self) -> HistogramStats {
        if self.reservoir.is_empty() {
            // Reachable only for a freshly created series, which cannot be read back
            // through the public API before its first observation. Reporting zeros rather
            // than `None` keeps the JSON shape uniform.
            return HistogramStats {
                count: 0,
                sum: 0.0,
                mean: 0.0,
                min: 0.0,
                max: 0.0,
                variance: 0.0,
                p50: 0.0,
                p90: 0.0,
                p95: 0.0,
                p99: 0.0,
                retained_samples: 0,
                compacted_samples: 0,
            };
        }

        let mut sorted = self.reservoir.clone();
        // `total_cmp` rather than `partial_cmp` so that a NaN observation cannot make the
        // sort panic. NaN is ordered above every other value, which shows up in the output
        // as a visible max rather than as a crash in the metrics path.
        sorted.sort_by(f64::total_cmp);
        let n = sorted.len();

        // Lifetime mean from the lifetime sum, so it stays correct after compaction.
        let mean = if self.count == 0 {
            0.0
        } else {
            self.sum / self.count as f64
        };

        // Variance is over the retained window, consistent with the percentiles. Mixing a
        // lifetime mean with a windowed spread would produce a number that is the
        // variance of nothing.
        let window_mean = sorted.iter().sum::<f64>() / n as f64;
        let sum_squared_deviations: f64 = sorted
            .iter()
            .map(|value| {
                let deviation = value - window_mean;
                deviation * deviation
            })
            .sum();

        HistogramStats {
            count: self.count,
            sum: self.sum,
            mean,
            min: sorted[0],
            max: sorted[n - 1],
            // Population variance (divide by N): the convention for a complete observed
            // set. Sample variance would understate the spread of the data actually held.
            variance: sum_squared_deviations / n as f64,
            p50: percentile_of_sorted(&sorted, 50.0),
            p90: percentile_of_sorted(&sorted, 90.0),
            p95: percentile_of_sorted(&sorted, 95.0),
            p99: percentile_of_sorted(&sorted, 99.0),
            retained_samples: n,
            compacted_samples: self.compacted,
        }
    }
}

/// Selects the `percentile`-th percentile from an already-sorted ascending slice, by
/// **nearest rank**.
///
/// ```text
/// rank  = clamp(ceil(percentile / 100 * n), 1, n)
/// value = sorted[rank - 1]
/// ```
///
/// For `1..=1000` this gives p50 = 500, p90 = 900, p95 = 950 and p99 = 990 — every reported
/// percentile is a value that was actually observed. Interpolating between neighbours
/// would give a p50 of 500.5, a number that was never measured and that implies a
/// resolution the data does not have.
///
/// A `percentile` outside `(0, 100]` is clamped rather than rejected: the only callers are
/// this module's own fixed quantiles, a clamp cannot produce a nonsensical result, and a
/// panic in the metrics read path would take down a dashboard query.
#[must_use]
pub(crate) fn percentile_of_sorted(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    let fraction = (percentile / 100.0).clamp(f64::MIN_POSITIVE, 1.0);
    let raw_rank = (fraction * n as f64).ceil();
    // `ceil` of a positive value is at least 1.0, so the conversion cannot produce zero;
    // it is clamped anyway so the index below is provably in range.
    let rank = (raw_rank as usize).clamp(1, n);
    sorted[rank - 1]
}

/// A point-in-time summary of one series, ready for `ncui metrics --json`.
///
/// Owned and serializable so it can be handed to the exporter and written without holding
/// the registry lock, and so a caller can keep a copy for diffing against a later snapshot
/// of a registry that has since moved on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SeriesSnapshot {
    /// A counter's current total.
    Counter {
        /// The metric name.
        name: String,
        /// The series label set.
        labels: Labels,
        /// The lifetime total.
        value: u64,
    },
    /// A gauge's current value.
    Gauge {
        /// The metric name.
        name: String,
        /// The series label set.
        labels: Labels,
        /// The value most recently set.
        value: f64,
    },
    /// A histogram's full distribution summary.
    Histogram {
        /// The metric name.
        name: String,
        /// The series label set.
        labels: Labels,
        /// Every statistic the histogram reports, including the percentiles.
        stats: Box<HistogramStats>,
    },
}

impl SeriesSnapshot {
    /// The metric name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Counter { name, .. }
            | Self::Gauge { name, .. }
            | Self::Histogram { name, .. } => name,
        }
    }

    /// The series label set.
    #[must_use]
    pub fn labels(&self) -> &Labels {
        match self {
            Self::Counter { labels, .. }
            | Self::Gauge { labels, .. }
            | Self::Histogram { labels, .. } => labels,
        }
    }
}

/// The distribution statistics reported for a histogram (spec 72).
///
/// `count` and `sum` are lifetime totals. `min`, `max`, `variance` and the percentiles are
/// computed over the retained reservoir — a window of the most recent
/// `Limits::max_histogram_samples` observations. `retained_samples` and
/// `compacted_samples` are reported so that a consumer can tell the two apart without
/// guessing: while `compacted_samples` is zero the distribution covers all of history and
/// is exact.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HistogramStats {
    /// Lifetime number of observations.
    pub count: u64,
    /// Lifetime sum of observations.
    pub sum: f64,
    /// Lifetime mean, `sum / count`. Exact regardless of compaction.
    pub mean: f64,
    /// Smallest value in the retained window.
    pub min: f64,
    /// Largest value in the retained window.
    pub max: f64,
    /// Population variance of the retained window.
    pub variance: f64,
    /// 50th percentile of the retained window, by nearest rank.
    pub p50: f64,
    /// 90th percentile of the retained window, by nearest rank.
    pub p90: f64,
    /// 95th percentile of the retained window, by nearest rank.
    pub p95: f64,
    /// 99th percentile of the retained window, by nearest rank.
    pub p99: f64,
    /// Number of observations currently retained in the reservoir.
    pub retained_samples: usize,
    /// Lifetime observations dropped from the reservoir by compaction.
    pub compacted_samples: u64,
}

/// One registered series: its kind, its value, and when it was last touched.
#[derive(Debug, Clone)]
struct SeriesEntry {
    kind: MetricKind,
    value: SeriesValue,
    /// The tick at which this series was last updated; selects the eviction victim.
    last_updated: u64,
}

/// A mutation to apply to a series, one variant per [`MetricKind`].
///
/// This exists instead of passing a closure into the write path because a closure would
/// have to match on the stored value's variant and therefore needed an arm for the
/// impossible case. That arm would be either a silently-ignored update — a metric that
/// quietly stops recording, the worst failure mode an observability component has — or a
/// panic on the path every metric write goes through. With the update carried as data, the
/// kind check and the mutation are one exhaustive match in one place, and anything
/// inconsistent is reported as a structured error rather than terminating the process.
#[derive(Debug, Clone, Copy)]
enum SeriesUpdate {
    /// Add to a counter's total.
    CounterInc(u64),
    /// Replace a gauge's level.
    GaugeSet(f64),
    /// Append an observation to a histogram's reservoir.
    HistogramObserve(f64),
}

impl SeriesUpdate {
    /// The kind of series this update applies to.
    const fn kind(self) -> MetricKind {
        match self {
            Self::CounterInc(_) => MetricKind::Counter,
            Self::GaugeSet(_) => MetricKind::Gauge,
            Self::HistogramObserve(_) => MetricKind::Histogram,
        }
    }
}

/// The stored value of a series, one variant per [`MetricKind`].
#[derive(Debug, Clone)]
enum SeriesValue {
    /// A monotonic total.
    Counter(CounterState),
    /// The most recently set level.
    Gauge(f64),
    /// A bounded distribution.
    Histogram(HistogramState),
}

impl SeriesValue {
    /// The initial value for a series of the given kind, so a newly created series has a
    /// well-defined value before its first update rather than an uninhabited variant.
    fn initial(kind: MetricKind, histogram_capacity: usize) -> Self {
        match kind {
            MetricKind::Counter => Self::Counter(CounterState::default()),
            MetricKind::Gauge => Self::Gauge(0.0),
            MetricKind::Histogram => Self::Histogram(HistogramState::new(histogram_capacity)),
        }
    }

    /// Applies `update` to this value.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Internal`] if the update's kind does not match the stored
    /// value's kind. `apply_update` rejects that combination as a [`ErrorKind::Conflict`]
    /// *before* reaching here, so landing on this arm means the two checks disagree — a
    /// defect in this crate, which is exactly how it is classified. It is reported rather
    /// than unwrapped: a bug in a write path must not abort the runtime it is observing.
    fn apply(&mut self, update: SeriesUpdate, series: &SeriesKey) -> Result<()> {
        match (self, update) {
            (Self::Counter(counter), SeriesUpdate::CounterInc(by)) => {
                counter.value = counter.value.saturating_add(by);
                Ok(())
            }
            (Self::Gauge(gauge), SeriesUpdate::GaugeSet(value)) => {
                *gauge = value;
                Ok(())
            }
            (Self::Histogram(histogram), SeriesUpdate::HistogramObserve(value)) => {
                histogram.observe(value);
                Ok(())
            }
            (other, update) => Err(NcError::new(ErrorKind::Internal {
                operation: "MetricsRegistry::apply_update".to_owned(),
                reason: format!(
                    "update of kind {} applied to `{}`{} holding {other:?}",
                    update.kind().code(),
                    series.name,
                    render_labels(&series.labels)
                ),
            })),
        }
    }
}

/// A series' identity: name plus label set.
///
/// The kind is deliberately excluded so a kind conflict is detected on update instead of
/// silently splitting one logical metric into two unrelated-looking series.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SeriesKey {
    name: String,
    labels: Labels,
}

impl SeriesKey {
    fn new(name: &str, labels: &Labels) -> Self {
        Self {
            name: name.to_owned(),
            labels: labels.clone(),
        }
    }
}

/// The registry's mutable state, all of it behind one lock.
#[derive(Debug, Default)]
struct RegistryState {
    series: HashMap<SeriesKey, SeriesEntry>,
    /// A monotonically increasing counter used as the recency clock.
    ///
    /// A tick rather than a wall clock: it is free, it cannot go backwards, and it makes
    /// eviction order a pure function of the operation sequence — so a test asserting
    /// *which* series was evicted is deterministic.
    tick: u64,
    /// Lifetime eviction count, held outside `series` so it survives the pressure it
    /// reports on and costs no slot of the cap.
    evictions: u64,
}

impl RegistryState {
    /// Advances and returns the recency tick.
    fn next_tick(&mut self) -> u64 {
        self.tick = self.tick.saturating_add(1);
        self.tick
    }
}

/// A bounded, thread-safe registry of metric series.
///
/// Shared behind an [`std::sync::Arc`] from every subsystem that reports metrics, which is
/// why every mutating method takes `&self` and takes the registry's own lock rather than
/// the caller's.
///
/// # Boundedness
///
/// At most `Limits::max_metric_series` series are retained, and each histogram retains at
/// most `Limits::max_histogram_samples` observations. Both are enforced on the write path
/// rather than checked afterwards, so a runaway label cannot exhaust memory even briefly.
#[derive(Debug)]
pub struct MetricsRegistry {
    state: Mutex<RegistryState>,
    limits: Limits,
}

/// A series' identity, in a form a caller can hold across many writes.
///
/// This exists because building a [`Labels`] allocates on every call, and the metrics write
/// path is one of the few genuinely hot paths in the runtime. A caller that reports from
/// inside a loop builds the key once and reuses it; the `*_inc`-style methods take the
/// pieces directly for the cold call sites that report once per operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SeriesRef {
    key: SeriesKey,
}

impl SeriesRef {
    /// Builds a key for `name` with no labels.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            key: SeriesKey::new(name, &Labels::new()),
        }
    }

    /// Returns a key carrying one additional label pair.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Validation`] if the key is empty or duplicated.
    pub fn with_label(mut self, key: &str, value: &str) -> Result<Self> {
        self.key.labels.insert(key, value)?;
        Ok(self)
    }

    /// The metric name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.key.name
    }

    /// The label set.
    #[must_use]
    pub fn labels(&self) -> &Labels {
        &self.key.labels
    }
}

impl MetricsRegistry {
    /// Creates an empty registry bounded by `limits`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Validation`] if `limits` is itself invalid. The registry
    /// enforces the limits it is given, so accepting an invalid configuration would mean
    /// enforcing a cap of zero and rejecting every update.
    pub fn new(limits: Limits) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            state: Mutex::new(RegistryState::default()),
            limits,
        })
    }

    /// The limits this registry enforces.
    #[must_use]
    pub const fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The maximum number of series retained before eviction begins.
    #[must_use]
    pub const fn series_capacity(&self) -> usize {
        self.limits.max_metric_series
    }

    /// The maximum number of observations retained per histogram.
    #[must_use]
    pub const fn histogram_capacity(&self) -> usize {
        self.limits.max_histogram_samples
    }

    /// Adds `by` to the counter at `name`/`labels`, creating it at zero if absent.
    ///
    /// Counters are monotonic by construction: there is no decrement and no setter, so a
    /// counter's value can only grow. The addition saturates at `u64::MAX` rather than
    /// wrapping, because a wrapped counter reports a count of negative events.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Conflict`] if the series already exists with a different kind.
    pub fn counter_inc(&self, name: &str, labels: &Labels, by: u64) -> Result<()> {
        self.apply_update(&SeriesKey::new(name, labels), SeriesUpdate::CounterInc(by))
    }

    /// Replaces the gauge at `name`/`labels`, creating it if absent.
    ///
    /// Unlike a counter this overwrites: a gauge reports a level, not an accumulation, so
    /// the last value written is the value that is true now.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Conflict`] if the series already exists with a different kind.
    pub fn gauge_set(&self, name: &str, labels: &Labels, value: f64) -> Result<()> {
        self.apply_update(&SeriesKey::new(name, labels), SeriesUpdate::GaugeSet(value))
    }

    /// Records an observation in the histogram at `name`/`labels`.
    ///
    /// The observation is appended to the bounded reservoir; when the reservoir is full the
    /// oldest retained sample is dropped so the window advances. Lifetime `count` and `sum`
    /// are updated regardless, so the lifetime mean stays exact across compaction.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Conflict`] if the series already exists with a different kind.
    pub fn histogram_observe(&self, name: &str, labels: &Labels, value: f64) -> Result<()> {
        self.apply_update(
            &SeriesKey::new(name, labels),
            SeriesUpdate::HistogramObserve(value),
        )
    }

    /// Adds `by` to the counter identified by a prebuilt key.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Conflict`] if the series already exists with a different kind.
    pub fn inc_series(&self, series: &SeriesRef, by: u64) -> Result<()> {
        self.apply_update(&series.key, SeriesUpdate::CounterInc(by))
    }

    /// The current value of the counter at `name`/`labels`, if it exists.
    ///
    /// Returns `None` for an absent series *and* for a series of a different kind, so a
    /// caller cannot read a gauge's `f64` as a counter's total by accident.
    #[must_use]
    pub fn counter_value(&self, name: &str, labels: &Labels) -> Option<u64> {
        self.read(&SeriesKey::new(name, labels), |value| match value {
            SeriesValue::Counter(counter) => Some(counter.value),
            _ => None,
        })
    }

    /// The current value of the gauge at `name`/`labels`, if it exists.
    #[must_use]
    pub fn gauge_value(&self, name: &str, labels: &Labels) -> Option<f64> {
        self.read(&SeriesKey::new(name, labels), |value| match value {
            SeriesValue::Gauge(gauge) => Some(*gauge),
            _ => None,
        })
    }

    /// The distribution summary of the histogram at `name`/`labels`, if it exists.
    #[must_use]
    pub fn histogram_stats(&self, name: &str, labels: &Labels) -> Option<HistogramStats> {
        self.read(&SeriesKey::new(name, labels), |value| match value {
            SeriesValue::Histogram(histogram) => Some(histogram.stats()),
            _ => None,
        })
    }

    /// How many series have been evicted since the registry was created.
    #[must_use]
    pub fn evicted_total(&self) -> u64 {
        self.lock().evictions
    }

    /// The number of series currently retained, not counting the eviction counter.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().series.len()
    }

    /// Whether the registry holds no series.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A snapshot of every retained series, in a deterministic order.
    ///
    /// Series are ordered by `(name, labels)` rather than by hash or recency, so two
    /// snapshots of the same state are byte-identical and a diff shows only real changes.
    /// The eviction counter is prepended when it is non-zero, so a consumer reading only
    /// the snapshot can still see that the cap has bound.
    #[must_use]
    pub fn snapshot(&self) -> Vec<SeriesSnapshot> {
        let state = self.lock();
        let mut entries: Vec<(&SeriesKey, &SeriesEntry)> = state.series.iter().collect();
        // `sort_by_key` rather than `sort_by`: the key is the borrowed `SeriesKey` itself,
        // and `sort_by` would spell out the same `Ord` call by hand.
        entries.sort_by_key(|(key, _)| *key);

        let mut snapshots = Vec::with_capacity(entries.len() + 1);
        if state.evictions > 0 {
            snapshots.push(SeriesSnapshot::Counter {
                name: EVICTION_COUNTER_NAME.to_owned(),
                labels: Labels::new(),
                value: state.evictions,
            });
        }
        for (key, entry) in entries {
            let snapshot = match &entry.value {
                SeriesValue::Counter(counter) => SeriesSnapshot::Counter {
                    name: key.name.clone(),
                    labels: key.labels.clone(),
                    value: counter.value,
                },
                SeriesValue::Gauge(gauge) => SeriesSnapshot::Gauge {
                    name: key.name.clone(),
                    labels: key.labels.clone(),
                    value: *gauge,
                },
                SeriesValue::Histogram(histogram) => SeriesSnapshot::Histogram {
                    name: key.name.clone(),
                    labels: key.labels.clone(),
                    stats: Box::new(histogram.stats()),
                },
            };
            snapshots.push(snapshot);
        }
        snapshots
    }

    /// Finds or creates a series and applies `update` to it.
    ///
    /// One function so that every write goes through the same sequence: validate the kind,
    /// evict if the cap binds, mutate, mark as recently updated. A write path that skipped
    /// any of those steps would produce a series that evicts the wrong victim or never
    /// becomes the eviction target.
    ///
    /// The create path and the update path are distinguished by an explicit `contains_key`
    /// check rather than by the `Entry` API. That is deliberate: the create path may need to
    /// evict, and eviction needs `&mut` on the whole map, which an `Entry` borrow would
    /// forbid for as long as it lived. Both steps happen under the same lock, so the
    /// two-step form cannot race.
    fn apply_update(&self, key: &SeriesKey, update: SeriesUpdate) -> Result<()> {
        let kind = update.kind();
        let mut state = self.lock();
        let tick = state.next_tick();

        if !state.series.contains_key(key) {
            let capacity = self.limits.max_metric_series;
            // Evict before inserting so that the resulting size never exceeds the cap, even
            // momentarily.
            if state.series.len() >= capacity {
                Self::evict_oldest(&mut state);
            }
            state.series.insert(
                key.clone(),
                SeriesEntry {
                    kind,
                    value: SeriesValue::initial(kind, self.limits.max_histogram_samples),
                    last_updated: tick,
                },
            );
        }

        // A missing entry here cannot happen while the lock is held: the series was either
        // found or inserted one statement ago. Reported as a structured internal failure
        // rather than unwrapped, because an observability component must never take down the
        // process it is observing — and a panic would destroy the very logs that would
        // explain why it happened.
        let Some(entry) = state.series.get_mut(key) else {
            return Err(NcError::new(ErrorKind::Internal {
                operation: "MetricsRegistry::apply_update".to_owned(),
                reason: format!(
                    "metric series `{}`{} disappeared between insertion and update",
                    key.name,
                    render_labels(&key.labels)
                ),
            }));
        };
        if entry.kind != kind {
            return Err(NcError::new(ErrorKind::Conflict {
                reason: format!(
                    "metric `{}`{} is already a {}; refusing to write it as a {}",
                    key.name,
                    render_labels(&key.labels),
                    entry.kind.code(),
                    kind.code()
                ),
            }));
        }
        entry.last_updated = tick;
        entry.value.apply(update, key)
    }

    /// Reads a series through `read`, returning `None` if it is absent or of another kind.
    fn read<T>(&self, key: &SeriesKey, read: impl FnOnce(&SeriesValue) -> Option<T>) -> Option<T> {
        let state = self.lock();
        state.series.get(key).and_then(|entry| read(&entry.value))
    }

    /// Evicts the least recently updated series and counts the eviction.
    ///
    /// Ties on `last_updated` cannot occur through the write path — every write takes a
    /// fresh tick — but the scan falls back to the series key so the choice stays
    /// deterministic if one ever did, rather than depending on `HashMap` iteration order.
    fn evict_oldest(state: &mut RegistryState) {
        let victim = state
            .series
            .iter()
            .min_by(|(left_key, left), (right_key, right)| {
                left.last_updated
                    .cmp(&right.last_updated)
                    .then_with(|| left_key.cmp(right_key))
            })
            .map(|(key, _)| key.clone());

        if let Some(key) = victim {
            state.series.remove(&key);
            state.evictions = state.evictions.saturating_add(1);
        }
    }

    /// Takes the registry lock, recovering from a poisoned lock.
    ///
    /// A poisoned lock means some other thread panicked while holding it, leaving the
    /// registry in whatever state that thread had reached. Every mutation restores the
    /// per-series invariants before it returns, so a partially-applied write is limited to
    /// one series' value. Recovering the guard is therefore strictly better than
    /// propagating the poison, which would turn one unrelated panic into permanent loss of
    /// observability for the rest of the process — and observability is precisely what you
    /// need when investigating the panic that caused it.
    fn lock(&self) -> MutexGuard<'_, RegistryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Renders a label set for an error message: `{}` when empty, `key="value", …` otherwise.
fn render_labels(labels: &Labels) -> String {
    if labels.is_empty() {
        return "{}".to_owned();
    }
    let rendered: Vec<String> = labels
        .iter()
        .map(|(key, value)| format!("{key}={value:?}"))
        .collect();
    format!("{{{}}}", rendered.join(","))
}

/// Converts a [`Duration`] to milliseconds for a histogram observation.
///
/// Milliseconds rather than raw [`Duration`] because a latency dashboard reads in
/// milliseconds, and doing the conversion here keeps the unit in one place instead of at
/// every call site where a seconds/milliseconds mix-up would be invisible.
#[must_use]
pub fn duration_millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[cfg(test)]
// Exact float equality is the assertion, not an oversight.
//
// The percentiles, min, max and mean under test are exact by construction: the reservoir
// holds values observed as small integers and nearest-rank selection returns one of them
// unchanged, so the expected value is a representable literal like `968.0` and equality is
// exact. An epsilon comparison would weaken these tests — it would accept a percentile
// computed by interpolation, by bucketing, or off by a whole rank, which are precisely the
// implementations this module exists to rule out (spec 72). Tests that genuinely involve
// accumulated floating-point sums (`histogram_variance_is_the_population_variance_of_the_window`)
// keep a tolerance, because there the arithmetic is genuinely inexact.
#[allow(
    clippy::float_cmp,
    reason = "exact percentile values are the assertion"
)]
mod tests {
    use super::*;
    use ncui_core::LimitsBuilder;
    use std::sync::Arc;

    fn limits(max_series: usize, max_samples: usize) -> Limits {
        LimitsBuilder::new()
            .max_metric_series(max_series)
            .max_histogram_samples(max_samples)
            .build()
            .expect("test limits must be valid")
    }

    fn registry(max_series: usize, max_samples: usize) -> MetricsRegistry {
        MetricsRegistry::new(limits(max_series, max_samples)).expect("registry constructs")
    }

    fn labels<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> Labels {
        Labels::from_pairs(pairs.iter().map(|(k, v)| (k.as_ref(), v.as_ref())))
            .expect("test label sets carry no duplicate keys")
    }

    fn tagged(tag: &str) -> Labels {
        labels(&[("s", tag)])
    }

    // ---- Labels -------------------------------------------------------------------------

    #[test]
    fn label_pairs_are_kept_in_key_order_regardless_of_insertion_order() {
        let forward = labels(&[("a", "1"), ("b", "2")]);
        let reversed = labels(&[("b", "2"), ("a", "1")]);
        assert_eq!(
            forward, reversed,
            "identity must not depend on insertion order"
        );
        assert_eq!(
            forward,
            reversed.iter().collect::<Labels>(),
            "collecting the same pairs must rebuild the same identity"
        );
    }

    #[test]
    fn a_duplicate_label_key_is_rejected_rather_than_overwritten() {
        let err = Labels::from_pairs([("a", "1"), ("a", "2")])
            .expect_err("a duplicate key must not be silently resolved");
        assert!(matches!(err.kind(), ErrorKind::Validation { .. }));
        assert!(err.to_string().contains("duplicate"), "got: {err}");
    }

    #[test]
    fn an_empty_label_key_is_rejected() {
        let err = Labels::from_pairs([("", "1")]).expect_err("an empty key is unaddressable");
        assert!(matches!(err.kind(), ErrorKind::Validation { .. }));
    }

    #[test]
    fn label_lookup_and_containment_report_membership() {
        let set = labels(&[("window", "main"), ("kind", "button")]);
        assert_eq!(set.get("window"), Some("main"));
        assert_eq!(set.get("missing"), None);
        assert!(set.contains("kind"));
        assert!(!set.contains("windowx"));
        assert_eq!(set.len(), 2);
        assert!(!set.is_empty());
    }

    #[test]
    fn an_empty_label_set_is_the_default_and_serializes_as_an_empty_object() {
        let set = Labels::default();
        assert_eq!(set, Labels::new());
        assert!(set.is_empty());
        assert_eq!(serde_json::to_string(&set).expect("serializable"), "{}");
    }

    #[test]
    fn labels_round_trip_through_json_in_sorted_order() {
        let set = labels(&[("b", "2"), ("a", "1")]);
        let json = serde_json::to_string(&set).expect("serializable");
        assert_eq!(json, r#"{"a":"1","b":"2"}"#);
        let back: Labels = serde_json::from_str(&json).expect("deserializable");
        assert_eq!(back, set);
    }

    #[test]
    fn deserializing_labels_rejects_a_duplicate_key() {
        // serde_json keeps the last duplicate silently, so the duplicate must be caught
        // here rather than inherited from the parser.
        let err = serde_json::from_str::<Labels>(r#"{"a":"1","a":"2"}"#)
            .expect_err("a duplicate key must not deserialize");
        assert!(err.to_string().contains("duplicate"), "got: {err}");
    }

    #[test]
    fn labels_collected_from_an_iterator_keep_the_first_of_a_duplicate() {
        // `FromIterator` cannot report an error, so it drops the repeat. The asymmetry with
        // `from_pairs` (which rejects) is the documented reason to prefer the fallible one.
        let set: Labels = [("a", "1"), ("a", "2")].into_iter().collect();
        assert_eq!(set.get("a"), Some("1"));
        assert_eq!(
            set.len(),
            1,
            "the repeat must not add a second pair for one key"
        );
    }

    #[test]
    fn labels_are_try_constructible_from_a_vec() {
        let set = Labels::try_from(vec![("a", "1"), ("b", "2")]).expect("valid pairs");
        assert_eq!(set, labels(&[("a", "1"), ("b", "2")]));
        assert!(Labels::try_from(vec![("a", "1"), ("a", "2")]).is_err());
    }

    // ---- Series identity ----------------------------------------------------------------

    #[test]
    fn two_different_label_sets_are_two_different_series() {
        let reg = registry(16, 128);
        reg.counter_inc("hits", &tagged("main"), 1)
            .expect("first series");
        reg.counter_inc("hits", &tagged("modal"), 1)
            .expect("second series");
        assert_eq!(reg.len(), 2, "identity is name plus label set");
        assert_eq!(reg.counter_value("hits", &tagged("main")), Some(1));
        assert_eq!(reg.counter_value("hits", &tagged("modal")), Some(1));
    }

    #[test]
    fn label_set_order_does_not_create_a_second_series() {
        let reg = registry(16, 128);
        reg.counter_inc("hits", &labels(&[("a", "1"), ("b", "2")]), 1)
            .expect("first write");
        reg.counter_inc("hits", &labels(&[("b", "2"), ("a", "1")]), 1)
            .expect("second write");
        assert_eq!(reg.len(), 1);
        assert_eq!(
            reg.counter_value("hits", &labels(&[("a", "1"), ("b", "2")])),
            Some(2)
        );
    }

    #[test]
    fn the_same_name_under_a_different_kind_is_a_conflict_not_a_second_series() {
        let reg = registry(16, 128);
        let set = Labels::new();
        reg.counter_inc("thing", &set, 5).expect("counter write");
        let err = reg
            .gauge_set("thing", &set, 1.0)
            .expect_err("a counter must not accept a gauge write");
        assert!(matches!(err.kind(), ErrorKind::Conflict { .. }));
        assert!(err.to_string().contains("counter"), "got: {err}");
        assert_eq!(reg.len(), 1, "a conflict must not create a second series");
        assert_eq!(reg.counter_value("thing", &set), Some(5));
    }

    #[test]
    fn a_conflict_names_both_kinds_and_the_series() {
        let reg = registry(16, 128);
        reg.histogram_observe("thing", &tagged("a"), 1.0)
            .expect("histogram");
        let err = reg
            .counter_inc("thing", &tagged("a"), 1)
            .expect_err("kind conflict");
        let text = err.to_string();
        assert!(text.contains("histogram"), "got: {text}");
        assert!(text.contains("counter"), "got: {text}");
        assert!(text.contains("thing"), "got: {text}");
        assert!(text.contains("s=\"a\""), "got: {text}");
    }

    #[test]
    fn an_absent_series_reads_as_none_rather_than_zero() {
        let reg = registry(16, 128);
        assert_eq!(reg.counter_value("missing", &Labels::new()), None);
        assert_eq!(reg.gauge_value("missing", &Labels::new()), None);
        assert_eq!(reg.histogram_stats("missing", &Labels::new()), None);
    }

    #[test]
    fn reading_a_series_through_the_wrong_kind_returns_none() {
        let reg = registry(16, 128);
        reg.counter_inc("c", &Labels::new(), 3).expect("counter");
        assert_eq!(reg.gauge_value("c", &Labels::new()), None);
        assert_eq!(reg.histogram_stats("c", &Labels::new()), None);
    }

    // ---- Counter ------------------------------------------------------------------------

    #[test]
    fn a_counter_exists_at_zero_once_first_written() {
        let reg = registry(16, 128);
        reg.counter_inc("fresh", &Labels::new(), 0)
            .expect("writing a counter creates it");
        assert_eq!(reg.counter_value("fresh", &Labels::new()), Some(0));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn a_counter_only_ever_increases() {
        let reg = registry(16, 128);
        let set = Labels::new();
        let mut previous = 0;
        for step in 1..=100u64 {
            reg.counter_inc("monotonic", &set, step).expect("write");
            let current = reg.counter_value("monotonic", &set).expect("series exists");
            assert!(
                current >= previous,
                "counter went backwards: {previous} then {current}"
            );
            previous = current;
        }
        // 1 + 2 + ... + 100
        assert_eq!(previous, 5050);
    }

    #[test]
    fn a_counter_saturates_instead_of_wrapping() {
        let reg = registry(16, 128);
        let set = Labels::new();
        reg.counter_inc("saturating", &set, u64::MAX)
            .expect("write");
        reg.counter_inc("saturating", &set, 100).expect("write");
        assert_eq!(
            reg.counter_value("saturating", &set),
            Some(u64::MAX),
            "a wrapped counter would report a count of negative events"
        );
    }

    #[test]
    fn a_prebuilt_series_key_reaches_the_same_series_as_the_pieces() {
        let reg = registry(16, 128);
        let series = SeriesRef::new("jobs")
            .with_label("worker", "3")
            .expect("key");
        reg.inc_series(&series, 4).expect("write");
        let same = labels(&[("worker", "3")]);
        assert_eq!(reg.counter_value("jobs", &same), Some(4));
        assert_eq!(series.name(), "jobs");
        assert_eq!(series.labels(), &same);
    }

    #[test]
    fn a_prebuilt_series_key_rejects_a_bad_label() {
        let err = SeriesRef::new("jobs")
            .with_label("", "3")
            .expect_err("an empty key is invalid");
        assert!(matches!(err.kind(), ErrorKind::Validation { .. }));
    }

    // ---- Gauge --------------------------------------------------------------------------

    #[test]
    fn a_gauge_overwrites_rather_than_accumulates() {
        let reg = registry(16, 128);
        let set = Labels::new();
        reg.gauge_set("queue_depth", &set, 10.0).expect("first");
        reg.gauge_set("queue_depth", &set, 3.0).expect("second");
        reg.gauge_set("queue_depth", &set, 42.0).expect("third");
        assert_eq!(reg.gauge_value("queue_depth", &set), Some(42.0));
        assert_eq!(reg.len(), 1, "repeated sets touch one series");
    }

    #[test]
    fn a_gauge_can_move_in_both_directions_including_negative() {
        let reg = registry(16, 128);
        let set = Labels::new();
        reg.gauge_set("delta", &set, 5.0).expect("first");
        reg.gauge_set("delta", &set, -5.0).expect("second");
        assert_eq!(reg.gauge_value("delta", &set), Some(-5.0));
    }

    // ---- Histogram ----------------------------------------------------------------------

    #[test]
    fn histogram_percentiles_match_a_known_dataset() {
        let reg = registry(16, 2048);
        let set = Labels::new();
        for value in 1..=1000u32 {
            reg.histogram_observe("latency_ms", &set, f64::from(value))
                .expect("observe");
        }
        let stats = reg
            .histogram_stats("latency_ms", &set)
            .expect("series exists");

        assert_eq!(stats.count, 1000);
        assert_eq!(stats.sum, 500_500.0);
        assert_eq!(stats.mean, 500.5);
        assert_eq!(stats.min, 1.0);
        assert_eq!(stats.max, 1000.0);
        assert_eq!(stats.p50, 500.0);
        assert_eq!(stats.p90, 900.0);
        assert_eq!(stats.p95, 950.0);
        assert_eq!(stats.p99, 990.0);
        assert_eq!(stats.retained_samples, 1000);
        assert_eq!(
            stats.compacted_samples, 0,
            "everything fits, nothing compacted"
        );
    }

    #[test]
    fn histogram_variance_is_the_population_variance_of_the_window() {
        let reg = registry(16, 256);
        let set = Labels::new();
        for value in 1..=100u32 {
            reg.histogram_observe("v", &set, f64::from(value))
                .expect("observe");
        }
        let stats = reg.histogram_stats("v", &set).expect("series exists");
        // Population variance of 1..=100 is (n^2 - 1) / 12 = 833.25.
        let expected = (100.0_f64.mul_add(100.0, -1.0)) / 12.0;
        assert!(
            (stats.variance - expected).abs() < 1e-9,
            "expected {expected}, got {}",
            stats.variance
        );
    }

    #[test]
    fn every_reported_percentile_is_an_observed_value() {
        let reg = registry(16, 256);
        let set = Labels::new();
        for value in 1..=100u32 {
            reg.histogram_observe("observed", &set, f64::from(value))
                .expect("observe");
        }
        let stats = reg
            .histogram_stats("observed", &set)
            .expect("series exists");
        for (label, percentile) in [
            ("p50", stats.p50),
            ("p90", stats.p90),
            ("p95", stats.p95),
            ("p99", stats.p99),
        ] {
            assert!(
                percentile.fract() == 0.0 && (1.0..=100.0).contains(&percentile),
                "{label} = {percentile} was not an observed integer value"
            );
        }
    }

    #[test]
    fn a_single_observation_reports_itself_for_every_statistic() {
        let reg = registry(16, 128);
        let set = Labels::new();
        reg.histogram_observe("one", &set, 7.5).expect("observe");
        let stats = reg.histogram_stats("one", &set).expect("series exists");
        assert_eq!(stats.count, 1);
        assert_eq!(stats.sum, 7.5);
        assert_eq!(stats.mean, 7.5);
        assert_eq!(stats.min, 7.5);
        assert_eq!(stats.max, 7.5);
        assert_eq!(stats.variance, 0.0);
        assert_eq!(stats.p50, 7.5);
        assert_eq!(stats.p99, 7.5);
        assert_eq!(stats.retained_samples, 1);
    }

    #[test]
    fn observation_order_does_not_change_the_distribution() {
        let forward = registry(16, 256);
        let reverse = registry(16, 256);
        let set = Labels::new();
        for value in 1..=50u32 {
            forward
                .histogram_observe("latency_ms", &set, f64::from(value))
                .expect("observe");
        }
        for value in (1..=50u32).rev() {
            reverse
                .histogram_observe("latency_ms", &set, f64::from(value))
                .expect("observe");
        }
        assert_eq!(
            forward.histogram_stats("latency_ms", &set),
            reverse.histogram_stats("latency_ms", &set)
        );
    }

    #[test]
    fn the_reservoir_is_bounded_and_oldest_samples_are_compacted_out() {
        // A capacity far below the observation count proves the window slides rather than
        // growing: memory stays bounded and the percentiles describe recent values.
        let reg = registry(16, 64);
        let set = Labels::new();
        for value in 1..=1000u32 {
            reg.histogram_observe("latency_ms", &set, f64::from(value))
                .expect("observe");
        }
        let stats = reg
            .histogram_stats("latency_ms", &set)
            .expect("series exists");
        assert_eq!(
            stats.retained_samples, 64,
            "the reservoir must not exceed its bound"
        );
        assert_eq!(stats.compacted_samples, 936);
        // The window holds the most recent 64 values, 937..=1000, so the percentiles are
        // the tail's: p50 is 968, p99 is 991, max is 1000. Values 1..=936 are gone.
        assert_eq!(stats.min, 937.0);
        assert_eq!(stats.max, 1000.0);
        // Nearest rank over the 64 retained values: rank = ceil(q/100 * 64).
        assert_eq!(stats.p50, 968.0);
        assert_eq!(stats.p90, 994.0);
        assert_eq!(stats.p95, 997.0);
        // ceil(0.99 * 64) = 64, so p99 is the largest retained value.
        assert_eq!(stats.p99, 1000.0);
    }

    #[test]
    fn lifetime_count_and_sum_survive_compaction_so_the_mean_stays_exact() {
        let reg = registry(16, 64);
        let set = Labels::new();
        for value in 1..=1000u32 {
            reg.histogram_observe("latency_ms", &set, f64::from(value))
                .expect("observe");
        }
        let stats = reg
            .histogram_stats("latency_ms", &set)
            .expect("series exists");
        assert_eq!(stats.count, 1000, "lifetime count must not be windowed");
        assert_eq!(stats.sum, 500_500.0, "lifetime sum must not be windowed");
        assert_eq!(
            stats.mean, 500.5,
            "mean is lifetime sum over lifetime count"
        );
    }

    #[test]
    fn a_slow_burst_is_forgotten_once_the_window_slides_past_it() {
        // The documented accuracy caveat, demonstrated rather than merely asserted: p99
        // reports the burst while it is inside the window and stops reporting it once
        // more than `capacity` fast observations have arrived.
        let reg = registry(16, 64);
        let set = Labels::new();
        for _ in 0..60 {
            reg.histogram_observe("latency_ms", &set, 1.0)
                .expect("observe fast");
        }
        for _ in 0..4 {
            reg.histogram_observe("latency_ms", &set, 10_000.0)
                .expect("observe slow");
        }
        let with_burst = reg
            .histogram_stats("latency_ms", &set)
            .expect("series exists");
        assert_eq!(with_burst.p99, 10_000.0, "the burst is inside the window");
        assert_eq!(with_burst.max, 10_000.0);
        assert_eq!(with_burst.compacted_samples, 0);

        // 64 more fast observations, which is exactly the reservoir width: the window now
        // holds only fast values and the burst has been compacted out entirely.
        for _ in 0..64 {
            reg.histogram_observe("latency_ms", &set, 1.0)
                .expect("observe fast");
        }
        let after = reg
            .histogram_stats("latency_ms", &set)
            .expect("series exists");
        assert_eq!(after.count, 128, "lifetime count keeps every observation");
        assert_eq!(after.compacted_samples, 64);
        assert_eq!(after.p99, 1.0, "the window has slid past the burst");
        assert_eq!(after.max, 1.0);
        // Lifetime sum and mean still account for the burst exactly, which is the whole
        // reason count and sum are not windowed alongside the percentiles.
        assert_eq!(after.sum, 60.0 + 40_000.0 + 64.0);
        assert!((after.mean - (after.sum / 128.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn a_nan_observation_does_not_break_the_sort_or_the_min() {
        let reg = registry(16, 128);
        let set = Labels::new();
        reg.histogram_observe("odd", &set, 1.0).expect("observe");
        reg.histogram_observe("odd", &set, f64::NAN)
            .expect("observe");
        reg.histogram_observe("odd", &set, 2.0).expect("observe");
        let stats = reg.histogram_stats("odd", &set).expect("series exists");
        assert_eq!(stats.count, 3);
        assert_eq!(stats.min, 1.0);
        // `total_cmp` orders NaN above every other value, so it surfaces as the maximum
        // rather than as a panic in the metrics read path.
        assert!(stats.max.is_nan());
    }

    #[test]
    fn two_histograms_keep_independent_reservoirs() {
        let reg = registry(16, 64);
        let fast = tagged("fast");
        let slow = tagged("slow");
        for value in 1..=100u32 {
            reg.histogram_observe("latency_ms", &fast, f64::from(value))
                .expect("observe fast");
        }
        for value in 1000..=1099u32 {
            reg.histogram_observe("latency_ms", &slow, f64::from(value))
                .expect("observe slow");
        }
        let fast_stats = reg
            .histogram_stats("latency_ms", &fast)
            .expect("fast series");
        let slow_stats = reg
            .histogram_stats("latency_ms", &slow)
            .expect("slow series");
        // Each series keeps its own window: `fast` retains 37..=100 and `slow` retains
        // 1036..=1099, so neither histogram sees the other's observations at all.
        assert_eq!(fast_stats.max, 100.0);
        assert_eq!(fast_stats.min, 37.0);
        assert_eq!(slow_stats.min, 1036.0);
        assert_eq!(slow_stats.max, 1099.0);
        assert_eq!(fast_stats.count, 100, "lifetime count is not windowed");
        assert_eq!(slow_stats.count, 100);
    }

    // ---- Eviction -----------------------------------------------------------------------

    #[test]
    fn exceeding_the_series_cap_evicts_the_least_recently_updated_series() {
        let reg = registry(3, 128);
        let (a, b, c, d) = (tagged("a"), tagged("b"), tagged("c"), tagged("d"));

        reg.counter_inc("m", &a, 1).expect("a");
        reg.counter_inc("m", &b, 1).expect("b");
        reg.counter_inc("m", &c, 1).expect("c");
        assert_eq!(reg.len(), 3);
        assert_eq!(reg.evicted_total(), 0);

        // Touch `a` so `b` becomes the least recently updated, then insert a fourth series.
        reg.counter_inc("m", &a, 1).expect("a again");
        reg.counter_inc("m", &d, 1).expect("d triggers eviction");

        assert_eq!(reg.len(), 3, "the cap must hold after eviction");
        assert_eq!(
            reg.counter_value("m", &b),
            None,
            "the least recently updated series must be the victim"
        );
        assert_eq!(
            reg.counter_value("m", &a),
            Some(2),
            "a was touched and must survive despite being older"
        );
        assert_eq!(reg.counter_value("m", &c), Some(1));
        assert_eq!(reg.counter_value("m", &d), Some(1));
    }

    #[test]
    fn eviction_increments_the_eviction_counter_so_it_is_observable() {
        let reg = registry(2, 128);
        for index in 0..5 {
            reg.counter_inc("m", &tagged(&index.to_string()), 1)
                .expect("write");
        }
        // Five distinct series through a cap of two means three evictions.
        assert_eq!(reg.evicted_total(), 3);
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn the_eviction_counter_is_itself_present_in_a_snapshot() {
        let reg = registry(1, 128);
        reg.counter_inc("m", &tagged("a"), 1).expect("first");
        reg.counter_inc("m", &tagged("b"), 1)
            .expect("second evicts the first");
        let entry = reg
            .snapshot()
            .into_iter()
            .find(|snapshot| snapshot.name() == EVICTION_COUNTER_NAME)
            .expect("the eviction counter must appear in the snapshot");
        match entry {
            SeriesSnapshot::Counter { value, .. } => assert_eq!(value, 1),
            other => panic!("the eviction counter must be a counter, got {other:?}"),
        }
    }

    #[test]
    fn the_eviction_counter_is_absent_while_nothing_has_been_evicted() {
        let reg = registry(4, 128);
        reg.counter_inc("m", &Labels::new(), 1).expect("write");
        assert!(
            reg.snapshot()
                .iter()
                .all(|snapshot| snapshot.name() != EVICTION_COUNTER_NAME),
            "a counter reporting zero evictions would be noise in every snapshot"
        );
    }

    #[test]
    fn the_eviction_counter_costs_no_slot_of_the_cap_it_reports_on() {
        // The counter lives outside the series map precisely so a full registry can still
        // say it is full. With a cap of one, one user series plus the counter must both be
        // reported.
        let reg = registry(1, 128);
        for index in 0..3 {
            reg.counter_inc("m", &tagged(&index.to_string()), 1)
                .expect("write");
        }
        assert_eq!(reg.len(), 1, "only user series count against the cap");
        assert_eq!(reg.evicted_total(), 2);
        assert_eq!(reg.snapshot().len(), 2, "one user series plus the counter");
    }

    #[test]
    fn a_series_the_caller_rewrites_keeps_its_value_across_an_eviction_pressure_run() {
        let reg = registry(2, 128);
        let pinned = tagged("pinned");
        reg.counter_inc("m", &pinned, 100).expect("pin");
        for index in 0..20 {
            reg.counter_inc("m", &tagged(&index.to_string()), 1)
                .expect("churn");
            reg.counter_inc("m", &pinned, 0).expect("keep pinned warm");
        }
        assert_eq!(
            reg.counter_value("m", &pinned),
            Some(100),
            "a series touched on every pass must never be the victim"
        );
    }

    #[test]
    fn updating_an_existing_series_never_triggers_an_eviction() {
        let reg = registry(2, 128);
        let (a, b) = (tagged("a"), tagged("b"));
        reg.counter_inc("m", &a, 1).expect("a");
        reg.counter_inc("m", &b, 1).expect("b");
        for _ in 0..50 {
            reg.counter_inc("m", &a, 1).expect("a again");
            reg.counter_inc("m", &b, 1).expect("b again");
        }
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.evicted_total(), 0, "an update is not a new series");
    }

    #[test]
    fn a_cap_of_one_keeps_exactly_the_most_recent_series() {
        let reg = registry(1, 128);
        for index in 0..10 {
            reg.counter_inc("m", &tagged(&index.to_string()), 1)
                .expect("write");
        }
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.counter_value("m", &tagged("9")), Some(1));
        assert_eq!(reg.evicted_total(), 9);
    }

    // ---- Snapshot -----------------------------------------------------------------------

    #[test]
    fn a_snapshot_is_ordered_by_name_then_labels() {
        let reg = registry(16, 128);
        reg.gauge_set("zeta", &Labels::new(), 1.0).expect("z");
        reg.gauge_set("alpha", &tagged("2"), 1.0).expect("a2");
        reg.gauge_set("alpha", &tagged("1"), 1.0).expect("a1");
        let rendered: Vec<String> = reg
            .snapshot()
            .iter()
            .map(|snapshot| {
                let pairs: Vec<String> = snapshot
                    .labels()
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect();
                format!("{}/{}", snapshot.name(), pairs.join(","))
            })
            .collect();
        assert_eq!(rendered, vec!["alpha/s=1", "alpha/s=2", "zeta/"]);
    }

    #[test]
    fn a_snapshot_reports_each_kind_with_its_own_shape() {
        let reg = registry(16, 256);
        reg.counter_inc("c", &Labels::new(), 7).expect("counter");
        reg.gauge_set("g", &Labels::new(), 1.5).expect("gauge");
        for value in 1..=10u32 {
            reg.histogram_observe("h", &Labels::new(), f64::from(value))
                .expect("observe");
        }
        let snapshot = reg.snapshot();
        assert_eq!(snapshot.len(), 3);

        let counter = snapshot
            .iter()
            .find(|entry| entry.name() == "c")
            .expect("counter present");
        let value: serde_json::Value = serde_json::to_value(counter).expect("serializes");
        assert_eq!(value["kind"], "counter");
        assert_eq!(value["value"], 7);
        assert!(value.get("stats").is_none(), "a counter carries no stats");

        let gauge = snapshot
            .iter()
            .find(|entry| entry.name() == "g")
            .expect("gauge present");
        let value: serde_json::Value = serde_json::to_value(gauge).expect("serializes");
        assert_eq!(value["kind"], "gauge");
        assert_eq!(value["value"], 1.5);

        let histogram = snapshot
            .iter()
            .find(|entry| entry.name() == "h")
            .expect("histogram present");
        let value: serde_json::Value = serde_json::to_value(histogram).expect("serializes");
        assert_eq!(value["kind"], "histogram");
        assert_eq!(value["stats"]["count"], 10);
        assert_eq!(value["stats"]["p50"], 5.0);
        assert_eq!(value["stats"]["p99"], 10.0);
        assert_eq!(value["stats"]["max"], 10.0);
        assert_eq!(value["stats"]["retained_samples"], 10);
    }

    #[test]
    fn a_snapshot_round_trips_through_json() {
        let reg = registry(16, 256);
        reg.counter_inc("c", &tagged("v"), 7).expect("counter");
        for value in 1..=10u32 {
            reg.histogram_observe("h", &Labels::new(), f64::from(value))
                .expect("observe");
        }
        let snapshot = reg.snapshot();
        let json = serde_json::to_string(&snapshot).expect("serializes");
        let back: Vec<SeriesSnapshot> = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, snapshot);
    }

    #[test]
    fn an_empty_registry_snapshots_to_an_empty_list() {
        assert!(registry(4, 128).snapshot().is_empty());
    }

    // ---- Construction and concurrency ----------------------------------------------------

    #[test]
    fn a_registry_rejects_invalid_limits_rather_than_enforcing_zero() {
        let bad = Limits {
            max_metric_series: 0,
            ..Limits::default()
        };
        let err = MetricsRegistry::new(bad).expect_err("a zero series cap is invalid");
        assert!(matches!(err.kind(), ErrorKind::Validation { .. }));
    }

    #[test]
    fn a_registry_reports_the_limits_it_enforces() {
        let reg = registry(5, 128);
        assert_eq!(reg.series_capacity(), 5);
        assert_eq!(reg.histogram_capacity(), 128);
        assert_eq!(reg.limits().max_metric_series, 5);
        assert!(reg.is_empty());
    }

    #[test]
    fn a_registry_shared_behind_an_arc_accumulates_across_threads() {
        // The registry is written from every subsystem at once; concurrent writes must be
        // serialized by the registry's own lock rather than lost.
        let reg = Arc::new(registry(64, 256));
        let threads = 8_u32;
        let per_thread = 250_u32;
        let handles: Vec<_> = (0..threads)
            .map(|thread| {
                let reg = Arc::clone(&reg);
                let set = tagged(&thread.to_string());
                std::thread::spawn(move || {
                    for _ in 0..per_thread {
                        reg.counter_inc("jobs", &set, 1).expect("write");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("worker thread must not panic");
        }

        let total: u64 = (0..threads)
            .filter_map(|thread| reg.counter_value("jobs", &tagged(&thread.to_string())))
            .sum();
        assert_eq!(
            total,
            u64::from(threads * per_thread),
            "no increment may be lost"
        );
        assert_eq!(reg.len(), threads as usize);
    }

    #[test]
    fn concurrent_histogram_observations_all_count() {
        let reg = Arc::new(registry(16, 256));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let reg = Arc::clone(&reg);
                std::thread::spawn(move || {
                    for value in 1..=100u32 {
                        reg.histogram_observe("latency_ms", &Labels::new(), f64::from(value))
                            .expect("observe");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("worker thread must not panic");
        }
        let stats = reg
            .histogram_stats("latency_ms", &Labels::new())
            .expect("series exists");
        assert_eq!(
            stats.count, 400,
            "lifetime count must include every observation"
        );
        // Four threads each contributing 1..=100, whose sum is 5050.
        assert_eq!(stats.sum, 4.0 * 5050.0);
        assert_eq!(stats.max, 100.0);
    }

    #[test]
    fn duration_millis_converts_exactly() {
        assert_eq!(duration_millis(Duration::from_millis(250)), 250.0);
        assert_eq!(duration_millis(Duration::from_secs(2)), 2000.0);
        assert_eq!(duration_millis(Duration::ZERO), 0.0);
    }

    #[test]
    fn the_public_metric_types_stay_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MetricsRegistry>();
        assert_send_sync::<SeriesRef>();
        assert_send_sync::<Labels>();
        assert_send_sync::<HistogramStats>();
        assert_send_sync::<SeriesSnapshot>();
        assert_send_sync::<MetricKind>();
    }

    #[test]
    fn metric_kinds_have_stable_distinct_codes() {
        let codes = [
            MetricKind::Counter.code(),
            MetricKind::Gauge.code(),
            MetricKind::Histogram.code(),
        ];
        assert_eq!(codes, ["counter", "gauge", "histogram"]);
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "kind codes must be unique");
    }

    #[test]
    fn the_nearest_rank_helper_clamps_rather_than_panicking() {
        let sorted = [1.0, 2.0, 3.0];
        assert_eq!(
            percentile_of_sorted(&sorted, 0.0),
            1.0,
            "clamped up to rank 1"
        );
        assert_eq!(
            percentile_of_sorted(&sorted, 500.0),
            3.0,
            "clamped down to the maximum"
        );
        assert_eq!(percentile_of_sorted(&[], 50.0), 0.0);
        assert_eq!(percentile_of_sorted(&[42.0], 99.0), 42.0);
    }
}
