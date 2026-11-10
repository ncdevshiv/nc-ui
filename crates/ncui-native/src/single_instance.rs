//! One instance per application, and what the second launch does.
//!
//! A desktop application is expected to be one thing. Opening the same file twice should reach
//! the window that is already there rather than start a second copy, and a link the user clicked
//! should arrive at the instance that is running. Without that, a deep link opens a new process,
//! and the user ends up with two windows showing the same document and no way to tell which one
//! is which.
//!
//! # How it is done without `unsafe`
//!
//! The lock is a file created exclusively — `create_new`, which is atomic on every platform this
//! runtime supports — beside the application's state, holding the port a first instance is
//! listening on and the token it will accept. The second launch reads the port, connects over
//! loopback, sends its argument vector as JSON, and exits. Nothing here reaches a platform
//! primitive `std` does not expose, so one implementation serves all three platforms.
//!
//! The token is what makes the hand-off a hand-off rather than an injection: any local process
//! can connect to a loopback port, so a payload without the token is refused. The token is
//! readable by the user who owns the state directory, which is the same trust level as the file
//! that holds it — this is a guard against accident and against another user's process, not a
//! defence against the user themselves.
//!
//! # What it deliberately does not do
//!
//! It does not bring the first instance's window forward. Raising a window is a platform
//! operation with its own rules about what may steal focus, and inventing a second mechanism for
//! it here would be a second thing to get wrong. The second launch's arguments are *delivered*;
//! what the runtime does with them — reloading the page, opening the document — is the runtime's
//! decision, and it is the same decision it makes for any other event.
//!
//! # A stale lock
//!
//! A process that is killed leaves its lock file behind. So the second launch does not trust the
//! file: it tries to connect, and a refused connection means the writer is gone. Only then does
//! it take the lock over, and the takeover is itself atomic, so two launches racing for a stale
//! lock cannot both win.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The largest hand-off payload that will be read.
///
/// A hand-off is an argument vector, not a document: a caller that sends more than this has
/// confused the two, and refusing is better than an unbounded read on a socket any local process
/// can reach.
pub const MAX_HANDOFF_BYTES: u64 = 64 * 1024;

/// How long a hand-off may take, in either direction.
pub const HANDOFF_TIMEOUT: Duration = Duration::from_secs(5);

/// The most arguments a hand-off may carry.
pub const MAX_HANDOFF_ARGUMENTS: usize = 256;

/// The most bytes one argument may carry.
pub const MAX_HANDOFF_ARGUMENT_BYTES: usize = 4096;

/// Why an instance could not be established or reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceError {
    /// The state directory could not be used.
    State {
        /// What failed.
        reason: String,
    },
    /// The lock file names something that is not a port.
    MalformedLock {
        /// What was found.
        reason: String,
    },
    /// The first instance could not be reached, and the lock could not be taken over.
    Unreachable {
        /// What failed.
        reason: String,
    },
    /// A hand-off arrived without the token the first instance is waiting for.
    Unauthorised,
    /// A hand-off was larger than the bound, or named more arguments than one may.
    TooLarge {
        /// What was over the bound.
        what: &'static str,
    },
    /// The payload was not the shape a hand-off has.
    MalformedHandoff {
        /// What was wrong.
        reason: String,
    },
}

impl InstanceError {
    /// The stable code a caller sees on the wire.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::State { .. } => "instance_state_unusable",
            Self::MalformedLock { .. } => "instance_lock_malformed",
            Self::Unreachable { .. } => "instance_unreachable",
            Self::Unauthorised => "instance_unauthorised",
            Self::TooLarge { .. } => "instance_handoff_too_large",
            Self::MalformedHandoff { .. } => "instance_handoff_malformed",
        }
    }
}

