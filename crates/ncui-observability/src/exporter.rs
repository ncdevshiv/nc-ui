//! JSON Lines export of collected records and metrics.
//!
//! # Why JSON Lines
//!
//! One JSON object per line, newline-terminated, is the format every log pipeline in this
//! space already speaks: `jq` reads it, a shipper appends to it, a rotation policy is
//! "split on newlines", and a truncated final line costs exactly one record rather than the
//! whole file. A JSON array would make the writer hold every record in memory to close the
//! bracket, and would make a partial write unrecoverable — which for a log destination is
//! the difference between losing the tail and losing everything.
//!
//! # Why the sink is a caller-supplied `Write`
//!
//! Every method here takes `&mut impl Write`. That is what makes the exporter testable
//! without a filesystem, and it is also the right production shape: the runtime binary
//! owns the destination — a file, a socket, an in-memory buffer for a test — and this crate
//! owns only the encoding. Nothing here opens a path or resolves an environment variable,
//! so there is no configuration to get wrong and no file handle to leak.
//!
//! # What is written
//!
//! [`JsonLinesExporter::write_events`] writes an interleaved stream of log records and metric snapshots, each
//! tagged by `"type"` so that a reader can tell them apart without guessing from the field
//! names. [`JsonLinesExporter::write_logs`] and [`JsonLinesExporter::write_metrics`] write a homogeneous batch of either. All
//! three produce the same per-line shape for their kind, so a consumer can handle a mixed
//! file and a single-kind file with one parser.

use std::io::Write;

use ncui_core::Result;
use serde::Serialize;
use serde::Serializer;

use crate::logring::LogRecord;
use crate::metrics::SeriesSnapshot;

