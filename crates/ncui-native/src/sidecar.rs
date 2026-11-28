//! A long-lived sidecar: one program, many calls, one line of JSON each way.
//!
//! The one-shot sidecar (`process.rs`) starts a program per call and reads one answer —
//! the right shape for a stateless tool, and the wrong shape for a backend that pays a
//! start-up cost on every call: an interpreter loading a model, a database connection, a
//! warm index. This module is the other shape. The program is started once, kept running,
//! and answers one newline-delimited JSON line per call over the same standard streams,
//! so the expensive part happens once and every call after it is warm.
//!
//! # The wire contract
//!
//! A call writes `{"id": N, "payload": …}` as one line and reads lines until one carries
//! that `id`. An answer is `{"id": N, "result": …}` or `{"id": N, "error": {"message": …}}`;
//! a line without an `id` is not an answer to anything and is dropped rather than
//! misrouted. The contract is the NDJSON dialect log pipelines speak, so a backend can be
//! developed against `jq` and a file before it is wired to a page.
//!
//! # Containment is the same suspicion, held for longer
//!
//! A program that stays running is still a program the runtime started on a page's
//! behalf: the environment is scrubbed, the working directory is pinned to the
//! application, there is no shell, output is bounded, every call is timed, and the
//! process group (Unix) or the kill path (Windows) is the runner's. The difference from a
//! one-shot run is state, not scrutiny.
//!
//! # What a dropped line and a dead pipe have in common
//!
//! Every failure is named: a program that has exited is `not_running`, an answer that
//! does not arrive in time is a `timeout`, an answer that reports an error carries that
//! error's message, and a line that breaks the contract is a `transport` failure. Nothing
//! is swallowed, because a page waiting on a backend is owed an answer or a reason.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind as IoErrorKind, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::process::{ProcessError, scrubbed_environment};

/// The longest one answer line may be.
///
/// A bounded answer is what keeps a misbehaving backend from filling this process's
/// memory with a line that never ends. It is generous: a page's answer is a JSON
/// document, not a file.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// The most calls awaiting an answer at once.
///
/// A page calls one at a time through the seam; a bound here is a backstop for a caller
/// that pipelines, so pending calls are counted rather than unbounded.
pub const MAX_PENDING_CALLS: usize = 64;

/// Why a call to a long-lived sidecar did not produce an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarError {
    /// The program is not running: it was never started, has exited, or was stopped.
    NotRunning {
        /// What the program last wrote to standard error, when it wrote anything.
        stderr: String,
    },
    /// The answer did not arrive within the call's time limit.
    Timeout {
        /// How long the call waited.
        elapsed_ms: u64,
    },
    /// The program answered with an error of its own.
    Reported {
        /// The message the program's answer carried.
        message: String,
    },
    /// The program broke the line contract, or its pipes failed.
    Transport {
        /// What was wrong.
        reason: String,
    },
}

impl SidecarError {
    /// The stable wire name, used in the seam's refusals.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotRunning { .. } => "sidecar_not_running",
            Self::Timeout { .. } => "sidecar_timeout",
            Self::Reported { .. } => "sidecar_error",
            Self::Transport { .. } => "sidecar_transport",
        }
    }
}

impl core::fmt::Display for SidecarError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotRunning { stderr } => {
                write!(f, "the sidecar is not running: {stderr}")
            }
            Self::Timeout { elapsed_ms } => {
                write!(f, "the sidecar did not answer within {elapsed_ms}ms")
            }
            Self::Reported { message } => write!(f, "the sidecar reported an error: {message}"),
            Self::Transport { reason } => write!(f, "the sidecar's transport failed: {reason}"),
        }
    }
}

/// What the reader thread routes to a waiting call.
type Answer = Result<Value, SidecarError>;