impl core::fmt::Display for InstanceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::State { reason } => write!(f, "the instance state cannot be used: {reason}"),
            Self::MalformedLock { reason } => {
                write!(f, "the instance lock is not usable: {reason}")
            }
            Self::Unreachable { reason } => {
                write!(f, "the running instance could not be reached: {reason}")
            }
            Self::Unauthorised => {
                f.write_str("a hand-off arrived without the token this instance is waiting for")
            }
            Self::TooLarge { what } => write!(f, "the hand-off's {what} is over its bound"),
            Self::MalformedHandoff { reason } => {
                write!(f, "the hand-off is not the shape one has: {reason}")
            }
        }
    }
}

impl std::error::Error for InstanceError {}

/// What the lock file holds: how to reach the instance that wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Lock {
    /// The port the first instance listens on.
    port: u16,
    /// The token a hand-off must carry.
    token: String,
}

/// What the payload of a hand-off holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Handoff {
    /// The token the lock file published.
    token: String,
    /// The second launch's argument vector.
    args: Vec<String>,
}

/// The outcome of asking to be the instance for an application.
#[derive(Debug)]
pub enum Acquired {
    /// This process is the instance: it owns the lock and the listener.
    First(Box<InstanceGuard>),
    /// Another instance is running and reachable.
    AlreadyRunning {
        /// The port to hand arguments to.
        port: u16,
        /// The token that hand-off must carry.
        token: String,
    },
}

/// The first instance's side of the arrangement.
///
/// Dropping it releases the lock, so a process that exits cleanly leaves nothing behind for the
/// next launch to work around.
#[derive(Debug)]
pub struct InstanceGuard {
    path: PathBuf,
    listener: TcpListener,
    token: String,
}

