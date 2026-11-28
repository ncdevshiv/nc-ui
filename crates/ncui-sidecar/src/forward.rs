//! The transport half: stable pipe ends over a reincarnating child.
//!
//! The shape in one paragraph: the slot holds the current incarnation's
//! pipes under a sequence number. The owner's writes go through the slot to
//! whatever child is current. The forwarder thread takes each incarnation's
//! stdout as it appears and copies it into the stable pipe the owner reads —
//! so the owner never re-opens anything, and the death window is the only
//! time a write can fail, which it does loudly.

use std::io::{self, Read, Write};
use std::process::{ChildStdin, ChildStdout};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::SidecarShared;

/// The current incarnation's pipes, plus the sequence that tells the
/// forwarder a new child has stood up.
#[derive(Debug, Clone)]
pub(crate) struct IncarnationSlot {
    inner: Arc<(Mutex<SlotInner>, Condvar)>,
}

#[derive(Debug)]
struct SlotInner {
    seq: u64,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    closed: bool,
}

impl IncarnationSlot {
    pub(crate) fn new(stdin: ChildStdin, stdout: ChildStdout) -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(SlotInner {
                    seq: 1,
                    stdin: Some(stdin),
                    stdout: Some(stdout),
                    closed: false,
                }),
                Condvar::new(),
            )),
        }
    }

    /// Publishes a new incarnation's pipes — the restart path. The sequence
    /// bump is what wakes the forwarder to pick up the new stdout.
    pub(crate) fn publish(&self, stdin: ChildStdin, stdout: ChildStdout) {
        let (mutex, condvar) = &*self.inner;
        {
            let mut inner = mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner.stdin = Some(stdin);
            inner.stdout = Some(stdout);
            inner.seq += 1;
        }
        condvar.notify_all();
    }

    /// Closes the slot — the terminate path. The owner's writes fail from
    /// here on, and the forwarder exits once the dying child's pipe empties.
    pub(crate) fn close(&self) {
        let (mutex, condvar) = &*self.inner;
        {
            let mut inner = mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner.closed = true;
            inner.stdin = None;
            inner.stdout = None;
        }
        condvar.notify_all();
    }
}

/// The owner's write half: writes to whatever incarnation is current.
///
/// A write in the death window — the child down, the restart not yet up —
/// fails with `BrokenPipe` rather than pretending, and a write that the
/// child itself refuses surfaces the child's own error unchanged.
#[derive(Debug)]
pub struct SidecarStdin {
    slot: IncarnationSlot,
    // The shared state is held so the writer can tell terminated apart from
    // merely dead — both fail the write, but terminated fails it permanently.
    #[allow(
        dead_code,
        reason = "the terminated/distinction belongs to the error path; wired as inspection matures"
    )]
    shared: Arc<SidecarShared>,
}

impl SidecarStdin {
    pub(crate) fn new(slot: IncarnationSlot, shared: Arc<SidecarShared>) -> Self {
        Self { slot, shared }
    }
}

impl Write for SidecarStdin {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let (mutex, _) = &*self.slot.inner;
        let mut inner = mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(stdin) = inner.stdin.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the sidecar is down — the death window before a restart, or terminated",
            ));
        };
        stdin.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        let (mutex, _) = &*self.slot.inner;
        let mut inner = mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(stdin) = inner.stdin.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the sidecar is down — the death window before a restart, or terminated",
            ));
        };
        stdin.flush()
    }
}

/// The forwarder: copies each incarnation's stdout into the stable pipe the
/// owner reads. Follows sequence bumps; exits when the slot closes and the
/// last pipe has drained — which is what makes owner-side EOF mean
/// terminated, never merely dead.
pub(crate) fn spawn_stdout_forwarder(
    slot: IncarnationSlot,
    mut writer: io::PipeWriter,
    shared: Arc<SidecarShared>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut seen_seq = 0u64;
        let mut current: Option<ChildStdout> = None;
        loop {
            // Pick up the newest incarnation's stdout — waiting when nothing
            // new has arrived, unless the slot is closed and nothing remains.
            if current.is_none() {
                let (mutex, condvar) = &*slot.inner;
                let mut inner = mutex
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                loop {
                    if inner.seq != seen_seq && inner.stdout.is_some() {
                        seen_seq = inner.seq;
                        current = inner.stdout.take();
                        break;
                    }
                    if inner.closed {
                        // Nothing more will ever come — the drain is finished
                        // by definition, because there is no current pipe.
                        drop(writer);
                        return;
                    }
                    inner = condvar
                        .wait(inner)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
            let Some(stdout) = current.as_mut() else {
                continue;
            };
            let mut buffer = [0u8; 8192];
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    // EOF: this incarnation is done speaking. If the slot is
                    // closed the sidecar is terminated and the owner's pipe
                    // must EOF too; otherwise the restart will publish.
                    current = None;
                    let terminated = matches!(
                        *shared
                            .status
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                        crate::SidecarStatus::Terminated
                    );
                    if terminated {
                        drop(writer);
                        return;
                    }
                }
                Ok(read) => {
                    if writer.write_all(&buffer[..read]).is_err() {
                        // The owner closed its read end — there is no one to
                        // forward for, and hanging onto the child would be
                        // the orphan shape this crate exists to prevent.
                        return;
                    }
                }
                Err(_) => {
                    current = None;
                }
            }
        }
    })
}
