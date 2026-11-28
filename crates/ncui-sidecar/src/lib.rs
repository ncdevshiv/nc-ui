//! Supervised child processes: the sidecar lifecycle the runtime owns.
//!
//! A sidecar is a child process the host spawns, talks to, restarts when it
//! crashes, and — above all — never leaves behind. The station domain module
//! is the first consumer: a Bun-or-Go process owned by the runtime, spoken to
//! over stdio, expected to live exactly as long as its host and not one
//! second longer.
//!
//! # The never-orphan guarantee, honestly
//!
//! On Windows the guarantee is structural, not best-effort: every child is
//! assigned to a Job Object created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`,
//! so the operating system itself kills every sidecar the moment the host's
//! handle closes — clean exit, crash, debugger kill, power loss short of the
//! kernel. There is no path where the host dies and the child survives.
//!
//! One residual is documented rather than hidden: assignment happens
//! immediately after spawn, so a child that itself spawns a grandchild in
//! the first microseconds of its life — before the assignment lands — can
//! leave that grandchild outside the job. A sidecar that daemonizes at
//! startup is outside the honest envelope of this guarantee; the fix
//! direction, if a real sidecar ever does that, is spawn-suspended +
//! assign + resume, which `std::process` does not expose without
//! reimplementing process creation by hand.
//!
//! On non-Windows hosts the guarantee degrades honestly: the supervisor
//! terminates children when it is dropped, but a host crash can orphan them.
//! The unix shape is a process-group kill or `PR_SET_PDEATHSIG`; it is not
//! implemented because the host targets Windows first, and a half-implemented
//! unix path would be worse than this paragraph.
//!
//! # Supervision semantics
//!
//! Restart is a policy, not an accident: [`RestartPolicy::OnFailure`]
//! restarts a child that exits non-zero, with exponential backoff, up to a
//! counted maximum. A child that exits cleanly stays down — zero is the
//! child saying it finished, and restarting finished work is a loop, not
//! supervision. An explicit terminate is never restarted. Every transition
//! is inspectable truth: state, pid, restart count, the last exit code.
//!
//! # The transport survives restart
//!
//! A restarted child is a new process with new pipes — so the byte path
//! cannot be the child's pipes, or the first crash would strand it. What the
//! owner holds instead is stable: a [`SidecarStdin`] that writes to whatever
//! incarnation is current, and a stdout pipe fed by a forwarder thread that
//! follows incarnations as they change. Writes in the death window — after
//! the child died, before the restart stands up — fail honestly with
//! `BrokenPipe`; the restart is visible in [`SidecarStatus`], and the
//! application re-handshakes on the new incarnation exactly as it would on
//! any reconnect. Bytes are never silently dropped: a write either reaches
//! the current incarnation or errors.

mod forward;
mod job;

use std::collections::HashMap;
use std::io;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub use forward::SidecarStdin;
use forward::{IncarnationSlot, spawn_stdout_forwarder};
use job::SidecarJob;

/// Identifies a supervised sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SidecarId(u64);

impl SidecarId {
    /// The numeric id.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for SidecarId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sidecar-{}", self.0)
    }
}

/// What a sidecar runs and how it is supervised.
#[derive(Debug, Clone)]
pub struct SidecarSpec {
    /// A human name for inspection — "station", "indexer". Truth in diagnostics.
    pub name: String,
    /// The program to spawn.
    pub program: std::path::PathBuf,
    /// Arguments, in order.
    pub args: Vec<String>,
    /// Extra environment, overlayed on the inherited one.
    pub env: Vec<(String, String)>,
    /// Whether the child sees only `env` — the containment posture the
    /// page-facing wire layers keep: nothing the host inherited leaks into a
    /// program a page can reach.
    pub env_clear: bool,
    /// Working directory; the host's own when unset.
    pub working_dir: Option<std::path::PathBuf>,
    /// What happens when the child exits.
    pub restart: RestartPolicy,
}

/// What happens when the child exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    /// Never restart: exit is final, however it happened.
    Never,
    /// Restart on non-zero exit, with exponential backoff from `base_backoff`
    /// doubling per attempt and capping at sixty seconds, up to
    /// `max_restarts` restarts — after which the sidecar is declared failed
    /// and stays down, because a process that cannot stay up is a fault to
    /// surface, not a loop to run.
    OnFailure {
        /// The restart ceiling.
        max_restarts: u32,
        /// The first backoff; each subsequent attempt doubles it.
        base_backoff: Duration,
    },
}