/// One exportable item.
///
/// Tagged by `"type"` so a mixed stream is unambiguous. A consumer switching on one field
/// is far more robust than a consumer inferring the kind from which fields happen to be
/// present, which is the only alternative and which breaks the moment a new field is added
/// to one of the shapes.
///
/// `PartialEq` is absent because [`LogRecord`] has none: the error it carries compares by
/// failure class rather than by content, so a derived equality here would claim two events
/// are interchangeable when their messages differ.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExportEvent<'a> {
    /// A log record.
    Log(&'a LogRecord),
    /// A metric series snapshot.
    Metric(&'a SeriesSnapshot),
}

/// Writes collected telemetry as JSON Lines.
///
/// A unit struct with associated functions: it holds no state, and the sink is a parameter.
/// That is deliberate — an exporter that owned a writer would have to be constructed per
/// destination, would need interior mutability to be shared, and would make the "where does
/// this go" decision at construction time instead of at the call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JsonLinesExporter;

impl JsonLinesExporter {
    /// Writes one event as a single line.
    ///
    /// # Errors
    ///
    /// Propagates a serialization failure (which for these types means a value that cannot
    /// be represented in JSON) or a write failure from `sink`, as
    /// [`ErrorKind::Codec`](ncui_core::ErrorKind::Codec) and
    /// [`ErrorKind::Io`](ncui_core::ErrorKind::Io) respectively.
    pub fn write_event<W: Write + ?Sized>(sink: &mut W, event: &ExportEvent<'_>) -> Result<()> {
        let line = serde_json::to_string(event).map_err(|err| codec_error("serialize", &err))?;
        sink.write_all(line.as_bytes())
            .and_then(|()| sink.write_all(b"\n"))
            .map_err(|err| io_error("write json line", &err))
    }

    /// Writes every event as its own line, in the order given.
    ///
    /// # Errors
    ///
    /// As [`JsonLinesExporter::write_event`]. A failure stops the write: a partially written
    /// export that a caller believes completed is worse than one that reports the failure,
    /// because the destination may already hold a torn final line. The bytes written before
    /// the failure are not rolled back — a `Write` sink cannot be unwound — so a caller that
    /// needs all-or-nothing should render to a buffer first.
    pub fn write_events<W: Write + ?Sized>(sink: &mut W, events: &[ExportEvent<'_>]) -> Result<()> {
        for event in events {
            Self::write_event(sink, event)?;
        }
        Ok(())
    }

    /// Writes every log record as its own line.
    ///
    /// # Errors
    ///
    /// As [`JsonLinesExporter::write_event`].
    pub fn write_logs<W: Write + ?Sized>(sink: &mut W, records: &[LogRecord]) -> Result<()> {
        for record in records {
            let line = serde_json::to_string(&ExportEvent::Log(record))
                .map_err(|err| codec_error("serialize log record", &err))?;
            sink.write_all(line.as_bytes())
                .and_then(|()| sink.write_all(b"\n"))
                .map_err(|err| io_error("write log record", &err))?;
        }
        Ok(())
    }

    /// Writes every metric snapshot as its own line.
    ///
    /// # Errors
    ///
    /// As [`JsonLinesExporter::write_event`].
    pub fn write_metrics<W: Write + ?Sized>(
        sink: &mut W,
        snapshots: &[SeriesSnapshot],
    ) -> Result<()> {
        for snapshot in snapshots {
            let line = serde_json::to_string(&ExportEvent::Metric(snapshot))
                .map_err(|err| codec_error("serialize metric snapshot", &err))?;
            sink.write_all(line.as_bytes())
                .and_then(|()| sink.write_all(b"\n"))
                .map_err(|err| io_error("write metric snapshot", &err))?;
        }
        Ok(())
    }
}

/// Builds a codec error for a serialization failure.
fn codec_error(operation: &str, err: &serde_json::Error) -> ncui_core::NcError {
    ncui_core::NcError::new(ncui_core::ErrorKind::Codec {
        operation: format!("exporter::{operation}"),
        reason: err.to_string(),
    })
}

/// Builds an I/O error carrying the operation that failed.
fn io_error(operation: &str, err: &std::io::Error) -> ncui_core::NcError {
    ncui_core::NcError::new(ncui_core::ErrorKind::Io {
        operation: format!("exporter::{operation}"),
        io_kind: err.kind(),
        message: err.to_string(),
    })
}

/// Serializes a series snapshot with its labels flattened into a single object.
///
/// Used by the `ncui metrics --json` path where the consumer expects
/// `{"name":…,"kind":…,"value":…}` rather than a nested `labels` object. Kept here rather
/// than as a method on [`SeriesSnapshot`] so that the crate's own canonical shape — the
/// tagged enum — stays what it is and this rendering stays a presentation concern.
#[derive(Debug, Clone)]
pub struct FlatSnapshot<'a>(pub &'a SeriesSnapshot);

impl Serialize for FlatSnapshot<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        match self.0 {
            SeriesSnapshot::Counter {
                name,
                labels,
                value,
            } => {
                map.serialize_entry("name", name)?;
                map.serialize_entry("kind", "counter")?;
                map.serialize_entry("value", value)?;
                map.serialize_entry("labels", labels)?;
            }
            SeriesSnapshot::Gauge {
                name,
                labels,
                value,
            } => {
                map.serialize_entry("name", name)?;
                map.serialize_entry("kind", "gauge")?;
                map.serialize_entry("value", value)?;
                map.serialize_entry("labels", labels)?;
            }
            SeriesSnapshot::Histogram {
                name,
                labels,
                stats,
            } => {
                map.serialize_entry("name", name)?;
                map.serialize_entry("kind", "histogram")?;
                map.serialize_entry("count", &stats.count)?;
                map.serialize_entry("sum", &stats.sum)?;
                map.serialize_entry("mean", &stats.mean)?;
                map.serialize_entry("min", &stats.min)?;
                map.serialize_entry("max", &stats.max)?;
                map.serialize_entry("variance", &stats.variance)?;
                map.serialize_entry("p50", &stats.p50)?;
                map.serialize_entry("p90", &stats.p90)?;
                map.serialize_entry("p95", &stats.p95)?;
                map.serialize_entry("p99", &stats.p99)?;
                map.serialize_entry("retained_samples", &stats.retained_samples)?;
                map.serialize_entry("compacted_samples", &stats.compacted_samples)?;
                map.serialize_entry("labels", labels)?;
            }
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logring::{LogLevel, LogRecord};
    use crate::metrics::{HistogramStats, Labels, SeriesSnapshot};
    use ncui_core::{CorrelationId, ErrorKind, NcError};

    /// A sink that records what it was given and can be made to fail on demand.
    #[derive(Debug, Default)]
    struct TestSink {
        bytes: Vec<u8>,
        /// When set, the `write` that sees a buffer at least this long fails.
        fail_after: Option<usize>,
        /// The number of successful `write` calls before a failure is armed.
        writes_before_failure: usize,
    }

    impl TestSink {
        fn failing(fail_after: usize) -> Self {
            Self {
                fail_after: Some(fail_after),
                ..Self::default()
            }
        }

        fn text(&self) -> String {
            String::from_utf8(self.bytes.clone()).expect("json output is utf-8")
        }

        /// The rendered output split into lines.
        ///
        /// Owned rather than borrowed from `self`: the caller would otherwise have to hold
        /// the intermediate `String` alive, and every call site here wants the lines on
        /// their own.
        fn lines(&self) -> Vec<String> {
            self.text().lines().map(str::to_owned).collect()
        }
    }

    impl Write for TestSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes_before_failure += 1;
            if let Some(limit) = self.fail_after
                && self.writes_before_failure > limit
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "sink closed",
                ));
            }
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn log_record() -> LogRecord {
        LogRecord::new(
            1_700_000_000_000,
            LogLevel::Error,
            "ipc",
            "dispatch",
            "request timed out",
        )
        .with_correlation(CorrelationId::from_raw(0xABCD).expect("non-zero id"))
        .with_error(
            &NcError::new(ErrorKind::Timeout { elapsed_ms: 30 })
                .operation("ipc::await_response")
                .correlated(CorrelationId::from_raw(0xABCD).expect("non-zero id")),
        )
    }

    fn counter_snapshot() -> SeriesSnapshot {
        SeriesSnapshot::Counter {
            name: "ipc_requests_total".to_owned(),
            labels: Labels::from_pairs([("endpoint", "render")]).expect("unique key"),
            value: 42,
        }
    }

    fn gauge_snapshot() -> SeriesSnapshot {
        SeriesSnapshot::Gauge {
            name: "worker_queue_depth".to_owned(),
            labels: Labels::new(),
            value: 7.5,
        }
    }

    fn histogram_snapshot() -> SeriesSnapshot {
        SeriesSnapshot::Histogram {
            name: "ipc_dispatch_latency_ms".to_owned(),
            labels: Labels::new(),
            stats: Box::new(HistogramStats {
                count: 1000,
                sum: 500_500.0,
                mean: 500.5,
                min: 1.0,
                max: 1000.0,
                variance: 83_333.25,
                p50: 500.0,
                p90: 900.0,
                p95: 950.0,
                p99: 990.0,
                retained_samples: 1000,
                compacted_samples: 0,
            }),
        }
    }

    // ---- Log export ---------------------------------------------------------------------

    #[test]
    fn a_log_record_is_written_as_one_json_object_on_one_line() {
        let mut sink = TestSink::default();
        JsonLinesExporter::write_logs(&mut sink, &[log_record()]).expect("write succeeds");
        let text = sink.text();
        assert!(
            text.ends_with('\n'),
            "every line must be newline-terminated"
        );
        assert_eq!(text.lines().count(), 1, "got: {text:?}");

        let value: serde_json::Value = serde_json::from_str(text.lines().next().expect("line"))
            .expect("the line must be valid json");
        assert_eq!(value["type"], "log");
        assert_eq!(value["level"], "error");
        assert_eq!(value["subsystem"], "ipc");
        assert_eq!(value["operation"], "dispatch");
        assert_eq!(value["message"], "request timed out");
        assert_eq!(value["timestamp_ms"], 1_700_000_000_000u64);
        assert_eq!(value["error"]["code"], "timeout");
        assert_eq!(value["error"]["operation"], "ipc::await_response");
        assert_eq!(value["correlation"], "0000000000000000000000000000abcd");
    }

    #[test]
    fn several_log_records_become_several_lines_in_order() {
        let records: Vec<LogRecord> = (1..=3u64)
            .map(|index| {
                LogRecord::new(
                    index,
                    LogLevel::Info,
                    "ipc",
                    "dispatch",
                    format!("m{index}"),
                )
            })
            .collect();
        let mut sink = TestSink::default();
        JsonLinesExporter::write_logs(&mut sink, &records).expect("write succeeds");
        let lines = sink.lines();
        assert_eq!(lines.len(), 3);
        for (index, line) in lines.iter().enumerate() {
            let value: serde_json::Value = serde_json::from_str(line).expect("valid json");
            assert_eq!(value["timestamp_ms"], (index + 1) as u64);
        }
    }

    #[test]
    fn writing_no_records_writes_nothing() {
        let mut sink = TestSink::default();
        JsonLinesExporter::write_logs(&mut sink, &[]).expect("write succeeds");
        assert!(
            sink.text().is_empty(),
            "an empty batch must not emit a blank line"
        );
    }

    // ---- Metric export ------------------------------------------------------------------

    #[test]
    fn a_counter_snapshot_carries_its_name_labels_and_value() {
        let mut sink = TestSink::default();
        JsonLinesExporter::write_metrics(&mut sink, &[counter_snapshot()]).expect("write");
        let value: serde_json::Value = serde_json::from_str(&sink.lines()[0]).expect("valid json");
        assert_eq!(value["type"], "metric");
        assert_eq!(value["name"], "ipc_requests_total");
        assert_eq!(value["kind"], "counter");
        assert_eq!(value["value"], 42);
        assert_eq!(value["labels"]["endpoint"], "render");
    }

    #[test]
    fn a_gauge_snapshot_carries_its_value() {
        let mut sink = TestSink::default();
        JsonLinesExporter::write_metrics(&mut sink, &[gauge_snapshot()]).expect("write");
        let value: serde_json::Value = serde_json::from_str(&sink.lines()[0]).expect("valid json");
        assert_eq!(value["type"], "metric");
        assert_eq!(value["kind"], "gauge");
        assert_eq!(value["value"], 7.5);
        assert_eq!(value["labels"], serde_json::json!({}));
    }

    #[test]
    fn a_histogram_snapshot_carries_every_statistic_including_the_percentiles() {
        let mut sink = TestSink::default();
        JsonLinesExporter::write_metrics(&mut sink, &[histogram_snapshot()]).expect("write");
        let value: serde_json::Value = serde_json::from_str(&sink.lines()[0]).expect("valid json");
        assert_eq!(value["type"], "metric");
        assert_eq!(value["kind"], "histogram");
        assert_eq!(value["name"], "ipc_dispatch_latency_ms");
        // The statistics live under `stats` because `SeriesSnapshot` is an internally
        // tagged enum: the kind tag and the identifying fields are siblings of the stats
        // object, not of its fields. Asserting the nesting is part of asserting the shape.
        let stats = &value["stats"];
        for (field, expected) in [
            ("count", 1000),
            ("retained_samples", 1000),
            ("compacted_samples", 0),
        ] {
            assert_eq!(stats[field], expected, "field {field} must be exported");
        }
        for (field, expected) in [
            ("min", 1.0),
            ("max", 1000.0),
            ("p50", 500.0),
            ("p90", 900.0),
            ("p95", 950.0),
            ("p99", 990.0),
            ("sum", 500_500.0),
            ("mean", 500.5),
            ("variance", 83_333.25),
        ] {
            assert_eq!(stats[field], expected, "field {field} must be exported");
        }
    }

    #[test]
    fn a_metric_without_labels_exports_an_empty_label_object() {
        let mut sink = TestSink::default();
        JsonLinesExporter::write_metrics(&mut sink, &[gauge_snapshot()]).expect("write");
        let value: serde_json::Value = serde_json::from_str(&sink.lines()[0]).expect("valid json");
        assert_eq!(
            value["labels"],
            serde_json::json!({}),
            "an absent label set must still be present so consumers need not special-case it"
        );
    }

    // ---- Mixed streams ------------------------------------------------------------------

    #[test]
    fn a_mixed_stream_is_unambiguous_because_every_line_is_tagged() {
        let record = log_record();
        let metric = counter_snapshot();
        let events = [
            ExportEvent::Log(&record),
            ExportEvent::Metric(&metric),
            ExportEvent::Log(&record),
        ];
        let mut sink = TestSink::default();
        JsonLinesExporter::write_events(&mut sink, &events).expect("write succeeds");

        let lines = sink.lines();
        assert_eq!(lines.len(), 3);
        let kinds: Vec<String> = lines
            .iter()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).expect("valid json");
                value["type"].as_str().expect("a type tag").to_owned()
            })
            .collect();
        assert_eq!(kinds, vec!["log", "metric", "log"]);
    }

    #[test]
    fn a_registry_snapshot_round_trips_through_the_exporter_into_the_same_values() {
        // The end-to-end claim `ncui metrics --json` makes: what the registry reports is what
        // the exporter writes, with nothing dropped in between.
        use crate::metrics::MetricsRegistry;
        use ncui_core::LimitsBuilder;

        let limits = LimitsBuilder::new()
            .max_metric_series(16)
            .max_histogram_samples(2048)
            .build()
            .expect("valid limits");
        let registry = MetricsRegistry::new(limits).expect("registry");
        registry
            .counter_inc("ipc_requests_total", &Labels::new(), 42)
            .expect("counter");
        registry
            .gauge_set("worker_queue_depth", &Labels::new(), 7.5)
            .expect("gauge");
        for value in 1..=1000u32 {
            registry
                .histogram_observe("ipc_dispatch_latency_ms", &Labels::new(), f64::from(value))
                .expect("observe");
        }

        let snapshots = registry.snapshot();
        let mut sink = TestSink::default();
        JsonLinesExporter::write_metrics(&mut sink, &snapshots).expect("write succeeds");
        assert_eq!(sink.lines().len(), 3);

        let mut parsed = Vec::new();
        for line in &sink.lines() {
            parsed.push(serde_json::from_str::<serde_json::Value>(line).expect("valid json"));
        }
        let counter = parsed
            .iter()
            .find(|value| value["name"] == "ipc_requests_total")
            .expect("counter exported");
        assert_eq!(counter["value"], 42);
        let histogram = parsed
            .iter()
            .find(|value| value["name"] == "ipc_dispatch_latency_ms")
            .expect("histogram exported");
        assert_eq!(histogram["stats"]["p50"], 500.0);
        assert_eq!(histogram["stats"]["p99"], 990.0);
        assert_eq!(histogram["stats"]["max"], 1000.0);
    }

    #[test]
    fn a_log_ring_query_result_round_trips_through_the_exporter() {
        use crate::logring::LogRing;
        use ncui_core::LimitsBuilder;

        let limits = LimitsBuilder::new()
            .log_ring_capacity(8)
            .build()
            .expect("valid limits");
        let ring = LogRing::new(limits).expect("ring");
        ring.push(log_record());

        let records = ring.query(&crate::logring::LogQuery::with_limit(10));
        let mut sink = TestSink::default();
        JsonLinesExporter::write_logs(&mut sink, &records).expect("write succeeds");
        assert_eq!(sink.lines().len(), 1);
        let value: serde_json::Value = serde_json::from_str(&sink.lines()[0]).expect("valid json");
        assert_eq!(value["subsystem"], "ipc");
    }

    // ---- The flat rendering ---------------------------------------------------------------

    #[test]
    fn the_flat_rendering_puts_the_percentiles_at_the_top_level() {
        let json = serde_json::to_string(&FlatSnapshot(&histogram_snapshot())).expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(value["kind"], "histogram");
        assert_eq!(value["p99"], 990.0);
        assert!(value.get("stats").is_none(), "flat means flat");
    }

    #[test]
    fn the_flat_rendering_covers_every_kind() {
        for snapshot in [counter_snapshot(), gauge_snapshot()] {
            let json = serde_json::to_string(&FlatSnapshot(&snapshot)).expect("serializes");
            let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
            assert!(value.get("name").is_some());
            assert!(value.get("kind").is_some());
            assert!(value.get("value").is_some());
            assert!(value.get("labels").is_some());
        }
    }

    // ---- Failure reporting ----------------------------------------------------------------

    #[test]
    fn a_failing_sink_reports_an_io_error_naming_the_operation() {
        let mut sink = TestSink::failing(0);
        let err = JsonLinesExporter::write_logs(&mut sink, &[log_record()])
            .expect_err("a closed sink must be reported");
        match err.kind() {
            ErrorKind::Io {
                operation, io_kind, ..
            } => {
                assert_eq!(operation, "exporter::write log record");
                assert_eq!(*io_kind, std::io::ErrorKind::BrokenPipe);
            }
            other => panic!("expected an I/O error, got {other:?}"),
        }
    }

    #[test]
    fn a_failing_sink_during_metric_export_names_the_metric_operation() {
        let mut sink = TestSink::failing(0);
        let err = JsonLinesExporter::write_metrics(&mut sink, &[counter_snapshot()])
            .expect_err("a closed sink must be reported");
        assert!(
            err.to_string().contains("exporter::write metric snapshot"),
            "got: {err}"
        );
        assert!(err.is_retryable(), "an I/O failure is retryable by policy");
    }

    #[test]
    fn a_failure_midway_stops_the_write_instead_of_emitting_a_partial_line() {
        // Armed to fail on the second write, so the first record is complete and the second
        // never starts. A half-written final line is what a naive writer would leave.
        let mut sink = TestSink::failing(1);
        let records = vec![
            LogRecord::new(1, LogLevel::Info, "ipc", "op", "first"),
            LogRecord::new(2, LogLevel::Info, "ipc", "op", "second"),
        ];
        assert!(
            JsonLinesExporter::write_logs(&mut sink, &records).is_err(),
            "the failure must propagate"
        );
        let text = sink.text();
        assert!(text.contains("first"), "the complete record must remain");
        assert!(
            !text.contains("second"),
            "the failed record must not be partially written: {text:?}"
        );
    }

    #[test]
    fn every_written_line_is_valid_json_when_the_batch_is_large() {
        // The property that makes JSON Lines worth choosing: a reader can process the file
        // one line at a time without waiting for the end.
        let records: Vec<LogRecord> = (0..200u64)
            .map(|index| {
                LogRecord::new(
                    index,
                    LogLevel::Info,
                    "ipc",
                    "op",
                    format!("message {index}"),
                )
            })
            .collect();
        let mut sink = TestSink::default();
        JsonLinesExporter::write_logs(&mut sink, &records).expect("write succeeds");

        let lines = sink.lines();
        assert_eq!(lines.len(), 200);
        for (index, line) in lines.iter().enumerate() {
            let value: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("line {index} is not valid json: {err}"));
            assert_eq!(value["timestamp_ms"], index as u64);
        }
    }

    #[test]
    fn a_message_containing_a_newline_still_produces_one_line() {
        // JSON encoding escapes the newline, so a multi-line message cannot forge a
        // record boundary in a line-oriented format.
        let record = LogRecord::new(
            1,
            LogLevel::Error,
            "ipc",
            "op",
            "line one\nline two\n{\"type\":\"log\"}",
        );
        let mut sink = TestSink::default();
        JsonLinesExporter::write_logs(&mut sink, &[record]).expect("write succeeds");
        assert_eq!(sink.lines().len(), 1, "got: {:?}", sink.text());
        let value: serde_json::Value = serde_json::from_str(&sink.lines()[0]).expect("valid json");
        assert_eq!(value["message"], "line one\nline two\n{\"type\":\"log\"}");
    }
}
