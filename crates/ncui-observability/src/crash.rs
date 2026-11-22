//! Crash reports: the structured record the runtime leaves behind when it panics.
//!
//! ADR-001 requires that a crash is "detected, reported, and recovered from". The
//! detection and recovery half lives with the window host (`ncui-webview`'s lifecycle,
//! which holds the engine's state); this module is the reporting half: the record a
//! panic handler serialises, so a crash on a machine nobody was watching is still
//! diagnosable the next morning.
//!
//! # What a report carries, and why every field is bounded
//!
//! A report names the runtime version, the wall-clock moment, the process uptime, the
//! thread, the panic's message and source location, and a captured backtrace. Every
//! free-text field is bounded: a panic message is text somebody else's code formatted,
//! and a report without a bound is a report a hostile or merely broken dependency can
//! fill with a gigabyte. The bounds are public constants so the writer of the report —
//! and the reader of the file — can size what they are prepared for.
//!
//! # Determinism and the clock
//!
//! Like the rest of this crate, building a report reads no clock: the caller supplies
//! the wall-clock nanoseconds and the uptime it measured. The panic hook that calls
//! [`CrashReport::capture`] with the real clock lives with the binary, which is also
//! where the crash directory is chosen — this crate formats, it does not decide where
//! files go.
//!
//! # What this module deliberately does not do
//!
//! It does not install the hook, choose a directory, or touch the filesystem: the sink
//! is a parameter, exactly as in [`crate::exporter`]. A report is one JSON line, so the
//! same reader that consumes exported telemetry can consume a crash report.

use std::io::Write;
use std::panic::PanicHookInfo;
use std::time::Duration;

use serde::Serialize;

use ncui_core::{ErrorKind, NcError, Result};

/// The longest panic message a report carries.
///
/// A panic message is formatted by whatever panicked; most are a sentence, and one over
/// this bound is a stream, not a message. The tail is cut at a character boundary and
/// marked so a reader can tell a cut message from a whole one.
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024;

/// The longest backtrace a report carries.
///
/// A deep stack in a large program can produce more text than the rest of the report by
/// orders of magnitude. The head of the backtrace — the frames closest to the panic — is
/// the part that names the fault, so that is the part kept.
pub const MAX_BACKTRACE_BYTES: usize = 64 * 1024;

/// Where a panic happened, as the standard library reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PanicLocation {
    /// The source file the panic came from.
    pub file: String,
    /// The line in that file.
    pub line: u32,
    /// The column on that line.
    pub column: u32,
}

/// One crash, as the process that suffered it recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrashReport {
    /// The runtime version that panicked, e.g. `"0.1.0"`.
    pub version: String,
    /// The wall-clock moment of the panic, as nanoseconds since the Unix epoch.
    ///
    /// Supplied by the caller rather than read here, so a report is reproducible from
    /// the inputs that produced it.
    pub unix_time_nanos: u128,
    /// How long the process had been up when it panicked.
    ///
    /// A crash three seconds after launch and the same crash after three days are
    /// different investigations: this is the field that tells them apart.
    pub uptime: Duration,
    /// The thread the panic happened on, or `"unnamed"` when it had no name.
    pub thread: String,
    /// The panic message, bounded to [`MAX_MESSAGE_BYTES`].
    pub message: String,
    /// Where the panic was raised, when the standard library could name it.
    ///
    /// A panic from `panic!()` with no explicit location still carries the call site;
    /// a panic re-raised across a boundary may not.
    pub location: Option<PanicLocation>,
    /// The captured backtrace, bounded to [`MAX_BACKTRACE_BYTES`].
    ///
    /// Captured with `force_capture`, which does not consult `RUST_BACKTRACE`: a crash
    /// report that depends on an environment variable being set is a report that is
    /// empty exactly when it is needed most.
    pub backtrace: String,
}

