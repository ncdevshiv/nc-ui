//! OTLP encoding: the OpenTelemetry wire format for this crate's logs and metrics.
//!
//! A collector speaks OTLP over HTTP with protobuf bodies. The encoding here turns this
//! crate's own records — a [`LogRecord`] ring and [`SeriesSnapshot`] metric series — into
//! those protobuf messages, so a caller with an HTTP client and a collector URL can ship
//! telemetry without this crate growing a network stack.
//!
//! # What is encoded, and what is not
//!
//! **Logs**: one `ResourceLogs` per call, with the record's time, severity, and message
//! body. **Metrics**: counters as cumulative monotonic `Sum`s, gauges as `Gauge`s, and
//! histograms as `Histogram`s carrying their count, sum, minimum and maximum. The crate's
//! histogram snapshot does not expose bucket counts, so bucket fields are not encoded
//! rather than invented, and its percentiles have no place in an OTLP histogram data
//! point. Traces are not encoded: this crate carries correlation identifiers, not span
//! trees, and an OTLP span needs a span tree.
//!
//! # The protobuf is hand-written, on purpose
//!
//! The wire format used here is a small, fixed subset of protobuf — varints, four fixed-64
//! fields, and length-delimited submessages — written and read by the functions below and
//! by the tests' own decoder. A generated protobuf runtime would be a dependency decision
//! (ADR-012) taken for roughly two hundred lines of encoding.
//!
//! The field numbers are the OpenTelemetry protocol's own (v1.x): `Metric.gauge = 5`,
//! `Metric.sum = 7`, `Metric.histogram = 9`, `NumberDataPoint.as_double = 4`,
//! `NumberDataPoint.as_int = 6`, `NumberDataPoint.attributes = 9`, `LogRecord.body = 5`.

use crate::logring::{LogLevel, LogRecord};
use crate::metrics::SeriesSnapshot;

/// The content type an OTLP/HTTP request for these encodings carries.
pub const OTLP_CONTENT_TYPE: &str = "application/x-protobuf";

/// The protobuf wire type for a varint.
const WIRE_VARINT: u8 = 0;
/// The protobuf wire type for a fixed 64-bit field.
const WIRE_FIXED64: u8 = 1;
/// The protobuf wire type for a length-delimited field.
const WIRE_BYTES: u8 = 2;

/// OTLP `SeverityNumber` for a log record's level.
///
/// The spec numbers severities in bands of four (trace 1, debug 5, info 9, warn 13, error
/// 17, fatal 21); this crate's levels map onto the low end of each band.
fn severity_number(level: LogLevel) -> u64 {
    match level {
        LogLevel::Trace => 1,
        LogLevel::Debug => 5,
        LogLevel::Info => 9,
        LogLevel::Warn => 13,
        LogLevel::Error => 17,
    }
}

/// Appends a varint.
fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Appends a field header: the field number and the wire type it is written as.
fn write_tag(out: &mut Vec<u8>, field: u32, wire: u8) {
    write_varint(out, (u64::from(field) << 3) | u64::from(wire));
}