/// One long-lived sidecar program.
///
/// Cloning is not supported on purpose: a sidecar is one program, and a caller that wants
/// two shares this one, because the state that makes it worth keeping alive is the state
/// one program holds.
pub struct StreamingSidecar {
    stdin: Mutex<Box<dyn Write + Send>>,
    /// The process, when this sidecar spawned it. `None` when a supervisor owns
    /// the lifecycle — the transport stays alive across incarnations, so the
    /// sidecar's liveness question is answered by the transport then.
    child: Mutex<Option<Child>>,
    /// On Windows, the job that holds the program and everything it starts: dropping it
    /// is the kill switch that reaches a detached grandchild a direct kill cannot.
    /// `None` when a supervisor owns the process — its own job is the kill switch.
    #[cfg(windows)]
    job: Option<win32job::Job>,
    /// Whether the byte path is open. The reader's EOF is the only writer: for
    /// a self-spawned sidecar it means the program is gone; for a supervised
    /// one it means the supervisor is gone — either way, calls stop being
    /// answerable and the truth is the same.
    transport_open: Arc<AtomicBool>,
    pending: Arc<Mutex<HashMap<u64, Sender<Answer>>>>,
    next_id: AtomicU64,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    _reader: JoinHandle<()>,
    _stderr: Option<JoinHandle<()>>,
    label: String,
}

impl core::fmt::Debug for StreamingSidecar {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The guarded fields are the child's pipes and the call routing: they say nothing a
        // caller can act on, and the ones a caller asks about are here.
        f.debug_struct("StreamingSidecar")
            .field("label", &self.label)
            .field("alive", &self.is_alive())
            .finish_non_exhaustive()
    }
}