impl CrashReport {
    /// Builds a report from a panic's parts.
    ///
    /// [`CrashReport::capture`] extracts these parts from a [`PanicHookInfo`]; this
    /// constructor is the bounded, clock-free half that tests can drive directly.
    #[must_use]
    pub fn from_parts(
        version: &str,
        unix_time_nanos: u128,
        uptime: Duration,
        thread: Option<&str>,
        message: &str,
        location: Option<PanicLocation>,
        backtrace: &str,
    ) -> Self {
        Self {
            version: version.to_owned(),
            unix_time_nanos,
            uptime,
            thread: thread.unwrap_or("unnamed").to_owned(),
            message: bounded(message, MAX_MESSAGE_BYTES),
            location,
            backtrace: bounded(backtrace, MAX_BACKTRACE_BYTES),
        }
    }

    /// Captures a report from a panic the standard library handed to a hook.
    ///
    /// `process_start` is an `Instant` taken at process start, so the uptime is the
    /// caller's measurement rather than a second clock read here.
    #[must_use]
    pub fn capture(
        info: &PanicHookInfo<'_>,
        process_uptime: Duration,
        unix_time_nanos: u128,
        version: &str,
    ) -> Self {
        let message = panic_payload_message(info.payload());
        let location = info.location().map(|location| PanicLocation {
            file: location.file().to_owned(),
            line: location.line(),
            column: location.column(),
        });
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        Self::from_parts(
            version,
            unix_time_nanos,
            process_uptime,
            std::thread::current().name(),
            &message,
            location,
            &backtrace,
        )
    }

    /// Writes the report as one JSON line.
    ///
    /// One line, newline-terminated, so a crash directory is readable the same way the
    /// telemetry sink is: line by line, with a truncated final line costing one record.
    ///
    /// # Errors
    ///
    /// A serialisation or sink failure is reported rather than swallowed: a crash
    /// report that could not be written is a lost diagnostic, and the caller — the hook
    /// that is about to let the process die — decides what that is worth.
    pub fn write<W: Write + ?Sized>(self, sink: &mut W) -> Result<()> {
        let line = serde_json::to_string(&self).map_err(|err| {
            NcError::new(ErrorKind::Codec {
                operation: "crash::write report".to_owned(),
                reason: err.to_string(),
            })
        })?;
        sink.write_all(line.as_bytes())
            .and_then(|()| sink.write_all(b"\n"))
            .map_err(|err| {
                NcError::new(ErrorKind::Io {
                    operation: "crash::write report".to_owned(),
                    io_kind: err.kind(),
                    message: err.to_string(),
                })
            })
    }
}

/// The panic payload as a message string.
///
/// A payload raised by `panic!("literal")` or `panic!(string)` carries the text; one
/// raised by `panic!(arbitrary_value)` carries something that may not be text at all,
/// and is reported by its type name rather than dropped.
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_owned();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    // A payload that is neither text nor a string is reported by the fact of its kind:
    // the stable surface offers no name for a value behind `dyn Any`, and inventing one
    // would be worse than saying what is known.
    "<non-string panic payload>".to_owned()
}