/// The inspectable truth of one sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarStatus {
    /// Alive and supervised.
    Running {
        /// The operating system process id.
        pid: u32,
        /// When this incarnation started.
        since: Instant,
    },
    /// Down, with a restart scheduled — the backoff window is the honesty
    /// that a crash-looping child is not hammering the machine.
    Restarting {
        /// Which restart attempt comes next (1-based).
        attempt: u32,
        /// The exit code that triggered the restart.
        last_exit: i32,
    },
    /// Exited and staying down — clean exit under `OnFailure`, any exit
    /// under `Never`, or a child that exhausted its restarts.
    Exited {
        /// The exit code, when the OS reported one; `None` means killed.
        code: Option<i32>,
        /// Why it stayed down.
        reason: ExitReason,
    },
    /// Terminated on purpose — explicit terminate or supervisor drop.
    Terminated,
}

/// Why an exited sidecar stayed down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// Exit code zero: the child finished — restarting finished work is a loop.
    CleanExit,
    /// The policy was `Never`.
    PolicyNever,
    /// The restart ceiling was reached — a fault to surface, not a loop to run.
    RestartsExhausted,
}

/// One sidecar's shared truth, written by the monitor, read by inspection.
#[derive(Debug)]
pub(crate) struct SidecarShared {
    spec: SidecarSpec,
    status: Mutex<SidecarStatus>,
    restarts: AtomicU64,
    slot: IncarnationSlot,
    /// The sidecar's own job object — held here because terminating the
    /// sidecar is taking it out and dropping it, which is the kernel killing
    /// the whole tree. `None` only ever means terminated.
    job: Mutex<Option<SidecarJob>>,
}

/// The owner's handle onto a spawned sidecar: the stable byte path plus the
/// inspection surface.
#[derive(Debug)]
pub struct SidecarHandle {
    id: SidecarId,
    /// The write half of the transport — stable across restarts, honest
    /// about the death window.
    pub stdin: SidecarStdin,
    /// The read half of the transport — a pipe fed by the forwarder, stable
    /// across restarts. EOF means terminated, not merely dead.
    pub stdout: io::PipeReader,
    shared: Arc<SidecarShared>,
}

impl SidecarHandle {
    /// The sidecar's id.
    #[must_use]
    pub fn id(&self) -> SidecarId {
        self.id
    }

    /// The current inspectable truth.
    #[must_use]
    pub fn status(&self) -> SidecarStatus {
        status_of(&self.shared)
    }

    /// How many times this sidecar has been restarted.
    #[must_use]
    pub fn restart_count(&self) -> u64 {
        self.shared.restarts.load(AtomicOrdering::Relaxed)
    }

    /// Consumes the handle and yields the stable transport ends.
    ///
    /// The composition point for a wire layer — `StreamingSidecar` — that owns
    /// the request/response protocol while the supervisor owns the process:
    /// the pipes survive restart, so a protocol built on them survives it too.
    /// The sidecar itself does not stop — supervision lives in the `Supervisor`
    /// that spawned it and ends with it — but the inspect-and-terminate
    /// interface this handle was is gone with it. A caller that may still want
    /// to terminate keeps the [`SidecarId`] and asks the supervisor.
    #[must_use]
    pub fn into_transport(self) -> (SidecarStdin, io::PipeReader) {
        (self.stdin, self.stdout)
    }
}

fn status_of(shared: &SidecarShared) -> SidecarStatus {
    shared
        .status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The supervisor: owns the job object, the registry, and every monitor
/// thread. Dropping it terminates every sidecar — and even if the drop never
/// runs, the job object closes with the process and the OS does the rest.
pub struct Supervisor {
    inner: Arc<SupervisorInner>,
}

impl Clone for Supervisor {
    /// Another handle on the same supervised set — for a seam that must carry
    /// the supervisor into a closure without owning the runtime's field. The
    /// terminate-on-drop guarantee is the inner Arc's: every sidecar dies when
    /// the *last* handle does, never when a clone does.
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor").finish_non_exhaustive()
    }
}

