//! Crash reporting: the hook that leaves a report on disk when this process panics.
//!
//! The report's shape lives in `ncui-observability` ([`CrashReport`]); this module owns
//! the two decisions a report cannot make for itself: **where the file goes**, and **what
//! happens to the panic afterwards**. The panic is re-reported to the previous hook, so
//! the stderr a developer expects is unchanged; the report is written best-effort beside
//! it, because a crash hook that panics turns one failure into an abort.
//!
//! # Where reports go
//!
//! `NCUI_CRASH_DIR` names the directory when set; otherwise the system temp directory's
//! `ncui-crash-reports` is used. The variable is read once at installation:
//! a crash report's destination is not something to re-decide while crashing.

use std::panic;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ncui_observability::CrashReport;

/// The environment variable that names the crash-report directory.
pub const CRASH_DIR_ENV: &str = "NCUI_CRASH_DIR";

/// The directory used when the environment does not name one.
pub const DEFAULT_CRASH_DIRECTORY: &str = "ncui-crash-reports";

/// The crash-report directory: the environment's choice, or the temp-directory default.
#[must_use]
pub fn crash_directory() -> PathBuf {
    if let Ok(directory) = std::env::var(CRASH_DIR_ENV)
        && !directory.trim().is_empty()
    {
        return PathBuf::from(directory);
    }
    std::env::temp_dir().join(DEFAULT_CRASH_DIRECTORY)
}

/// Installs the crash-report hook, writing reports to [`crash_directory`].
///
/// Call once, at process start, with the instant the process began so a report can say
/// how long the process had been up when it died.
pub fn install(process_start: Instant) {
    install_into(crash_directory(), process_start);
}

/// Installs the crash-report hook, writing reports into `directory`.
///
/// The previous hook is kept and called first, so installing this never silences the
/// panic output a developer or test harness already relies on.
pub fn install_into(directory: PathBuf, process_start: Instant) {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        previous(info);
        let report = CrashReport::capture(
            info,
            process_start.elapsed(),
            unix_time_nanos(),
            env!("CARGO_PKG_VERSION"),
        );
        // Best-effort by contract: a report that cannot be written must not turn a
        // panic into an abort, and there is nowhere left to report the failure to.
        let _ = write_report(&directory, &report);
    }));
}

/// Writes one report into `directory` as `crash-<unix nanos>.json`.
///
/// # Errors
///
/// Propagates the filesystem failure when the directory cannot be created or the file
/// cannot be written.
fn write_report(directory: &Path, report: &CrashReport) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    let path = directory.join(format!("crash-{}.json", report.unix_time_nanos));
    let mut file = std::fs::File::create(path)?;
    // A serialisation failure cannot happen for these fields, and an I/O failure has
    // nowhere better to go from inside a panic hook; both are best-effort here.
    let _ = report.clone().write(&mut file);
    Ok(())
}

/// The wall clock as nanoseconds since the Unix epoch, or `0` before it.
fn unix_time_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_nanos())
}
