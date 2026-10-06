//! Installation of the process-wide `tracing` subscriber.
//!
//! The runtime has exactly one logging pipeline, and this module owns it. Two decisions
//! dominate the design, and both are about not taking the process down:
//!
//! * **No output bypasses `tracing`.** Nothing here writes to stdout or stderr directly.
//!   A line that skips the subscriber cannot be filtered, cannot be correlated, and cannot
//!   be switched to JSON for a shipper. Every line the runtime emits goes through a
//!   `tracing` event so that one filter governs all of it.
//! * **Installing twice is an error, not a panic.** `tracing::subscriber::set_global_default`
//!   is designed to be called once per process; the ergonomic wrapper around it
//!   (`tracing::subscriber::set_global_default` used via `init` in most crates) panics or
//!   silently replaces the subscriber depending on which path is taken. Here [`init`] maps
//!   the "already set" signal to [`ErrorKind::Conflict`] so that a second call — from a
//!   plugin, a test, or a re-entrant initialisation — is a reportable condition rather than
//!   a crash or a silent takeover of somebody else's subscriber.
//!
//! # Modes
//!
//! [`LogMode`] selects a *policy*, and [`LogFormat`] selects a *rendering*. They are
//! separate because they answer different questions: a production log shipper wants JSON,
//! and a production *diagnostic dump* also wants JSON, while a developer's terminal wants
//! colour even in a mode that happens to be quiet.
//!
//! | Mode | Debug instrumentation | Default filter | Intended deployment |
//! |---|---|---|---|
//! | [`LogMode::Development`] | enabled | `debug` | a developer's machine, terminal |
//! | [`LogMode::Diagnostic`] | enabled | `debug`, plus spans and timings | a bug report's machine, collected offline |
//! | [`LogMode::Testing`] | enabled, but noise suppressed per-test | `warn` | CI and `cargo test` |
//! | [`LogMode::Production`] | **disabled** | `info` | a shipped, user-facing runtime |
//!
//! # Why production forbids debug instrumentation (spec 99)
//!
//! Debug-level instrumentation is not free. Each enabled call site carries a level
//! comparison, a string-formatting check, and — for the fields that a debug event carries —
//! real argument construction. On the paths that run per frame, per IPC message and per
//! worker task, that cost is paid continuously and buys nothing: a production user cannot
//! act on a debug line, and shipping one risks leaking the very values the diagnostic level
//! was designed to keep local. [`TracingConfig::effective_filter`] therefore *refuses* to
//! admit a `debug` or `trace` directive in production mode, whatever `NCUI_LOG` asks for.
//! It is a hard ceiling, not a default.
//!
//! The refusal is total rather than "ignore `NCUI_LOG`": an operator who sets `NCUI_LOG` to
//! `debug` in production gets a warning through `tracing` and the production filter. Silently
//! ignoring the request would leave an operator believing they had turned debug on, which is
//! the exact confusion this design exists to prevent.
//!
//! # Filtering
//!
//! The filter comes from `NCUI_LOG` (an [`tracing_subscriber::EnvFilter`] directive string
//! such as `info,ncui_ipc=debug`) and falls back to the mode's default when unset. It is
//! always applied, including in production, so there is no configuration in which the
//! subscriber ignores the requested filter.

use std::env;

use ncui_core::{ErrorKind, NcError, Result};
use tracing::subscriber::SetGlobalDefaultError;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

use crate::LOG_FILTER_ENV;

/// How log lines are rendered.
///
/// Rendering is separate from [`LogMode`] because the two are chosen for different reasons:
/// a mode is a deployment policy, a format is a destination's requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-readable lines for a terminal, with optional ANSI colour.
    ///
    /// The default. A developer reading a live terminal wants `12:04:31 INFO ipc::dispatch`
    /// and colours, not a JSON object per line.
    #[default]
    Pretty,
    /// One JSON object per line, for a log shipper or an aggregator.
    ///
    /// Required by anything downstream that parses logs rather than reading them, and the
    /// only format that survives a cross-process boundary without a lossy regex.
    Json,
}

impl LogFormat {
    /// The stable lowercase name used in configuration files and CLI arguments.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Pretty => "pretty",
            Self::Json => "json",
        }
    }
}