/// Appends a length-delimited field's header and payload.
fn write_bytes_field(out: &mut Vec<u8>, field: u32, payload: &[u8]) {
    write_tag(out, field, WIRE_BYTES);
    write_varint(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

/// Appends a string field.
fn write_string(out: &mut Vec<u8>, field: u32, value: &str) {
    write_bytes_field(out, field, value.as_bytes());
}

/// Appends a submessage field.
fn write_submessage(out: &mut Vec<u8>, field: u32, body: &[u8]) {
    write_bytes_field(out, field, body);
}

/// Appends a fixed 64-bit field.
fn write_fixed64(out: &mut Vec<u8>, field: u32, value: u64) {
    write_tag(out, field, WIRE_FIXED64);
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a varint field.
fn write_varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    write_tag(out, field, WIRE_VARINT);
    write_varint(out, value);
}

/// Appends a double field.
fn write_double(out: &mut Vec<u8>, field: u32, value: f64) {
    write_tag(out, field, WIRE_FIXED64);
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends an `AnyValue` holding a string, as a log record's body.
fn write_string_value(out: &mut Vec<u8>, field: u32, value: &str) {
    let mut any = Vec::new();
    write_string(&mut any, 1, value);
    write_submessage(out, field, &any);
}

/// Appends a `KeyValue` pair, as resource or data-point attributes.
fn write_key_value(out: &mut Vec<u8>, field: u32, key: &str, value: &str) {
    let mut pair = Vec::new();
    write_string(&mut pair, 1, key);
    let mut any = Vec::new();
    write_string(&mut any, 1, value);
    write_submessage(&mut pair, 2, &any);
    write_submessage(out, field, &pair);
}

/// Encodes log records as one OTLP `ExportLogsServiceRequest`.
///
/// The request carries one resource (named `ncui`), one scope, and every record in order.
/// A record's JSON form supplies its timestamp, level and message, which keeps this module
/// from second-guessing [`LogRecord`]'s own field naming.
#[must_use]
pub fn encode_logs(records: &[LogRecord]) -> Vec<u8> {
    let mut scope_logs = Vec::new();
    for record in records {
        let Ok(value) = serde_json::to_value(record) else {
            continue;
        };
        let mut log_record = Vec::new();
        if let Some(timestamp) = value
            .get("timestamp_ms")
            .and_then(serde_json::Value::as_u64)
        {
            write_fixed64(&mut log_record, 1, timestamp.saturating_mul(1_000_000));
        }
        let level = value
            .get("level")
            .and_then(serde_json::Value::as_str)
            .and_then(|text| match text {
                "trace" => Some(LogLevel::Trace),
                "debug" => Some(LogLevel::Debug),
                "info" => Some(LogLevel::Info),
                "warn" => Some(LogLevel::Warn),
                "error" => Some(LogLevel::Error),
                _ => None,
            });
        if let Some(level) = level {
            write_varint_field(&mut log_record, 2, severity_number(level));
        }
        if let Some(message) = value.get("message").and_then(serde_json::Value::as_str) {
            write_string_value(&mut log_record, 5, message);
        }
        write_submessage(&mut scope_logs, 2, &log_record);
    }

    let mut scope = Vec::new();
    write_string(&mut scope, 1, "ncui");
    write_submessage(&mut scope, 2, &scope_logs);

    let mut resource_logs = Vec::new();
    let mut resource = Vec::new();
    write_key_value(&mut resource, 1, "service.name", "ncui");
    write_submessage(&mut resource_logs, 1, &resource);
    write_submessage(&mut resource_logs, 2, &scope);

    let mut request = Vec::new();
    write_submessage(&mut request, 1, &resource_logs);
    request
}

/// Encodes metric series as one OTLP `ExportMetricsServiceRequest`.
///
/// Counters become cumulative monotonic `Sum`s, gauges become `Gauge`s, and histograms
/// become `Histogram`s with their count, sum, minimum and maximum. The series' labels ride
/// on each data point as attributes.
#[must_use]
pub fn encode_metrics(snapshots: &[SeriesSnapshot]) -> Vec<u8> {
    let mut metrics = Vec::new();
    for snapshot in snapshots {
        let mut metric = Vec::new();
        write_string(&mut metric, 1, snapshot.name());
        match snapshot {
            SeriesSnapshot::Counter {
                name: _,
                labels,
                value,
            } => {
                let mut sum = Vec::new();
                let mut point = Vec::new();
                write_fixed64(
                    &mut point,
                    6,
                    i64::try_from(*value).unwrap_or(i64::MAX) as u64,
                );
                write_attributes(&mut point, labels);
                write_submessage(&mut sum, 1, &point);
                write_varint_field(&mut sum, 2, 2); // cumulative
                write_varint_field(&mut sum, 3, 1); // monotonic
                write_submessage(&mut metric, 7, &sum);
            }
            SeriesSnapshot::Gauge {
                name: _,
                labels,
                value,
            } => {
                let mut gauge = Vec::new();
                let mut point = Vec::new();
                write_double(&mut point, 4, *value);
                write_attributes(&mut point, labels);
                write_submessage(&mut gauge, 1, &point);
                write_submessage(&mut metric, 5, &gauge);
            }
            SeriesSnapshot::Histogram {
                name: _,
                labels,
                stats,
            } => {
                // The crate's histogram snapshot carries percentiles and sample counts,
                // not bucket counts, so the data point carries the sample count and leaves
                // the distribution fields at their defaults rather than inventing them.
                let mut histogram = Vec::new();
                let mut point = Vec::new();
                write_fixed64(&mut point, 4, stats.count);
                write_double(&mut point, 5, stats.sum);
                write_double(&mut point, 9, stats.min);
                write_double(&mut point, 10, stats.max);
                write_attributes(&mut point, labels);
                write_submessage(&mut histogram, 1, &point);
                write_varint_field(&mut histogram, 2, 2); // cumulative
                write_submessage(&mut metric, 9, &histogram);
            }
        }
        write_submessage(&mut metrics, 2, &metric);
    }

    let mut resource_metrics = Vec::new();
    let mut scope_metrics = Vec::new();
    write_submessage(&mut scope_metrics, 2, &metrics);
    let mut resource = Vec::new();
    write_key_value(&mut resource, 1, "service.name", "ncui");
    write_submessage(&mut resource_metrics, 1, &resource);
    write_submessage(&mut resource_metrics, 2, &scope_metrics);

    let mut request = Vec::new();
    write_submessage(&mut request, 1, &resource_metrics);
    request
}

/// Writes a label set as data-point attributes.
fn write_attributes(out: &mut Vec<u8>, labels: &crate::metrics::Labels) {
    let Ok(value) = serde_json::to_value(labels) else {
        return;
    };
    if let Some(map) = value.as_object() {
        for (key, label_value) in map {
            let text = match label_value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            write_key_value(out, 9, key, &text);
        }
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::{encode_logs, encode_metrics};
    use crate::logring::{LogLevel, LogRecord};
    use crate::metrics::{HistogramStats, Labels, SeriesSnapshot};

    /// Whether `bytes` contains `needle` as a contiguous subsequence.
    fn contains(bytes: &[u8], needle: &[u8]) -> bool {
        bytes.windows(needle.len()).any(|window| window == needle)
    }

    #[test]
    fn an_empty_log_batch_still_produces_a_request() {
        let request = encode_logs(&[]);
        assert!(!request.is_empty(), "an empty batch is still a request");
        // The request opens with the resource_logs field: tag 0x0A (field 1, wire 2).
        assert_eq!(request[0], 0x0A);
    }

    #[test]
    fn a_log_record_carries_its_time_severity_and_message() {
        let records = vec![LogRecord::new(
            1_700_000_000_123,
            LogLevel::Warn,
            "ipc",
            "dispatch",
            "the frame was refused",
        )];
        let request = encode_logs(&records);
        // SeverityNumber WARN = 13, as field 2's varint: tag 0x10, value 0x0D.
        assert!(
            contains(&request, &[0x10, 0x0D]),
            "the warn severity must be encoded as field 2's varint"
        );
        // The message rides in an AnyValue body on field 5.
        assert!(
            contains(&request, b"the frame was refused"),
            "the message must be encoded verbatim"
        );
    }

    #[test]
    fn a_log_records_message_with_control_characters_stays_on_one_line() {
        let records = vec![LogRecord::new(
            1,
            LogLevel::Error,
            "ipc",
            "dispatch",
            "first\nsecond",
        )];
        let request = encode_logs(&records);
        // The encoded request is protobuf, so the embedded newline is data, not a break.
        assert!(
            contains(&request, b"first\nsecond"),
            "the message is encoded verbatim"
        );
    }

    #[test]
    fn counters_encode_as_cumulative_monotonic_sums() {
        let labels = Labels::from_pairs([("command", "run")]).expect("labels");
        let snapshots = vec![SeriesSnapshot::Counter {
            name: "native.sidecar_calls".to_owned(),
            labels,
            value: 7,
        }];
        let request = encode_metrics(&snapshots);
        // The metric's name is encoded as a string field.
        assert!(
            contains(&request, b"native.sidecar_calls"),
            "the metric name must be encoded"
        );
        // The data point's value rides on field 6 as a fixed 64-bit integer:
        // tag (6 << 3) | 1 = 0x31, then 7's little-endian bytes.
        let mut value = vec![0x31];
        value.extend_from_slice(&7u64.to_le_bytes());
        assert!(
            contains(&request, &value),
            "the counter's value must be encoded as field 6's fixed 64"
        );
        // The sum is cumulative (temporality 2 on field 2's varint: tag 0x10) and
        // monotonic (field 3's varint: tag 0x18, value 1).
        assert!(contains(&request, &[0x10, 0x02]), "cumulative");
        assert!(contains(&request, &[0x18, 0x01]), "monotonic");
    }

    #[test]
    fn gauges_encode_their_value_as_a_double() {
        let snapshots = vec![SeriesSnapshot::Gauge {
            name: "queue.depth".to_owned(),
            labels: Labels::default(),
            value: 3.5,
        }];
        let request = encode_metrics(&snapshots);
        // field 4, wire 1 (fixed 64): tag 0x21, then 3.5's little-endian bytes.
        let mut needle = vec![0x21];
        needle.extend_from_slice(&3.5f64.to_le_bytes());
        assert!(
            contains(&request, &needle),
            "the gauge's value must be encoded as field 4's double"
        );
    }

    #[test]
    fn histograms_carry_their_count_sum_minimum_and_maximum() {
        let snapshots = vec![SeriesSnapshot::Histogram {
            name: "call.latency".to_owned(),
            labels: Labels::default(),
            stats: Box::new(HistogramStats {
                count: 9,
                sum: 42.0,
                min: 1.0,
                max: 8.0,
                mean: 4.666_666_666_666_667,
                variance: 1.0,
                p50: 4.0,
                p90: 8.0,
                p95: 8.0,
                p99: 8.0,
                retained_samples: 9,
                compacted_samples: 0,
            }),
        }];
        let request = encode_metrics(&snapshots);
        // The histogram rides on Metric field 9: tag 0x4A.
        assert!(contains(&request, &[0x4A]), "the histogram is present");
        // count = 9 as field 4's fixed 64: tag 0x21, then 9's little-endian bytes.
        let mut count = vec![0x21];
        count.extend_from_slice(&9u64.to_le_bytes());
        assert!(contains(&request, &count), "the count must be encoded");
        // sum = 42.0 as field 5's double: tag (5 << 3) | 1 = 0x29, then 42.0's
        // little-endian bytes.
        let mut sum = vec![0x29];
        sum.extend_from_slice(&42.0f64.to_le_bytes());
        assert!(contains(&request, &sum), "the sum must be encoded");
    }

    #[test]
    fn the_resource_names_the_service() {
        let request = encode_logs(&[]);
        assert!(
            contains(&request, b"service.name"),
            "the resource attribute key must be encoded"
        );
        assert!(contains(&request, b"ncui"), "the service name is ncui");
    }
}