/// Cuts text to `max` bytes at a character boundary, marking the cut.
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[cut]", &text[..end])
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Duration;

    use super::{CrashReport, MAX_BACKTRACE_BYTES, MAX_MESSAGE_BYTES, PanicLocation, bounded};

    /// A sink that accepts the first `survive` writes and then refuses, for asserting
    /// that a write failure reaches the caller instead of being swallowed.
    struct FailingSink {
        survive: usize,
        written: Vec<u8>,
    }

    impl FailingSink {
        fn failing(after: usize) -> Self {
            Self {
                survive: after,
                written: Vec::new(),
            }
        }
    }

    impl Write for FailingSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.survive == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "sink closed",
                ));
            }
            self.survive -= 1;
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn sample_report() -> CrashReport {
        CrashReport::from_parts(
            "0.1.0",
            1_700_000_000_000_000_000,
            Duration::from_secs(41),
            Some("main"),
            "explicit panic",
            Some(PanicLocation {
                file: "src/main.rs".to_owned(),
                line: 7,
                column: 5,
            }),
            "stack backtrace:\n  0: main",
        )
    }

    #[test]
    fn a_report_is_one_line_of_json_naming_what_happened() {
        let report = sample_report();
        let mut sink = Vec::new();
        report.clone().write(&mut sink).expect("write succeeds");
        let line = String::from_utf8(sink).expect("utf8");
        assert!(line.ends_with('\n'), "the report is newline-terminated");
        assert_eq!(line.matches('\n').count(), 1, "exactly one line: {line:?}");
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("valid json");
        assert_eq!(value["version"], "0.1.0");
        assert_eq!(value["thread"], "main");
        assert_eq!(value["message"], "explicit panic");
        assert_eq!(value["location"]["file"], "src/main.rs");
        assert_eq!(value["location"]["line"], 7);
        assert_eq!(value["uptime"]["secs"], 41);
        assert_eq!(
            value["unix_time_nanos"].as_u64(),
            Some(1_700_000_000_000_000_000u64)
        );
    }

    #[test]
    fn a_thread_without_a_name_is_reported_as_unnamed() {
        let report = CrashReport::from_parts("0.1.0", 0, Duration::ZERO, None, "boom", None, "");
        assert_eq!(report.thread, "unnamed");
        assert!(report.location.is_none());
    }

    #[test]
    fn an_oversized_message_is_cut_at_a_character_boundary_and_marked() {
        let long = "x".repeat(MAX_MESSAGE_BYTES + 1024);
        let report =
            CrashReport::from_parts("0.1.0", 0, Duration::ZERO, Some("t"), &long, None, "");
        assert!(report.message.len() < long.len(), "the message was cut");
        assert!(
            report.message.ends_with("…[cut]"),
            "the cut is visible to a reader: {}",
            report.message.len()
        );
        assert!(
            report.message.len() <= MAX_MESSAGE_BYTES + "…[cut]".len(),
            "the cut stays close to the bound"
        );
    }

    #[test]
    fn a_multibyte_message_is_not_cut_mid_character() {
        // Each 'é' is two bytes; a byte-boundary cut would split one.
        let long = "é".repeat(MAX_MESSAGE_BYTES);
        let report =
            CrashReport::from_parts("0.1.0", 0, Duration::ZERO, Some("t"), &long, None, "");
        assert!(
            report.message.starts_with("é"),
            "the cut never splits a character"
        );
    }

    #[test]
    fn an_oversized_backtrace_is_cut() {
        let long = "0: frame\n".repeat(MAX_BACKTRACE_BYTES);
        let report =
            CrashReport::from_parts("0.1.0", 0, Duration::ZERO, Some("t"), "boom", None, &long);
        assert!(
            report.backtrace.len() < long.len(),
            "the backtrace was cut to the bound"
        );
    }

    #[test]
    fn a_write_failure_is_reported_not_swallowed() {
        let report = sample_report();
        let mut sink = FailingSink::failing(0);
        let err = report
            .write(&mut sink)
            .expect_err("a closed sink must be reported");
        assert!(
            err.to_string().contains("crash::write report"),
            "the operation names the failure: {err}"
        );
    }

    #[test]
    fn a_message_containing_a_newline_still_produces_one_line() {
        // A panic message is arbitrary text, newlines included. The report stays one
        // line, so a crash directory stays readable line by line.
        let report = CrashReport::from_parts(
            "0.1.0",
            0,
            Duration::ZERO,
            Some("t"),
            "first line\nsecond line\n",
            None,
            "",
        );
        let mut sink = Vec::new();
        report.write(&mut sink).expect("write succeeds");
        let text = String::from_utf8(sink).expect("utf8");
        assert_eq!(
            text.matches('\n').count(),
            1,
            "one record, one line: {text:?}"
        );
        let value: serde_json::Value = serde_json::from_str(text.trim()).expect("valid json");
        assert_eq!(value["message"], "first line\nsecond line\n");
    }

    #[test]
    fn bounding_a_short_text_leaves_it_alone() {
        assert_eq!(bounded("short", 1024), "short");
    }
}