impl StreamingSidecar {
    /// Starts the program the runner resolves, and keeps it running.
    ///
    /// The runner supplies the containment: the program is resolved inside the
    /// application (a bare name resolves on `PATH`, the same as a one-shot sidecar), the
    /// working directory is the application's, and the environment is scrubbed.
    ///
    /// # Errors
    ///
    /// [`ProcessError::NotFound`] for a program that is not there, and
    /// [`ProcessError::SpawnFailed`] when the operating system refuses to start it.
    pub fn spawn(
        runner: &crate::process::ProcessRunner,
        program: &str,
        args: &[String],
    ) -> Result<Self, ProcessError> {
        let resolved = runner.resolve_program(program)?;
        let mut command = Command::new(&resolved);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .envs(scrubbed_environment())
            .current_dir(runner.root());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(0x0800_0000);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|error| match error.kind() {
            IoErrorKind::NotFound => ProcessError::NotFound {
                program: program.to_owned(),
            },
            _ => ProcessError::SpawnFailed {
                program: program.to_owned(),
                reason: error.to_string(),
            },
        })?;
        // The same containment the one-shot runner has: the program and everything it
        // starts end together, even a descendant that detaches from its parent.
        #[cfg(windows)]
        let job = crate::process::assign_job(&mut child);

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProcessError::SpawnFailed {
                program: program.to_owned(),
                reason: "the child's standard output could not be read".to_owned(),
            })?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ProcessError::SpawnFailed {
                program: program.to_owned(),
                reason: "the child's standard error could not be read".to_owned(),
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProcessError::SpawnFailed {
                program: program.to_owned(),
                reason: "the child's standard input could not be written".to_owned(),
            })?;

        let pending: Arc<Mutex<HashMap<u64, Sender<Answer>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let stderr_tail: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

        let transport_open = Arc::new(AtomicBool::new(true));
        let reader = spawn_reader(
            stdout,
            Arc::clone(&pending),
            Arc::clone(&stderr_tail),
            Arc::clone(&transport_open),
        )
        .map_err(|error| ProcessError::SpawnFailed {
            program: program.to_owned(),
            reason: format!("the sidecar's reader thread did not start: {error}"),
        })?;
        let stderr_drain =
            spawn_stderr_drain(stderr, Arc::clone(&stderr_tail)).map_err(|error| {
                ProcessError::SpawnFailed {
                    program: program.to_owned(),
                    reason: format!("the sidecar's error thread did not start: {error}"),
                }
            })?;

        Ok(Self {
            stdin: Mutex::new(Box::new(stdin)),
            child: Mutex::new(Some(child)),
            #[cfg(windows)]
            job,
            transport_open,
            pending,
            next_id: AtomicU64::new(1),
            stderr_tail,
            _reader: reader,
            _stderr: Some(stderr_drain),
            label: program.to_owned(),
        })
    }

    /// Wraps an externally-owned transport in the request/response wire.
    ///
    /// This is the composition point for a supervised sidecar: the caller —
    /// `ncui-sidecar`'s `Supervisor` — owns the process, its job object and its
    /// restart policy, and hands over the *stable* pipe ends. The supervisor's
    /// transport survives restart: a crash-and-respawn is invisible here
    /// except that calls pending at the crash time out, which is the bounded,
    /// honest way to say "the incarnation that owed you an answer is gone".
    ///
    /// There is no standard error to drain: the supervisor decides the
    /// child's stderr posture, and today that is silence — diagnostics for a
    /// supervised backend come through the supervisor's status, not a tail.
    ///
    /// # Errors
    ///
    /// [`ProcessError::SpawnFailed`] when the reader thread cannot start — the
    /// pipes are returned to the caller on that path by drop, untouched.
    pub fn from_pipes(
        stdin: impl Write + Send + 'static,
        stdout: impl Read + Send + 'static,
        label: String,
    ) -> Result<Self, ProcessError> {
        let pending: Arc<Mutex<HashMap<u64, Sender<Answer>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let stderr_tail: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let transport_open = Arc::new(AtomicBool::new(true));
        let reader = spawn_reader(
            stdout,
            Arc::clone(&pending),
            Arc::clone(&stderr_tail),
            Arc::clone(&transport_open),
        )
        .map_err(|error| ProcessError::SpawnFailed {
            program: label.clone(),
            reason: format!("the sidecar's reader thread did not start: {error}"),
        })?;
        Ok(Self {
            stdin: Mutex::new(Box::new(stdin)),
            child: Mutex::new(None),
            #[cfg(windows)]
            job: None,
            transport_open,
            pending,
            next_id: AtomicU64::new(1),
            stderr_tail,
            _reader: reader,
            _stderr: None,
            label,
        })
    }

    /// Asks the program one question and waits for its answer.
    ///
    /// The payload is written as one JSON line; the answer is the first line that names
    /// this call's `id`. A program that reports an error in its answer is an error here —
    /// the page sees the message the backend wrote, which is the backend speaking for
    /// itself.
    ///
    /// # Errors
    ///
    /// [`SidecarError::NotRunning`] when the program has exited, [`SidecarError::Timeout`]
    /// when no answer arrives in time, [`SidecarError::Reported`] when the program's
    /// answer carries an error, and [`SidecarError::Transport`] when a line breaks the
    /// contract or a pipe fails.
    pub fn call(&self, payload: &Value, timeout: Duration) -> Result<Value, SidecarError> {
        if !self.is_alive() {
            return Err(SidecarError::NotRunning {
                stderr: self.stderr_text(),
            });
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = std::sync::mpsc::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.len() >= MAX_PENDING_CALLS {
                return Err(SidecarError::Transport {
                    reason: format!("more than {MAX_PENDING_CALLS} calls are already waiting"),
                });
            }
            pending.insert(id, sender);
        }
        if let Err(error) = self.write_request(id, payload) {
            self.abandon(id);
            return Err(error);
        }
        match Self::await_answer(&receiver, timeout) {
            Ok(value) => Ok(value),
            Err(error) => {
                self.abandon(id);
                Err(error)
            }
        }
    }

    /// Whether the program is still running.
    ///
    /// For a supervised sidecar — one built with [`Self::from_pipes`] — the
    /// process is the supervisor's business and may have been restarted since
    /// the last call; what this answers then is whether the *transport* is
    /// open, which is the question a caller of this wire is actually asking.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match child.as_mut() {
            Some(child) => child.try_wait().is_ok_and(|status| status.is_none()),
            None => self.transport_open.load(Ordering::Acquire),
        }
    }

    /// Stops the program and fails every call waiting on it.
    ///
    /// The waiting calls are failed here rather than left to the reader's EOF, so a stop
    /// is an answer to everything it cut short, not a silence.
    pub fn stop(&mut self) {
        // Closing the job first ends the whole tree — including a detached grandchild a
        // direct kill cannot reach — and the direct kill after it is the fallback.
        #[cfg(windows)]
        {
            self.job = None;
        }
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(child) = child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.transport_open.store(false, Ordering::Release);
        drop(child);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(SidecarError::Transport {
                reason: "the sidecar was stopped".to_owned(),
            }));
        }
    }

    /// The last bytes the program wrote to standard error, as text.
    ///
    /// A backend that dies usually says why on standard error; the tail is what a
    /// `not_running` refusal carries, so the page's error names the real cause.
    #[must_use]
    pub fn stderr_text(&self) -> String {
        let tail = self
            .stderr_tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&tail).trim().to_owned()
    }

    /// Writes one request line to the program's standard input.
    fn write_request(&self, id: u64, payload: &Value) -> Result<(), SidecarError> {
        let line = serde_json::to_string(&serde_json::json!({ "id": id, "payload": payload }))
            .map_err(|error| SidecarError::Transport {
                reason: format!("the payload cannot be serialised: {error}"),
            })?;
        let mut stdin = self
            .stdin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush())
            .map_err(|error| SidecarError::NotRunning {
                stderr: format!("the sidecar's input pipe failed: {error}"),
            })
    }

    /// Waits for the answer routed to this call, until the time limit.
    fn await_answer(receiver: &Receiver<Answer>, timeout: Duration) -> Result<Value, SidecarError> {
        let deadline = Instant::now() + timeout;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SidecarError::Timeout {
                elapsed_ms: timeout.as_millis() as u64,
            });
        }
        match receiver.recv_timeout(remaining) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(error),
            Err(RecvTimeoutError::Timeout) => Err(SidecarError::Timeout {
                elapsed_ms: timeout.as_millis() as u64,
            }),
            Err(RecvTimeoutError::Disconnected) => Err(SidecarError::Transport {
                reason: "the sidecar's reader stopped before the answer arrived".to_owned(),
            }),
        }
    }

    /// Stops waiting for an answer that will no longer be received.
    fn abandon(&self, id: u64) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }
}