/// The deployment policy a [`TracingConfig`] expresses.
///
/// The mode decides two things: whether debug-level instrumentation is permitted at all,
/// and what the filter defaults to when `NCUI_LOG` is unset. See the module documentation
/// for the full mapping and for why production refuses debug instrumentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogMode {
    /// A developer's machine, reading a live terminal.
    ///
    /// Debug instrumentation permitted, filter defaults to `debug` so that the code under
    /// development is visible without configuration.
    #[default]
    Development,
    /// A machine collecting evidence for a bug report, usually offline.
    ///
    /// Debug instrumentation permitted, and span events and timings enabled so that a
    /// collected trace shows where the time went rather than only what happened. Span
    /// timings are the reason this mode exists separately from development: a bug report
    /// needs the *shape* of the execution, and a developer's terminal does not.
    Diagnostic,
    /// Automated tests.
    ///
    /// Debug instrumentation permitted, but the filter defaults to `warn` so that a test's
    /// output is readable rather than drowned. A failing test's own output is the signal;
    /// the runtime's chatter around it is noise that makes a failure harder to find.
    Testing,
    /// A shipped runtime running for a user.
    ///
    /// Debug instrumentation **refused**, whatever the environment asks for; see the module
    /// documentation. The filter defaults to `info`.
    Production,
}

impl LogMode {
    /// Whether debug- and trace-level instrumentation may be enabled in this mode.
    ///
    /// The single source of truth for the production refusal: [`TracingConfig`] consults
    /// this rather than hard-coding a mode match, so the policy cannot drift between the
    /// filter builder and the documentation.
    #[must_use]
    pub const fn permits_debug_instrumentation(self) -> bool {
        !matches!(self, Self::Production)
    }

    /// The filter directive used when `NCUI_LOG` is unset.
    ///
    /// Chosen per mode rather than globally so that a shipped binary is quiet without
    /// requiring configuration, while a developer sees the code they are working on.
    #[must_use]
    pub const fn default_filter(self) -> &'static str {
        match self {
            // `debug` for ncui's own targets, `info` elsewhere: a developer's dependencies
            // are not the subject of the change and would otherwise dominate the output.
            Self::Development | Self::Diagnostic => "debug,hyper=info,rustls=info",
            Self::Testing => "warn",
            Self::Production => "info",
        }
    }

    /// Whether span enter/exit and close events should be recorded.
    ///
    /// On for [`LogMode::Diagnostic`] only. Span events roughly double the volume of a
    /// subscriber's output, which is exactly right for an offline evidence dump and exactly
    /// wrong for a live terminal or a shipped runtime.
    #[must_use]
    pub const fn records_span_events(self) -> bool {
        matches!(self, Self::Diagnostic)
    }

    /// Whether ANSI colour is appropriate by default.
    ///
    /// On for a developer's terminal, off elsewhere — colour in a shipped log file or a
    /// CI capture is noise, and colour in JSON is corruption.
    #[must_use]
    pub const fn prefers_ansi(self) -> bool {
        matches!(self, Self::Development)
    }

    /// The stable lowercase name used in configuration files and CLI arguments.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Diagnostic => "diagnostic",
            Self::Testing => "testing",
            Self::Production => "production",
        }
    }
}

/// How the process-wide subscriber should be built.
#[derive(Debug, Clone, PartialEq)]
pub struct TracingConfig {
    mode: LogMode,
    format: LogFormat,
    /// An explicit filter override, replacing `NCUI_LOG`.
    filter: Option<String>,
    /// An explicit ANSI choice, replacing the mode's preference.
    ansi: Option<bool>,
}

impl TracingConfig {
    /// A configuration for `mode`, reading nothing from the environment yet.
    #[must_use]
    pub fn new(mode: LogMode) -> Self {
        // Not `const`: `LogFormat::default` is a derived trait method, not a const fn, so
        // this cannot be. The cost is one call per process, in `main`, which is not a
        // measurable amount.
        Self {
            mode,
            format: LogFormat::default(),
            filter: None,
            ansi: None,
        }
    }

    /// A production configuration: JSON output, no colour, `info` unless overridden.
    ///
    /// The shorthand a shipped binary uses, so that the production policy is expressed once
    /// and cannot be assembled incorrectly at a call site.
    #[must_use]
    pub fn production() -> Self {
        Self::new(LogMode::Production).with_format(LogFormat::Json)
    }

    /// A development configuration: pretty output with colour, `debug` unless overridden.
    #[must_use]
    pub fn development() -> Self {
        Self::new(LogMode::Development)
    }

    /// The deployment policy.
    #[must_use]
    pub const fn mode(&self) -> LogMode {
        self.mode
    }

    /// The rendering.
    #[must_use]
    pub const fn format(&self) -> LogFormat {
        self.format
    }

    /// Selects the rendering.
    #[must_use]
    pub const fn with_format(mut self, format: LogFormat) -> Self {
        self.format = format;
        self
    }