pub(crate) struct SupervisorInner {
    registry: Mutex<HashMap<SidecarId, Arc<SidecarShared>>>,
    next_id: AtomicU64,
    monitors: Mutex<Vec<JoinHandle<()>>>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    /// Creates the supervisor. Infallible because it allocates nothing the OS
    /// can refuse — the job objects are per-sidecar, created at spawn, where
    /// their failure is that spawn's error.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SupervisorInner {
                registry: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(0),
                monitors: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Spawns and supervises a sidecar. The answer is the owner's handle:
    /// the stable byte path, plus inspection and termination.
    ///
    /// # Errors
    ///
    /// The spawn itself, or the job assignment — a child that cannot be
    /// assigned is killed before this returns, because an unassignable child
    /// is precisely a child the guarantee cannot cover.
    pub fn spawn(&self, spec: SidecarSpec) -> io::Result<SidecarHandle> {
        let id = SidecarId(self.inner.next_id.fetch_add(1, AtomicOrdering::Relaxed) + 1);
        let job = SidecarJob::new()?;
        let mut child = spawn_child(&spec)?;
        job.assign(&child).inspect_err(|_| {
            // The guarantee cannot cover this child — it must not survive us.
            let _ = child.kill();
        })?;

        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            // unreachable in practice: piped at construction — but a missing pipe
            // is a spawn failure, not a panic.
            let _ = child.kill();
            return Err(io::Error::other(
                "the spawned child's pipes were not available",
            ));
        };
        let pid = child.id();

        let slot = IncarnationSlot::new(stdin, stdout);
        let shared = Arc::new(SidecarShared {
            spec,
            status: Mutex::new(SidecarStatus::Running {
                pid,
                since: Instant::now(),
            }),
            restarts: AtomicU64::new(0),
            slot: slot.clone(),
            job: Mutex::new(Some(job)),
        });
        self.inner
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, Arc::clone(&shared));

        // The stable byte path: owner-facing pipe ends plus the forwarder
        // thread that keeps them fed across incarnations.
        let (app_reader, forwarder_writer) = io::pipe()?;
        let forwarder = spawn_stdout_forwarder(slot, forwarder_writer, Arc::clone(&shared));
        self.inner
            .monitors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(forwarder);

        let monitor = spawn_monitor(child, Arc::clone(&shared));
        self.inner
            .monitors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(monitor);

        Ok(SidecarHandle {
            id,
            stdin: SidecarStdin::new(shared.slot.clone(), Arc::clone(&shared)),
            stdout: app_reader,
            shared,
        })
    }

    /// Terminates a sidecar explicitly: marked so the monitor never restarts
    /// it, then killed. Answers whether the sidecar existed.
    pub fn terminate(&self, id: SidecarId) -> bool {
        let shared = {
            self.inner
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .cloned()
        };
        let Some(shared) = shared else {
            return false;
        };
        terminate_shared(&shared);
        true
    }

    /// The inspectable truth for one sidecar.
    #[must_use]
    pub fn status(&self, id: SidecarId) -> Option<SidecarStatus> {
        self.inner
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .map(|shared| status_of(shared))
    }

    /// Terminates every sidecar — the runtime-teardown path. Drop calls this;
    /// calling it explicitly is the shape a careful shutdown takes when it
    /// wants the children down before other teardown happens.
    pub fn terminate_all(&self) {
        let ids: Vec<SidecarId> = self
            .inner
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        for id in ids {
            self.terminate(id);
        }
    }
}

/// Ends one sidecar: record the death, close the pipes, drop the job.
///
/// The kill itself is the kernel's: taking the job out and dropping it
/// closes the handle, and `KILL_ON_JOB_CLOSE` ends the whole tree —
/// grandchildren included, which a direct `TerminateProcess` never reaches.
fn terminate_shared(shared: &Arc<SidecarShared>) {
    {
        *shared
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SidecarStatus::Terminated;
    }
    // Closing the incarnation pipes is what wakes the forwarder and any
    // writer blocked mid-write — the death is real, not just recorded.
    shared.slot.close();
    let job = shared
        .job
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    drop(job);
    // Non-Windows has no job object; the pid kill is the documented
    // best-effort fallback.
    #[cfg(not(target_os = "windows"))]
    terminate_pid(current_pid(shared));
}

impl Drop for SupervisorInner {
    /// The last handle on the supervisor going away takes every sidecar with
    /// it — the never-orphan guarantee is the Arc's last drop, so a cloned
    /// `Supervisor` leaving scope early cannot kill a sibling's children.
    fn drop(&mut self) {
        let children: Vec<Arc<SidecarShared>> = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for shared in children {
            terminate_shared(&shared);
        }
        // The monitors were blocked in `wait()` on children that are now
        // dead — they exit on their own momentarily. Joining them here keeps
        // shutdown quiet rather than leaving threads running mid-drop.
        let monitors: Vec<JoinHandle<()>> = std::mem::take(
            &mut *self
                .monitors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for monitor in monitors {
            let _ = monitor.join();
        }
    }
}

/// Spawns one incarnation of the spec — stdin/stdout piped, stderr silent.
fn spawn_child(spec: &SidecarSpec) -> io::Result<Child> {
    let mut command = Command::new(&spec.program);
    if spec.env_clear {
        command.env_clear();
    }
    command
        .args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(dir) = &spec.working_dir {
        command.current_dir(dir);
    }
    command.spawn()
}

/// The pid of the current incarnation, when it is running.
#[cfg(not(target_os = "windows"))]
fn current_pid(shared: &SidecarShared) -> Option<u32> {
    match &*shared
        .status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        SidecarStatus::Running { pid, .. } => Some(*pid),
        _ => None,
    }
}