/// Locks a sidecar's shared state, mending a panic that poisoned it.
fn lock<T>(guard: &Mutex<T>) -> MutexGuard<'_, T> {
    guard
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for StreamingSidecar {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            self.job = None;
        }
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(child) = child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The most standard-error text a dying sidecar's tail keeps.
const STDERR_TAIL_BYTES: usize = 4096;

/// Reads the program's answers and routes each to the call waiting on it.
fn spawn_reader(
    stdout: impl Read + Send + 'static,
    pending: Arc<Mutex<HashMap<u64, Sender<Answer>>>>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    transport_open: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("ncui-sidecar-stdout".to_owned())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            let fail_all = |pending: &Arc<Mutex<HashMap<u64, Sender<Answer>>>>,
                            reason: SidecarError| {
                let mut waiting = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for (_, sender) in waiting.drain() {
                    let _ = sender.send(Err(reason.clone()));
                }
            };
            loop {
                match read_line_bounded(&mut reader, MAX_LINE_BYTES) {
                    Ok(Some(line)) => {
                        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
                            // A line that is not JSON is not an answer to anything, so it
                            // is dropped rather than misrouted to whichever call is oldest.
                            // The call it was meant for times out, which is the bounded
                            // way to say "no answer came"; one broken line does not end
                            // the program or the calls that follow it.
                            continue;
                        };
                        let id = message.get("id").and_then(Value::as_u64);
                        let Some(id) = id else {
                            // A line without an id is not an answer to anything. It is
                            // dropped rather than misrouted to whichever call is oldest.
                            continue;
                        };
                        let answer = if let Some(error) = message.get("error") {
                            let message = error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("the sidecar reported an error without a message")
                                .to_owned();
                            Err(SidecarError::Reported { message })
                        } else {
                            Ok(message.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let sender = pending
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&id);
                        if let Some(sender) = sender {
                            let _ = sender.send(answer);
                        }
                        // An answer for a call that gave up (a timeout) is dropped with it.
                    }
                    Ok(None) => {
                        // The program closed its standard output: it is gone, and every
                        // call still waiting is owed the reason.
                        let tail = stderr_tail
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let stderr = String::from_utf8_lossy(&tail).trim().to_owned();
                        drop(tail);
                        let reason = if stderr.is_empty() {
                            "the sidecar exited without answering".to_owned()
                        } else {
                            format!("the sidecar exited: {stderr}")
                        };
                        fail_all(&pending, SidecarError::Transport { reason });
                        transport_open.store(false, Ordering::Release);
                        return;
                    }
                    Err(error) => {
                        fail_all(
                            &pending,
                            SidecarError::Transport {
                                reason: format!("the sidecar's output could not be read: {error}"),
                            },
                        );
                        transport_open.store(false, Ordering::Release);
                        return;
                    }
                }
            }
        })
}