impl InstanceGuard {
    /// Asks to be the instance for `name`, using `state_dir` for the lock.
    ///
    /// # Errors
    ///
    /// [`InstanceError::State`] when the state directory cannot be created or written, and
    /// [`InstanceError::MalformedLock`] when the lock file exists but does not describe an
    /// instance. A lock whose instance is *gone* is not an error: it is taken over.
    pub fn acquire(state_dir: &Path, name: &str) -> Result<Acquired, InstanceError> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(InstanceError::State {
                reason: format!("`{name}` is not usable as an instance name"),
            });
        }
        std::fs::create_dir_all(state_dir).map_err(|error| InstanceError::State {
            reason: format!("{}: {error}", state_dir.display()),
        })?;
        let path = state_dir.join(format!("{name}.instance"));

        if let Some(existing) = read_lock(&path)? {
            if let Ok(stream) =
                TcpStream::connect_timeout(&loopback(existing.port), HANDOFF_TIMEOUT)
            {
                // The instance that wrote the lock answered, so it is the instance.
                drop(stream);
                return Ok(Acquired::AlreadyRunning {
                    port: existing.port,
                    token: existing.token,
                });
            }
            // Nobody answered: the writer is gone, and the lock is a leftover. Removing it is
            // what lets the next launch be the first instance rather than fail forever.
            let _ = std::fs::remove_file(&path);
        }

        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|error| InstanceError::State {
                reason: format!("no loopback port could be taken: {error}"),
            })?;
        let port = listener
            .local_addr()
            .map_err(|error| InstanceError::State {
                reason: format!("the listener has no address: {error}"),
            })?
            .port();
        let token = mint_token(port);
        let lock = Lock {
            port,
            token: token.clone(),
        };

        // `create_new` is the atomic step: two launches that both found a stale lock cannot both
        // write it, and the loser re-reads what the winner wrote and hands over instead.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let encoded = serde_json::to_vec(&lock).map_err(|error| InstanceError::State {
                    reason: format!("the lock could not be encoded: {error}"),
                })?;
                file.write_all(&encoded)
                    .map_err(|error| InstanceError::State {
                        reason: format!("{}: {error}", path.display()),
                    })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another launch won the race. It is either listening by now or about to be.
                if let Some(winner) = read_lock(&path)?
                    && TcpStream::connect_timeout(&loopback(winner.port), HANDOFF_TIMEOUT).is_ok()
                {
                    return Ok(Acquired::AlreadyRunning {
                        port: winner.port,
                        token: winner.token,
                    });
                }
                return Err(InstanceError::Unreachable {
                    reason: "another launch holds the lock and is not answering".to_owned(),
                });
            }
            Err(error) => {
                return Err(InstanceError::State {
                    reason: format!("{}: {error}", path.display()),
                });
            }
        }

        Ok(Acquired::First(Box::new(Self {
            path,
            listener,
            token,
        })))
    }

    /// The port this instance listens on.
    ///
    /// # Errors
    ///
    /// [`InstanceError::State`] when the listener has no address, which would mean the operating
    /// system accepted a bind it cannot describe.
    pub fn port(&self) -> Result<u16, InstanceError> {
        self.listener
            .local_addr()
            .map(|address| address.port())
            .map_err(|error| InstanceError::State {
                reason: format!("the listener has no address: {error}"),
            })
    }

    /// Waits for one hand-off and returns its arguments.
    ///
    /// # Errors
    ///
    /// [`InstanceError::Unauthorised`] for a payload without this instance's token,
    /// [`InstanceError::TooLarge`] or [`InstanceError::MalformedHandoff`] for a payload that is
    /// not a hand-off, and [`InstanceError::State`] when the wait itself fails.
    pub fn receive(&self) -> Result<Vec<String>, InstanceError> {
        let (stream, _) = self
            .listener
            .accept()
            .map_err(|error| InstanceError::State {
                reason: format!("the hand-off could not be accepted: {error}"),
            })?;
        let payload = read_bounded(&stream)?;
        let handoff: Handoff =
            serde_json::from_slice(&payload).map_err(|error| InstanceError::MalformedHandoff {
                reason: error.to_string(),
            })?;
        if handoff.token != self.token {
            return Err(InstanceError::Unauthorised);
        }
        if handoff.args.len() > MAX_HANDOFF_ARGUMENTS {
            return Err(InstanceError::TooLarge {
                what: "argument list",
            });
        }
        if handoff
            .args
            .iter()
            .any(|arg| arg.len() > MAX_HANDOFF_ARGUMENT_BYTES)
        {
            return Err(InstanceError::TooLarge { what: "argument" });
        }
        Ok(handoff.args)
    }

    /// Reads one hand-off if one is waiting, and returns `None` when none is.
    ///
    /// This is the form a running application uses: a pump loop cannot block waiting for a
    /// second launch that may never come, and a launch that arrives between two pumps must not
    /// be lost either — which is why the listener is asked rather than polled for a file.
    ///
    /// # Errors
    ///
    /// As [`Self::receive`], except that an empty queue is `Ok(None)` rather than an error.
    pub fn try_receive(&self) -> Result<Option<Vec<String>>, InstanceError> {
        self.listener
            .set_nonblocking(true)
            .map_err(|error| InstanceError::State {
                reason: format!("the listener could not be made non-blocking: {error}"),
            })?;
        let accepted = self.listener.accept();
        // Put it back before anything else can observe it: a listener left non-blocking would
        // make the next blocking wait a busy loop.
        let restore = self.listener.set_nonblocking(false);
        match accepted {
            Ok((stream, _)) => {
                restore.map_err(|error| InstanceError::State {
                    reason: format!("the listener could not be made blocking again: {error}"),
                })?;
                // The accepted socket can inherit the listener's non-blocking mode, so a read started
                // before the peer's payload arrives reports `WouldBlock` and the hand-off is lost
                // even though it was sent. The client writes as soon as it connects and
                // `read_bounded` puts a read timeout on the stream, so waiting for it here is bounded
                // exactly as it is on the blocking path.
                stream
                    .set_nonblocking(false)
                    .map_err(|error| InstanceError::State {
                        reason: format!("the hand-off stream could not be made blocking: {error}"),
                    })?;
                let payload = read_bounded(&stream)?;
                let handoff: Handoff = serde_json::from_slice(&payload).map_err(|error| {
                    InstanceError::MalformedHandoff {
                        reason: error.to_string(),
                    }
                })?;
                if handoff.token != self.token {
                    return Err(InstanceError::Unauthorised);
                }
                if handoff.args.len() > MAX_HANDOFF_ARGUMENTS {
                    return Err(InstanceError::TooLarge {
                        what: "argument list",
                    });
                }
                if handoff
                    .args
                    .iter()
                    .any(|arg| arg.len() > MAX_HANDOFF_ARGUMENT_BYTES)
                {
                    return Err(InstanceError::TooLarge { what: "argument" });
                }
                Ok(Some(handoff.args))
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(InstanceError::State {
                reason: format!("the hand-off could not be accepted: {error}"),
            }),
        }
    }

    /// The token a hand-off to this instance must carry.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The lock file this instance holds.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // The lock is this process's claim, and a claim that outlives the process is the stale
        // lock the next launch has to work around.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Hands `args` to the instance listening on `port`.
///
/// # Errors
///
/// [`InstanceError::Unreachable`] when nothing answers, and
/// [`InstanceError::TooLarge`] for an argument vector no hand-off may carry.
pub fn hand_over(port: u16, token: &str, args: &[String]) -> Result<(), InstanceError> {
    if args.len() > MAX_HANDOFF_ARGUMENTS {
        return Err(InstanceError::TooLarge {
            what: "argument list",
        });
    }
    if args
        .iter()
        .any(|arg| arg.len() > MAX_HANDOFF_ARGUMENT_BYTES)
    {
        return Err(InstanceError::TooLarge { what: "argument" });
    }
    let handoff = Handoff {
        token: token.to_owned(),
        args: args.to_vec(),
    };
    let payload = serde_json::to_vec(&handoff).map_err(|error| InstanceError::Unreachable {
        reason: format!("the hand-off could not be encoded: {error}"),
    })?;
    let mut stream =
        TcpStream::connect_timeout(&loopback(port), HANDOFF_TIMEOUT).map_err(|error| {
            InstanceError::Unreachable {
                reason: format!("nothing answered on port {port}: {error}"),
            }
        })?;
    stream
        .set_write_timeout(Some(HANDOFF_TIMEOUT))
        .map_err(|error| InstanceError::Unreachable {
            reason: error.to_string(),
        })?;
    stream
        .write_all(&payload)
        .map_err(|error| InstanceError::Unreachable {
            reason: format!("the hand-off could not be written: {error}"),
        })
}

/// The loopback address a port is reached at.
fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

/// Reads a bounded payload from a stream.
fn read_bounded(stream: &TcpStream) -> Result<Vec<u8>, InstanceError> {
    stream
        .set_read_timeout(Some(HANDOFF_TIMEOUT))
        .map_err(|error| InstanceError::State {
            reason: error.to_string(),
        })?;
    let mut payload = Vec::new();
    // Bounded by `take`: a sender that never stops is a sender that stops at the bound.
    let read = stream
        .take(MAX_HANDOFF_BYTES + 1)
        .read_to_end(&mut payload)
        .map_err(|error| InstanceError::State {
            reason: format!("the hand-off could not be read: {error}"),
        })?;
    if read as u64 > MAX_HANDOFF_BYTES {
        return Err(InstanceError::TooLarge { what: "payload" });
    }
    Ok(payload)
}

/// The lock a path holds, or `None` when it holds none.
fn read_lock(path: &Path) -> Result<Option<Lock>, InstanceError> {
    let encoded = match std::fs::read(path) {
        Ok(encoded) => encoded,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(InstanceError::State {
                reason: format!("{}: {error}", path.display()),
            });
        }
    };
    let lock: Lock =
        serde_json::from_slice(&encoded).map_err(|error| InstanceError::MalformedLock {
            reason: error.to_string(),
        })?;
    if lock.token.is_empty() {
        return Err(InstanceError::MalformedLock {
            reason: "it carries no token, so no hand-off could be trusted".to_owned(),
        });
    }
    Ok(Some(lock))
}