/// The monitor: one thread per sidecar. Blocks in `wait()`, records the exit
/// truthfully, applies the restart policy, and owns the respawn — including
/// reassigning the new child to the job and republishing its pipes, because
/// the guarantee and the transport both apply to every incarnation.
#[allow(
    clippy::too_many_lines,
    reason = "the monitor loop is one linear lifecycle: wait, classify, restart or end"
)]
fn spawn_monitor(mut child: Child, shared: Arc<SidecarShared>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            let exit = child.wait();
            // Re-read the status the world sees now: an explicit terminate that
            // landed while we waited is final, whatever the exit says.
            if is_terminated(&shared) {
                return;
            }
            let code = exit.ok().and_then(|status| status.code());
            let restart = match (shared.spec.restart, code) {
                (
                    RestartPolicy::OnFailure {
                        max_restarts,
                        base_backoff,
                    },
                    Some(nonzero),
                ) if nonzero != 0 => {
                    let done = shared.restarts.load(AtomicOrdering::Relaxed);
                    (done < u64::from(max_restarts)).then_some((done + 1, base_backoff, nonzero))
                }
                _ => None,
            };
            let Some((attempt, base_backoff, nonzero)) = restart else {
                let reason = match (shared.spec.restart, code) {
                    (_, Some(0)) => ExitReason::CleanExit,
                    (RestartPolicy::Never, _) => ExitReason::PolicyNever,
                    _ => ExitReason::RestartsExhausted,
                };
                *shared
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SidecarStatus::Exited { code, reason };
                return;
            };
            {
                *shared
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SidecarStatus::Restarting {
                        attempt: attempt as u32,
                        last_exit: nonzero,
                    };
            }
            // Exponential backoff, capped — a crash-looping child is visible in
            // the Restarting state for the whole backoff, never a hammer.
            let backoff = base_backoff
                .checked_mul(2u32.saturating_pow((attempt as u32).saturating_sub(1)))
                .unwrap_or(Duration::from_secs(60))
                .min(Duration::from_secs(60));
            std::thread::sleep(backoff);
            // The terminate that landed during the backoff is final too.
            if is_terminated(&shared) {
                return;
            }
            let Ok(mut respawned) = spawn_child(&shared.spec) else {
                *shared
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = SidecarStatus::Exited {
                    code,
                    reason: ExitReason::RestartsExhausted,
                };
                return;
            };
            let assigned = shared
                .job
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .is_some_and(|job| job.assign(&respawned).is_ok());
            if !assigned {
                let _ = respawned.kill();
                *shared
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = SidecarStatus::Exited {
                    code,
                    reason: ExitReason::RestartsExhausted,
                };
                return;
            }
            let (Some(new_stdin), Some(new_stdout)) =
                (respawned.stdin.take(), respawned.stdout.take())
            else {
                let _ = respawned.kill();
                *shared
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = SidecarStatus::Exited {
                    code,
                    reason: ExitReason::RestartsExhausted,
                };
                return;
            };
            shared.restarts.store(attempt, AtomicOrdering::Relaxed);
            // Publish the pipes BEFORE the status says running: a write that lands
            // in the window between the two would otherwise go to the dead
            // incarnation's pipe and vanish — the race the wiring test exists to
            // catch. Status flips only when the transport is already the new child.
            shared.slot.publish(new_stdin, new_stdout);
            {
                *shared
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = SidecarStatus::Running {
                    pid: respawned.id(),
                    since: Instant::now(),
                };
            }
            child = respawned;
        }
    })
}

fn is_terminated(shared: &SidecarShared) -> bool {
    matches!(
        *shared
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        SidecarStatus::Terminated
    )
}

/// Terminates a process by id — the unix fallback, because unix has no job
/// object in this crate. `None` means nothing was running.
#[cfg(not(target_os = "windows"))]
fn terminate_pid(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
}