/// Drains standard error into a bounded tail, so a chatty program cannot fill memory and
/// a dying one still leaves its last words.
fn spawn_stderr_drain(
    stderr: std::process::ChildStderr,
    tail: Arc<Mutex<Vec<u8>>>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("ncui-sidecar-stderr".to_owned())
        .spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buffer = [0u8; 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => {
                        let mut kept = lock(&tail);
                        kept.extend_from_slice(&buffer[..read]);
                        if kept.len() > STDERR_TAIL_BYTES {
                            let excess = kept.len() - STDERR_TAIL_BYTES;
                            kept.drain(..excess);
                        }
                    }
                }
            }
        })
}

/// Reads one newline-terminated line, refusing a line longer than `max`.
fn read_line_bounded(reader: &mut impl BufRead, max: usize) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            // End of file: a final line without a newline is still a line, and an empty
            // buffer with nothing read is the end.
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8_lossy(&line).into_owned()))
            };
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(index) => {
                line.extend_from_slice(&available[..index]);
                reader.consume(index + 1);
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            None => {
                line.extend_from_slice(available);
                let read = available.len();
                reader.consume(read);
                if line.len() > max {
                    return Err(std::io::Error::new(
                        IoErrorKind::InvalidData,
                        format!("the sidecar wrote a line longer than {max} bytes"),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use serde_json::json;

    use super::{SidecarError, StreamingSidecar};
    use crate::process::{ProcessError, ProcessRunner};

    /// The interpreter the sidecar tests run their script with, named as a bare program
    /// so the runner resolves it the way it resolves any tool the application does not
    /// ship. Python stands in for "some interpreter with start-up cost".
    fn interpreter() -> String {
        if cfg!(windows) {
            "python".to_owned()
        } else {
            "python3".to_owned()
        }
    }

    /// A scratch directory the runner is rooted at, canonicalized the way the runner
    /// canonicalizes its root, so a script written here is found by its file name.
    fn scratch(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("ncui-sidecar-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("the scratch directory is creatable");
        root.canonicalize().unwrap_or(root)
    }

    /// Writes a script and returns its file name, for the runner to resolve in the root.
    fn write_script(root: &std::path::Path, name: &str, source: &str) -> String {
        std::fs::write(root.join(name), source).expect("the script is written");
        name.to_owned()
    }

    /// A sidecar that counts its calls in its own memory: the proof that one program is
    /// answering repeatedly, rather than a fresh one per call.
    const COUNTER: &str = r#"
import json, sys
count = 0
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    count += 1
    if request.get("payload", {}).get("fail"):
        sys.stdout.write(json.dumps({"id": request["id"], "error": {"message": "asked to fail"}}) + "\n")
    else:
        sys.stdout.write(json.dumps({"id": request["id"], "result": {"count": count}}) + "\n")
    sys.stdout.flush()
"#;

    #[test]
    fn one_program_answers_many_calls_and_keeps_its_state() {
        let root = scratch("counting");
        let script = write_script(&root, "counter.py", COUNTER);
        let runner = ProcessRunner::new(&root).expect("a runner");
        let mut sidecar =
            StreamingSidecar::spawn(&runner, &interpreter(), &[script]).expect("the sidecar runs");

        assert!(sidecar.is_alive(), "the program is running");
        for expected in [1, 2, 3] {
            let answer = sidecar
                .call(&json!({}), Duration::from_secs(10))
                .expect("the call is answered");
            assert_eq!(answer["count"], expected, "the program remembers its calls");
        }
        sidecar.stop();
    }

    #[test]
    fn a_sidecar_error_reaches_the_caller_and_the_program_survives_it() {
        let root = scratch("error");
        let script = write_script(&root, "counter.py", COUNTER);
        let runner = ProcessRunner::new(&root).expect("a runner");
        let mut sidecar =
            StreamingSidecar::spawn(&runner, &interpreter(), &[script]).expect("the sidecar runs");

        let error = sidecar
            .call(&json!({ "fail": true }), Duration::from_secs(10))
            .expect_err("an error answer is an error");
        assert_eq!(
            error,
            SidecarError::Reported {
                message: "asked to fail".to_owned()
            },
            "the backend's own message is carried"
        );
        // The program is still there, and still counting: an error answer is an answer,
        // not a death.
        let answer = sidecar
            .call(&json!({}), Duration::from_secs(10))
            .expect("the next call is answered");
        assert_eq!(answer["count"], 2);
        sidecar.stop();
    }

    #[test]
    fn a_call_that_outlives_its_time_limit_is_a_timeout() {
        let root = scratch("timeout");
        let script = write_script(
            &root,
            "sleeper.py",
            r#"
import json, sys, time
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    time.sleep(2)
    sys.stdout.write(json.dumps({"id": request["id"], "result": "late"}) + "\n")
    sys.stdout.flush()
"#,
        );
        let runner = ProcessRunner::new(&root).expect("a runner");
        let mut sidecar =
            StreamingSidecar::spawn(&runner, &interpreter(), &[script]).expect("the sidecar runs");

        let error = sidecar
            .call(&json!({}), Duration::from_millis(300))
            .expect_err("a slow answer times out");
        assert!(
            matches!(error, SidecarError::Timeout { .. }),
            "the wait is bounded: {error}"
        );
        sidecar.stop();
    }

    #[test]
    fn a_stopped_sidecar_is_reported_as_not_running() {
        let root = scratch("stopped");
        let script = write_script(&root, "counter.py", COUNTER);
        let runner = ProcessRunner::new(&root).expect("a runner");
        let mut sidecar =
            StreamingSidecar::spawn(&runner, &interpreter(), &[script]).expect("the sidecar runs");
        sidecar.stop();
        assert!(!sidecar.is_alive(), "a stopped program is not alive");
        let error = sidecar
            .call(&json!({}), Duration::from_secs(10))
            .expect_err("nothing answers after a stop");
        assert!(matches!(error, SidecarError::NotRunning { .. }), "{error}");
    }

    #[test]
    fn a_line_that_breaks_the_contract_is_dropped_and_the_program_continues() {
        let root = scratch("contract");
        let script = write_script(
            &root,
            "broken.py",
            r#"
import json, sys
first = True
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    if first:
        sys.stdout.write("this line is not json\n")
        sys.stdout.flush()
        first = False
    else:
        sys.stdout.write(json.dumps({"id": request["id"], "result": "ok"}) + "\n")
        sys.stdout.flush()
"#,
        );
        let runner = ProcessRunner::new(&root).expect("a runner");
        let mut sidecar =
            StreamingSidecar::spawn(&runner, &interpreter(), &[script]).expect("the sidecar runs");

        // The broken line cannot be routed to the call that is waiting for it, so that
        // call learns the bounded way — its own time limit — that no answer came.
        let error = sidecar
            .call(&json!({}), Duration::from_millis(1500))
            .expect_err("the call whose answer was garbage times out");
        assert!(matches!(error, SidecarError::Timeout { .. }), "{error}");
        // One broken line does not end the program or the calls that follow it.
        let answer = sidecar
            .call(&json!({}), Duration::from_secs(10))
            .expect("the next call is answered");
        assert_eq!(answer, json!("ok"));
        sidecar.stop();
    }

    #[test]
    fn a_program_that_is_not_there_is_a_spawn_failure() {
        let root = scratch("missing");
        let runner = ProcessRunner::new(&root).expect("a runner");
        let error = StreamingSidecar::spawn(&runner, "definitely-not-a-program-xyz", &[])
            .expect_err("a missing program is refused");
        assert!(matches!(error, ProcessError::NotFound { .. }), "{error}");
    }
}
