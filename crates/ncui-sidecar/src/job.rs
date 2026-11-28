//! The per-sidecar job object: the never-orphan guarantee's mechanism.
//!
//! One job per sidecar — not per supervisor — because the kill granularity is
//! the point: terminating one sidecar is dropping its job handle, which the
//! kernel honors by killing the whole tree the sidecar started, grandchildren
//! included. No `TerminateProcess` anywhere in this crate: the operating
//! system does the killing, which is why there is also no unsafe.
//!
//! The never-orphan guarantee is unchanged by the per-sidecar shape: every
//! handle the supervisor holds closes when the host dies — clean exit, crash,
//! anything — and `KILL_ON_JOB_CLOSE` turns each close into the kernel's own
//! kill. On non-Windows hosts the type is a documented no-op; the crate root
//! says what that costs.

use std::io;
use std::process::Child;

/// One sidecar's job object. Dropping it is the kill switch: the kernel ends
/// every process assigned, however deep the tree.
#[derive(Debug)]
pub(crate) struct SidecarJob {
    #[cfg(target_os = "windows")]
    job: win32job::Job,
}

impl SidecarJob {
    /// Creates the job with kill-on-close semantics.
    ///
    /// # Errors
    ///
    /// Job creation is an OS call and can fail; without it there is no
    /// guarantee, so the failure is surfaced rather than supervised around.
    #[cfg(target_os = "windows")]
    pub(crate) fn new() -> io::Result<Self> {
        let mut info = win32job::ExtendedLimitInfo::new();
        info.limit_kill_on_job_close();
        let job = win32job::Job::create_with_limit_info(&info)
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self { job })
    }

    #[cfg(not(target_os = "windows"))]
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {})
    }

    /// Assigns a freshly spawned incarnation. Every incarnation is assigned,
    /// not just the first — the guarantee does not lapse across a restart.
    ///
    /// # Errors
    ///
    /// Assignment either happens atomically or fails; there is no
    /// half-assigned state. A child that cannot be assigned must not survive
    /// the host, which is the caller's kill-before-return.
    #[cfg(target_os = "windows")]
    pub(crate) fn assign(&self, child: &Child) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        self.job
            .assign_process(child.as_raw_handle() as isize)
            .map_err(|error| io::Error::other(error.to_string()))
    }

    #[cfg(not(target_os = "windows"))]
    pub(crate) fn assign(&self, _child: &Child) -> io::Result<()> {
        Ok(())
    }
}