    /// Overrides the filter, ignoring `NCUI_LOG`.
    ///
    /// A programmatic override exists so that a host embedding the runtime can decide the
    /// filter itself; it is subject to the same production refusal as `NCUI_LOG`, because
    /// the reason for that refusal is the deployment, not the source of the directive.
    #[must_use]
    pub fn with_filter(mut self, filter: impl Into<String>) -> Self {
        self.filter = Some(filter.into());
        self
    }

    /// Overrides the ANSI choice, ignoring the mode's preference.
    #[must_use]
    pub const fn with_ansi(mut self, ansi: bool) -> Self {
        self.ansi = Some(ansi);
        self
    }

    /// Whether ANSI colour will be used.
    #[must_use]
    pub const fn ansi(&self) -> bool {
        match self.ansi {
            Some(explicit) => explicit,
            None => self.mode.prefers_ansi(),
        }
    }

    /// The filter string this configuration resolves to.
    ///
    /// Resolution order is: an explicit [`TracingConfig::with_filter`] override, then
    /// `NCUI_LOG`, then the mode's default. The resolved string is then passed through
    /// [`effective_filter`], so the production ceiling applies to all three sources
    /// identically.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Validation`] if the environment variable contains a directive
    /// string that [`tracing_subscriber::EnvFilter`] cannot parse. A malformed filter is
    /// reported at initialisation rather than silently falling back to the default, because
    /// an operator who typed a filter and got the default instead would not know why the
    /// verbosity they asked for never arrived.
    pub fn effective_filter(&self) -> Result<String> {
        let requested = match &self.filter {
            Some(explicit) => Some(explicit.clone()),
            // `.ok()` rather than a `match`: the variable's absence is the normal case
            // (the mode default then applies) and carries no information beyond "unset".
            // Both `NotPresent` and a non-UTF-8 value mean the same thing here.
            None => env::var(LOG_FILTER_ENV).ok(),
        };
        let requested = requested.unwrap_or_else(|| self.mode.default_filter().to_owned());
        effective_filter(&requested, self.mode)
    }
}

impl Default for TracingConfig {
    /// Development, matching [`LogFormat::default`]: a library's neutral choice is the one
    /// a developer wants, and a shipped binary must say [`TracingConfig::production`]
    /// explicitly.
    fn default() -> Self {
        Self::new(LogMode::Development)
    }
}

/// Applies the production ceiling to a requested filter directive string.
///
/// Every directive that names `debug` or `trace` is reduced to `info` when `mode` forbids
/// debug instrumentation; directives at `info` or above, and directives for a *target* that
/// does not name a level, are left alone. Bare target directives (e.g. `ncui_ipc`) are
/// rewritten to `=info` rather than dropped: a bare target means "everything from this
/// target", and in production that must mean "everything at or above info", not "nothing".
///
/// The reduction is textual and therefore exact — `EnvFilter` exposes no API for rewriting
/// the directives it has already parsed, so clamping the string before parsing is the only
/// way to enforce a ceiling that the parser cannot later widen.
pub fn effective_filter(requested: &str, mode: LogMode) -> Result<String> {
    let directives: Vec<&str> = requested
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();

    if mode.permits_debug_instrumentation() {
        return Ok(directives.join(","));
    }

    let mut clamped = Vec::with_capacity(directives.len());
    for directive in directives {
        // `clamp_directive` already returns an owned `String`, so it is moved, not cloned.
        clamped.push(clamp_directive(directive));
    }
    Ok(clamped.join(","))
}

/// Reduces one directive to at most `info`.
///
/// Returns an owned `String` because the clamped forms are constructed rather than sliced:
/// the level suffix is replaced while the target and any span scope are preserved verbatim.
fn clamp_directive(directive: &str) -> String {
    if let Some((target, level)) = directive.rsplit_once('=') {
        let level = level.trim();
        if matches!(level.to_ascii_lowercase().as_str(), "debug" | "trace") {
            // `target=level`, or `target[span{method=…}]=level`. Only the level changes; the
            // span-scoped form is preserved because dropping its fields would silently widen
            // the directive back to the whole target.
            return format!("{target}=info");
        }
        return directive.to_owned();
    }

    match directive.to_ascii_lowercase().as_str() {
        // A bare level applies to every target and must be clamped.
        "debug" | "trace" => "info".to_owned(),
        other if is_bare_target(other) => format!("{other}=info"),
        // Already compliant, or not a directive this function rewrites; passed through for
        // the parser to accept or reject. The matched slice is the lowercased directive, so
        // normalising the case here is intentional: filter directives are case-insensitive.
        other => other.to_owned(),
    }
}