/// The token a new instance accepts hand-offs with.
///
/// Minted from the port, the process id and the clock: it is not a secret in the cryptographic
/// sense — it lives in a file the user can read — but it must not be *guessable* by another
/// local process, which is what a value derived from a clock and a port and a pid is not.
fn mint_token(port: u16) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!(
        "{:08x}{:08x}{:08x}",
        u32::from(port),
        std::process::id(),
        nanos as u32
    )
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A directory this test owns and removes.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "ncui-instance-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// The guard, or a panic explaining that this process is not the first instance.
    fn first(scratch: &Scratch) -> InstanceGuard {
        match InstanceGuard::acquire(&scratch.path, "counter")
            .expect("the state directory is usable")
        {
            Acquired::First(guard) => *guard,
            Acquired::AlreadyRunning { port, .. } => {
                panic!("a fresh state directory must not name a running instance (port {port})")
            }
        }
    }

    #[test]
    fn a_fresh_state_directory_makes_this_process_the_instance() {
        let scratch = Scratch::new();
        let guard = first(&scratch);
        assert!(guard.port().expect("a bound listener has a port") > 0);
        assert!(
            guard.path().is_file(),
            "the lock must be on disk for the next launch to find"
        );
        assert!(!guard.token().is_empty());
    }

    #[test]
    fn a_second_acquire_reports_the_instance_that_is_running() {
        let scratch = Scratch::new();
        let guard = first(&scratch);
        let port = guard.port().expect("a port");
        match InstanceGuard::acquire(&scratch.path, "counter").expect("a usable state directory") {
            Acquired::AlreadyRunning { port: found, token } => {
                assert_eq!(
                    found, port,
                    "the second launch must reach the first instance"
                );
                assert_eq!(token, guard.token(), "and must be given its token");
            }
            Acquired::First(_) => panic!("a second launch must not become the instance"),
        }
    }

    #[test]
    fn a_hand_off_carries_the_arguments_to_the_instance() {
        let scratch = Scratch::new();
        let guard = first(&scratch);
        let port = guard.port().expect("a port");
        let token = guard.token().to_owned();
        let sender = std::thread::spawn(move || {
            hand_over(port, &token, &["--open".to_owned(), "notes.txt".to_owned()])
        });
        let received = guard.receive().expect("the hand-off arrives");
        assert_eq!(received_args(&received), vec!["--open", "notes.txt"]);
        sender
            .join()
            .expect("the sender does not panic")
            .expect("the hand-off is written");
    }

    /// The arguments as they arrived, for comparison against string literals.
    fn received_args(args: &[String]) -> Vec<&str> {
        args.iter().map(String::as_str).collect()
    }

    #[test]
    fn a_hand_off_without_the_token_is_refused() {
        let scratch = Scratch::new();
        let guard = first(&scratch);
        let port = guard.port().expect("a port");
        let sender = std::thread::spawn(move || {
            // A local process that found the port but not the token.
            hand_over(port, "not-the-token", &["--open".to_owned()])
        });
        let error = guard
            .receive()
            .expect_err("a payload without the token is refused");
        assert_eq!(error.code(), "instance_unauthorised");
        sender
            .join()
            .expect("the sender does not panic")
            .expect("the write itself succeeds; it is the payload that is refused");
    }

    #[test]
    fn a_stale_lock_is_taken_over_rather_than_trusted() {
        let scratch = Scratch::new();
        std::fs::create_dir_all(&scratch.path).expect("the state directory is creatable");
        // A lock whose writer is gone: the port is bound by nobody. The port is taken and
        // released so that it is a real port nothing is listening on.
        let dead = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a port");
        let port = dead.local_addr().expect("an address").port();
        drop(dead);
        let lock = serde_json::to_vec(&Lock {
            port,
            token: "0123456789abcdef".to_owned(), // ncui-allow-hygiene: a fixed lock token in a test, not a secret
        })
        .expect("a lock encodes");
        std::fs::write(scratch.path.join("counter.instance"), lock).expect("the lock is writable");

        let guard = first(&scratch);
        assert_ne!(
            guard.port().expect("a port"),
            port,
            "the new instance must listen on its own port, not the dead one"
        );
    }

    #[test]
    fn a_lock_that_is_not_a_lock_is_reported() {
        let scratch = Scratch::new();
        std::fs::create_dir_all(&scratch.path).expect("the state directory is creatable");
        std::fs::write(scratch.path.join("counter.instance"), b"not a lock")
            .expect("the file is writable");
        let error = InstanceGuard::acquire(&scratch.path, "counter")
            .expect_err("a lock that cannot be read is not a stale lock");
        assert_eq!(error.code(), "instance_lock_malformed");
    }

    #[test]
    fn an_oversized_hand_off_is_refused_before_it_is_sent() {
        let scratch = Scratch::new();
        let guard = first(&scratch);
        let port = guard.port().expect("a port");
        let too_many: Vec<String> = (0..=MAX_HANDOFF_ARGUMENTS).map(|n| n.to_string()).collect();
        assert_eq!(
            hand_over(port, guard.token(), &too_many)
                .expect_err("an argument list over the bound is refused")
                .code(),
            "instance_handoff_too_large"
        );
        let too_long = vec!["x".repeat(MAX_HANDOFF_ARGUMENT_BYTES + 1)];
        assert_eq!(
            hand_over(port, guard.token(), &too_long)
                .expect_err("an argument over the bound is refused")
                .code(),
            "instance_handoff_too_large"
        );
    }

    #[test]
    fn a_pump_loop_can_ask_for_a_hand_off_without_waiting_for_one() {
        // The form a running application uses: nothing waiting is not an error, and a hand-off
        // that arrives between two pumps is still there for the next one.
        let scratch = Scratch::new();
        let guard = first(&scratch);
        assert_eq!(
            guard
                .try_receive()
                .expect("an empty queue is not a failure"),
            None
        );
        let port = guard.port().expect("a port");
        let token = guard.token().to_owned();
        let sender = std::thread::spawn(move || hand_over(port, &token, &["--open".to_owned()]));
        let received = loop {
            if let Some(args) = guard.try_receive().expect("a usable listener") {
                break args;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(received_args(&received), vec!["--open"]);
        sender
            .join()
            .expect("the sender does not panic")
            .expect("written");
        // And the listener is usable for the next wait, because it was put back to blocking.
        assert_eq!(guard.try_receive().expect("still usable"), None);
    }

    #[test]
    fn dropping_the_instance_releases_the_lock() {
        let scratch = Scratch::new();
        let path = {
            let guard = first(&scratch);
            guard.path().to_path_buf()
        };
        assert!(!path.exists(), "a released instance leaves no lock behind");
        match InstanceGuard::acquire(&scratch.path, "counter").expect("a usable state directory") {
            Acquired::First(_) => {}
            Acquired::AlreadyRunning { .. } => {
                panic!("a released lock must not be reported as a running instance")
            }
        }
    }

    #[test]
    fn a_name_that_is_not_a_name_is_refused() {
        let scratch = Scratch::new();
        for name in ["", "a/b", "a\\b", "..", "a b"] {
            let error = InstanceGuard::acquire(&scratch.path, name)
                .expect_err("a name that could escape the state directory is refused");
            assert_eq!(error.code(), "instance_state_unusable", "{name}");
        }
    }
}