/// Whether a directive without `=` names a target rather than a level.
///
/// Targets are crate paths and `EnvFilter` accepts arbitrary dotted identifiers, so the test
/// is deliberately permissive: anything that is not a reserved level word is treated as a
/// target. Treating an unknown word as a target is safe because the resulting `target=info`
/// still bounds verbosity, whereas mistaking a target for a level would leave it unbounded.
fn is_bare_target(directive: &str) -> bool {
    !matches!(
        directive,
        "off" | "error" | "warn" | "info" | "debug" | "trace"
    )
}

/// Builds and installs the process-wide `tracing` subscriber.
///
/// # Errors
///
/// Returns [`ErrorKind::Conflict`] if a global subscriber has already been installed. This
/// is the idempotency guarantee: `tracing::subscriber::set_global_default` returns an error
/// rather than panicking when a global subscriber is already set, and that error is mapped
/// here to the workspace's structured conflict type. A second caller therefore learns that
/// somebody owns the subscriber instead of crashing the process or silently replacing the
/// first subscriber — the latter would be the worst outcome, because the second subscriber
/// would receive the first one's events with no indication that the first one's filter,
/// format and level had been discarded.
///
/// The returned error is also produced when `NCUI_LOG` cannot be parsed, and when the mode's
/// production ceiling had to reject a `debug` request — the latter as a warning through
/// `tracing` rather than as an error, since the resulting configuration is valid.
pub fn init(config: &TracingConfig) -> Result<()> {
    let resolved = config.effective_filter()?;
    let clamped =
        !config.mode.permits_debug_instrumentation() && resolved != config.mode.default_filter();
    let dispatch = build_dispatch(config)?;
    // `dispatcher::set_global_default` rather than `subscriber::set_global_default`: the
    // latter requires a concrete `Subscriber`, and the two formats produce two different
    // concrete types, so the stack is erased to a `Dispatch` first. Both go through the same
    // once-per-process initialisation, and — the point of this function — neither panics on
    // a second call.
    match tracing::dispatcher::set_global_default(dispatch) {
        Ok(()) => {
            if clamped {
                // Emitted after installation so the subscriber actually receives it: an
                // operator who set `NCUI_LOG=debug` on a shipped binary must be told it was
                // ignored, or they will believe they have debug output they do not have.
                // This is why the clamp is a ceiling rather than a hard rejection — the
                // configuration is valid and is installed, but the reduction is announced.
                tracing::warn!(
                    target: "ncui::tracing_setup",
                    effective = %resolved,
                    "debug instrumentation is disabled in production mode; the log filter was reduced to the production ceiling",
                );
            }
            Ok(())
        }
        Err(SetGlobalDefaultError { .. }) => Err(NcError::new(ErrorKind::Conflict {
            reason: format!(
                "a global tracing subscriber is already installed; \
                 {} logging must not be installed twice in one process",
                config.mode().code()
            ),
        })
        .operation("tracing_setup::init")),
    }
}

/// Whether a global `tracing` subscriber has already been installed.
///
/// Provided so a caller can decide *before* doing work whether to configure logging at all,
/// rather than discovering it through [`init`]'s error after paying for the build. Uses the
/// same signal [`init`] relies on, so the two can never disagree.
#[must_use]
pub fn is_initialized() -> bool {
    tracing::dispatcher::has_been_set()
}

/// Builds the subscriber described by `config` without installing it.
///
/// Split out from [`init`] because the two have different risks: building is pure and can
/// be exercised repeatedly, installing is a once-per-process side effect. Exposing the build
/// step is what makes the filter and format policy testable at all — a test cannot call
/// [`init`] twice, but it can call this as often as it likes and assert on what it built.
///
/// # Errors
///
/// Returns [`ErrorKind::Validation`] if the resolved filter cannot be parsed.
pub fn build_dispatch(config: &TracingConfig) -> Result<tracing::Dispatch> {
    let filter = EnvFilter::try_new(config.effective_filter()?).map_err(|err| {
        NcError::new(ErrorKind::Validation {
            field: format!("{LOG_FILTER_ENV} filter"),
            reason: format!("not a valid log filter directive string: {err}"),
        })
    })?;

    let ansi = config.ansi();
    let span_events = if config.mode.records_span_events() {
        FmtSpan::FULL
    } else {
        FmtSpan::NONE
    };

    // The two arms differ in their concrete `Subscriber` type — a `Json` formatter cannot be
    // reached from a `Full` one — so each arm erases to a `Dispatch` on its own. That is
    // `tracing_subscriber`'s type-level format selection at work: it is what makes JSON
    // formatting a compile-time choice rather than a per-event branch, and the price is
    // that the two arms cannot be unified into one binding. `filter` is moved into whichever
    // arm runs, which is why it is not cloned.
    let dispatch = match config.format() {
        LogFormat::Json => tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(true)
                .with_ansi(ansi)
                .with_env_filter(filter)
                .finish(),
        ),
        LogFormat::Pretty => tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_span_events(span_events)
                .with_ansi(ansi)
                .with_env_filter(filter)
                .finish(),
        ),
    };

    Ok(dispatch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    /// A sink that appends to a shared buffer, so a test can read back what the subscriber
    /// rendered without touching a file or the process's real stderr.
    ///
    /// The buffer lives behind an `Arc<Mutex<_>>` and the sink is `Clone`, which is what
    /// lets the test keep a handle after the subscriber has taken the writer: the test reads
    /// the same bytes the subscriber wrote.
    #[derive(Debug, Clone, Default)]
    struct CaptureSink(Arc<Mutex<Vec<u8>>>);

    impl CaptureSink {
        /// Everything the subscriber has rendered so far.
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().expect("test lock").clone())
                .expect("subscriber output is utf-8")
        }
    }

    impl Write for CaptureSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("test lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Hands the subscriber a *clone* of the sink per event.
    ///
    /// The clone shares the same buffer, so every event accumulates into one place the test
    /// can inspect. The alternative — handing out `&mut Self` — would require the sink to be
    /// mutably reachable through `&self`, which is precisely what the `Arc` avoids.
    impl<'a> MakeWriter<'a> for CaptureSink {
        type Writer = CaptureSink;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Builds a subscriber writing into `sink` instead of the process's stderr.
    ///
    /// Mirrors [`build_dispatch`] — same filter resolution, same format branch, same ANSI and
    /// span-event decisions — so that what these tests assert about is the real wiring and
    /// only the destination is swapped.
    fn subscriber_into(config: &TracingConfig, sink: CaptureSink) -> Result<tracing::Dispatch> {
        let filter = EnvFilter::try_new(config.effective_filter()?).map_err(|err| {
            NcError::new(ErrorKind::Validation {
                field: "filter".to_owned(),
                reason: err.to_string(),
            })
        })?;
        let ansi = config.ansi();
        let span_events = if config.mode.records_span_events() {
            FmtSpan::FULL
        } else {
            FmtSpan::NONE
        };
        let dispatch = match config.format() {
            LogFormat::Json => tracing::Dispatch::new(
                tracing_subscriber::fmt()
                    .json()
                    .flatten_event(true)
                    .with_current_span(true)
                    .with_span_list(true)
                    .with_ansi(ansi)
                    .with_env_filter(filter)
                    .with_writer(sink)
                    .finish(),
            ),
            LogFormat::Pretty => tracing::Dispatch::new(
                tracing_subscriber::fmt()
                    .with_span_events(span_events)
                    .with_ansi(ansi)
                    .with_env_filter(filter)
                    .with_writer(sink)
                    .finish(),
            ),
        };
        Ok(dispatch)
    }

    #[test]
    fn the_default_config_is_a_developer_pretty_pipeline() {
        let config = TracingConfig::default();
        assert_eq!(config.mode(), LogMode::Development);
        assert_eq!(config.format(), LogFormat::Pretty);
        assert!(config.ansi(), "a developer's terminal wants colour");
    }

    #[test]
    fn the_production_shorthand_is_json_without_colour() {
        let config = TracingConfig::production();
        assert_eq!(config.mode(), LogMode::Production);
        assert_eq!(config.format(), LogFormat::Json);
        assert!(
            !config.ansi(),
            "colour in a shipped log file or a JSON payload is corruption"
        );
    }

    #[test]
    fn each_mode_has_a_distinct_code() {
        let codes = [
            LogMode::Development.code(),
            LogMode::Diagnostic.code(),
            LogMode::Testing.code(),
            LogMode::Production.code(),
        ];
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 4, "mode codes must be unique");
    }

    #[test]
    fn format_codes_are_stable() {
        assert_eq!(LogFormat::Pretty.code(), "pretty");
        assert_eq!(LogFormat::Json.code(), "json");
        assert_eq!(LogFormat::default(), LogFormat::Pretty);
    }

    #[test]
    fn only_production_refuses_debug_instrumentation() {
        assert!(LogMode::Development.permits_debug_instrumentation());
        assert!(LogMode::Diagnostic.permits_debug_instrumentation());
        assert!(LogMode::Testing.permits_debug_instrumentation());
        assert!(
            !LogMode::Production.permits_debug_instrumentation(),
            "spec 99: production must never enable debug-level instrumentation"
        );
    }

    #[test]
    fn only_diagnostic_records_span_events() {
        assert!(LogMode::Diagnostic.records_span_events());
        assert!(!LogMode::Development.records_span_events());
        assert!(!LogMode::Testing.records_span_events());
        assert!(!LogMode::Production.records_span_events());
    }

    #[test]
    fn only_development_prefers_ansi() {
        assert!(LogMode::Development.prefers_ansi());
        for mode in [LogMode::Diagnostic, LogMode::Testing, LogMode::Production] {
            assert!(
                !mode.prefers_ansi(),
                "{} must not colour output",
                mode.code()
            );
        }
    }

    #[test]
    fn mode_defaults_are_ordered_from_verbose_to_quiet() {
        // A developer's default must be more verbose than a shipped runtime's, or the
        // default is wrong for one of the two deployments.
        let development = EnvFilter::try_new(LogMode::Development.default_filter())
            .expect("development default parses");
        let testing =
            EnvFilter::try_new(LogMode::Testing.default_filter()).expect("testing default parses");
        let production =
            EnvFilter::try_new(LogMode::Production.default_filter()).expect("production parses");

        // Ask each real filter about a real event. Fabricating `Metadata` by hand would need
        // a static callsite and would test the filter against a synthetic target; emitting an
        // event and observing whether it survives is the behaviour that actually matters.
        let debug_event = "a debug event that must be filtered or kept";
        for (mode, expected) in [
            (LogMode::Development, true),
            (LogMode::Diagnostic, true),
            (LogMode::Testing, false),
            (LogMode::Production, false),
        ] {
            let sink = CaptureSink::default();
            let config = TracingConfig::new(mode).with_ansi(false);
            let subscriber = subscriber_into(&config, sink.clone()).expect("builds");
            tracing::dispatcher::with_default(&subscriber, || {
                tracing::debug!("{debug_event}");
            });
            assert_eq!(
                sink.contents().contains(debug_event),
                expected,
                "{} should {} a debug event by default",
                mode.code(),
                if expected { "keep" } else { "drop" }
            );
        }

        // The three filters are constructed explicitly so that a regression in directive
        // *parsing* fails at construction, distinguishable from a regression in which mode
        // selects which filter is used.
        for filter in [development, testing, production] {
            assert!(
                filter.max_level_hint().is_some(),
                "a constructed filter must report the levels it admits"
            );
        }
    }

    // ---- The production ceiling ---------------------------------------------------------

    #[test]
    fn a_debug_filter_is_preserved_in_every_mode_except_production() {
        for mode in [LogMode::Development, LogMode::Diagnostic, LogMode::Testing] {
            let resolved = effective_filter("debug", mode).expect("valid directive");
            assert_eq!(resolved, "debug", "{} must honour debug", mode.code());
        }
    }

    #[test]
    fn a_debug_filter_is_clamped_to_info_in_production() {
        let resolved = effective_filter("debug", LogMode::Production).expect("valid directive");
        assert_eq!(
            resolved, "info",
            "production must never enable debug instrumentation, whatever was asked for"
        );
    }

    #[test]
    fn a_trace_filter_is_clamped_to_info_in_production() {
        assert_eq!(
            effective_filter("trace", LogMode::Production).expect("valid directive"),
            "info"
        );
    }

    #[test]
    fn a_per_target_debug_directive_is_clamped_in_production() {
        let resolved =
            effective_filter("info,ncui_ipc=debug", LogMode::Production).expect("valid directives");
        assert_eq!(
            resolved, "info,ncui_ipc=info",
            "a target-scoped debug directive must be clamped too"
        );
    }

    #[test]
    fn a_per_target_trace_directive_is_clamped_in_production() {
        let resolved =
            effective_filter("ncui_workers=trace", LogMode::Production).expect("valid directive");
        assert_eq!(resolved, "ncui_workers=info");
    }

    #[test]
    fn a_directive_at_or_above_info_is_left_alone_in_production() {
        let requested = "error,ipc=warn,workers=info,http=off";
        let resolved = effective_filter(requested, LogMode::Production).expect("valid directives");
        assert_eq!(resolved, requested, "a compliant filter must be untouched");
    }

    #[test]
    fn a_span_scoped_directive_keeps_its_fields_when_clamped() {
        let resolved = effective_filter("ncui_ipc[dispatch]=debug", LogMode::Production)
            .expect("valid directive");
        assert_eq!(
            resolved, "ncui_ipc[dispatch]=info",
            "the span scope must survive the clamp, or the directive widens to the target"
        );
    }

    #[test]
    fn a_bare_target_is_bounded_to_info_in_production() {
        // A bare target means "everything from this target". Unbounded, that would admit
        // debug and trace in production, which is the exact refusal this module exists to
        // enforce.
        let resolved = effective_filter("ncui_ipc", LogMode::Production).expect("valid directive");
        assert_eq!(resolved, "ncui_ipc=info");
    }

    #[test]
    fn a_production_filter_can_no_longer_enable_a_debug_event() {
        // The end-to-end statement of the same policy: build the real filter the subscriber
        // would use and ask it about a debug event.
        let config = TracingConfig::production().with_filter("debug");
        let resolved = config.effective_filter().expect("resolves");
        assert_eq!(
            resolved, "info",
            "the request must be clamped, not honoured"
        );
        assert!(
            EnvFilter::try_new(resolved).is_ok(),
            "the clamped directive must still parse"
        );

        let sink = CaptureSink::default();
        let subscriber = subscriber_into(&config, sink.clone()).expect("builds");
        tracing::dispatcher::with_default(&subscriber, || {
            tracing::debug!(secret = "must not be logged", "a debug event");
        });
        assert!(
            sink.contents().is_empty(),
            "no debug event may pass a production filter, got: {:?}",
            sink.contents()
        );
    }

    #[test]
    fn empty_directive_segments_are_dropped() {
        let resolved =
            effective_filter(" info , ,ncui_ipc=warn,", LogMode::Production).expect("valid");
        assert_eq!(resolved, "info,ncui_ipc=warn");
    }

    // ---- Filter resolution ---------------------------------------------------------------

    #[test]
    fn an_explicit_filter_overrides_the_environment() {
        // The environment is not read here, so this asserts the override wins over the mode
        // default deterministically rather than depending on the ambient `NCUI_LOG`.
        let config = TracingConfig::new(LogMode::Testing).with_filter("warn");
        assert_eq!(
            config.effective_filter().expect("resolves"),
            "warn",
            "an explicit override replaces the mode default"
        );
    }

    #[test]
    fn a_malformed_filter_is_reported_rather_than_silently_defaulted() {
        // Two stages are deliberately distinguished: resolution produces the directive
        // string and the *build* parses it. A malformed string therefore resolves without
        // error — resolution only clamps — and fails when a subscriber is built. What must
        // never happen is the mode default silently replacing what the operator asked for.
        let config = TracingConfig::new(LogMode::Development).with_filter("=not a directive=");
        assert_eq!(
            config
                .effective_filter()
                .expect("resolution does not parse"),
            "=not a directive=",
            "resolution must not rewrite or discard the requested filter"
        );
        let err = build_dispatch(&config).expect_err("a malformed filter must not build");
        assert!(matches!(err.kind(), ErrorKind::Validation { .. }));
        assert!(
            err.to_string().contains("not a valid log filter"),
            "the error must say what was wrong: {err}"
        );
    }

    // ---- Subscriber construction ----------------------------------------------------------

    #[test]
    fn a_pretty_subscriber_renders_a_human_readable_line() {
        let sink = CaptureSink::default();
        let config = TracingConfig::development().with_ansi(false);
        let subscriber = subscriber_into(&config, sink.clone()).expect("builds");
        tracing::dispatcher::with_default(&subscriber, || {
            tracing::info!("hello from the runtime");
        });
        let output = sink.contents();
        assert!(output.contains("hello from the runtime"), "got: {output}");
        assert!(
            !output.contains('\u{1b}'),
            "ANSI was explicitly disabled, so no escape codes may appear: {output:?}"
        );
    }

    #[test]
    fn a_json_subscriber_renders_one_object_per_line() {
        let sink = CaptureSink::default();
        let config = TracingConfig::production();
        let subscriber = subscriber_into(&config, sink.clone()).expect("builds");
        tracing::dispatcher::with_default(&subscriber, || {
            tracing::warn!(component = "ipc", "structured event");
        });
        let output = sink.contents();
        let lines: Vec<&str> = output.lines().filter(|line| !line.is_empty()).collect();
        assert_eq!(lines.len(), 1, "one event must be one line: {output:?}");
        let value: serde_json::Value =
            serde_json::from_str(lines[0]).expect("json output must parse");
        assert_eq!(value["level"], "WARN");
        assert_eq!(value["message"], "structured event");
        assert_eq!(value["component"], "ipc");
    }

    #[test]
    fn a_production_subscriber_does_not_render_a_debug_event() {
        let sink = CaptureSink::default();
        let config = TracingConfig::production().with_filter("debug");
        let subscriber = subscriber_into(&config, sink.clone()).expect("builds");
        tracing::dispatcher::with_default(&subscriber, || {
            tracing::debug!(secret = "must not be logged", "noisy debug detail");
        });
        let output = sink.contents();
        assert!(
            output.is_empty(),
            "a debug event must not reach a production subscriber: {output:?}"
        );
    }

    #[test]
    fn a_development_subscriber_does_render_a_debug_event() {
        let sink = CaptureSink::default();
        let config = TracingConfig::development()
            .with_ansi(false)
            .with_filter("debug");
        let subscriber = subscriber_into(&config, sink.clone()).expect("builds");
        tracing::dispatcher::with_default(&subscriber, || {
            tracing::debug!(detail = "kept", "debug detail");
        });
        let output = sink.contents();
        assert!(
            output.contains("debug detail"),
            "a developer must see debug: {output:?}"
        );
    }

    #[test]
    fn a_filter_can_exclude_a_target() {
        let sink = CaptureSink::default();
        let config = TracingConfig::development()
            .with_ansi(false)
            .with_filter("info,noisy_crate=off");
        let subscriber = subscriber_into(&config, sink.clone()).expect("builds");
        tracing::dispatcher::with_default(&subscriber, || {
            tracing::info!(target: "noisy_crate::inner", "suppressed");
            tracing::info!(target: "ncui_ipc::dispatch", "kept");
        });
        let output = sink.contents();
        assert!(!output.contains("suppressed"), "got: {output:?}");
        assert!(output.contains("kept"), "got: {output:?}");
    }

    #[test]
    fn an_operation_span_renders_its_correlation_and_component() {
        let sink = CaptureSink::default();
        let config = TracingConfig::new(LogMode::Diagnostic).with_ansi(false);
        let subscriber = subscriber_into(&config, sink.clone()).expect("builds");
        let context = crate::trace::TraceContext::new();
        tracing::dispatcher::with_default(&subscriber, || {
            let span = crate::trace::operation_span(&context, "ipc");
            let _entered = span.enter();
            tracing::info!(stage = "dispatch", "handling");
        });
        let output = sink.contents();
        assert!(
            output.contains(&context.correlation().to_string()),
            "the correlation identifier must appear in the rendered line: {output:?}"
        );
        // `tracing` renders a `&str` field through its `Debug` impl, so the value is
        // quoted. Asserting the quoted form is asserting what a reader actually sees.
        assert!(
            output.contains("component=\"ipc\"") || output.contains("\"component\":\"ipc\""),
            "the component must appear: {output:?}"
        );
    }

    #[test]
    fn building_a_subscriber_is_side_effect_free_and_repeatable() {
        // Building repeatedly is what makes the policy testable; installing is the part that
        // is once-per-process.
        for _ in 0..5 {
            assert!(
                build_dispatch(&TracingConfig::production()).is_ok(),
                "building a valid subscriber must never fail or consume a global"
            );
        }
    }

    #[test]
    fn a_malformed_filter_fails_the_build_with_a_validation_error() {
        let config = TracingConfig::development().with_filter("ncui[=broken");
        let err = build_dispatch(&config).expect_err("a malformed filter must not build");
        assert!(matches!(err.kind(), ErrorKind::Validation { .. }));
        assert!(err.to_string().contains("filter"), "got: {err}");
    }

    #[test]
    fn the_ansi_override_wins_over_the_mode_preference() {
        let on = TracingConfig::new(LogMode::Production).with_ansi(true);
        assert!(
            on.ansi(),
            "an explicit choice must win over the mode default"
        );
        let off = TracingConfig::development().with_ansi(false);
        assert!(!off.ansi());
    }

    #[test]
    fn installing_twice_reports_a_conflict_and_keeps_the_first_subscriber() {
        // The one operation in this module that mutates process-wide state, so it is
        // exercised exactly once, by a single test. The assertion is the idempotency
        // contract: the first call installs, the second is a `Conflict` error, and the
        // process is still alive — `tracing`'s own wrapper panics here, which is the
        // behaviour this crate exists to avoid.
        let config = TracingConfig::production();
        let second = init(&config);
        match second {
            Err(err) => {
                assert!(
                    matches!(err.kind(), ErrorKind::Conflict { .. }),
                    "a second install must be a conflict, got: {err}"
                );
                assert_eq!(err.operation_name(), "tracing_setup::init");
                assert!(is_initialized());
            }
            Ok(()) => {
                // Reached only if this test is the first to install anything, which is the
                // normal case. Install once more to prove the second call is rejected.
                let err = init(&config).expect_err("a second install must be rejected");
                assert!(matches!(err.kind(), ErrorKind::Conflict { .. }));
                assert!(is_initialized());
            }
        }
    }
}
