//! Running a program on a page's behalf, under a capability and a set of hard limits.
//!
//! An application that may run a program may run *anything* the program can do. That is
//! the authority `process.spawn` names, and it is why this module is small and strict
//! rather than convenient:
//!
//! * **No shell.** The program and its arguments are passed as an argument vector. Nothing
//!   is concatenated into a command line, so a page cannot turn an argument into a second
//!   command. On Windows this matters most: `cmd /C` would re-parse the line, and this
//!   module never invokes it.
//! * **A path names a file inside the application, or a bare program name.** A relative
//!   path is resolved with the same containment check the filesystem uses. An absolute path
//!   is refused: a page may run the application's own tools, or a program the operating
//!   system resolves by name — not an arbitrary file elsewhere on the disk.
//! * **The environment is scrubbed.** A child inherits a filtered environment: any variable
//!   whose name looks like a key, a secret, a token, a password or a credential is dropped,
//!   so a tool that prints its environment cannot print the user's secrets.
//! * **Output is bounded and the run is timed.** A program that never exits, or one that
//!   writes without end, is stopped rather than allowed to exhaust the process.
//!
//! What is *not* done here: a sandbox. A program that is allowed to run runs with the
//! user's authority, exactly as it would if the user started it. The capability is what
//! gates that, and the manifest is where the user grants it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ncui_security::path::SafePath;
use serde::{Deserialize, Serialize};

/// The largest amount of a child's output that is kept, per stream.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// The longest a child may run when the request does not say.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest a child may run, whatever the request asks for.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(300);

/// The substring patterns that mark an environment variable as a secret.
///
/// The rule is a name filter, not a value filter: a variable is dropped because of what it
/// is called, because a value cannot be recognised as a secret without knowing the secret.
const SECRET_NAME_PATTERNS: &[&str] = &[
    "KEY",
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "AUTH",
    "SESSION",
    "COOKIE",
    "PRIVATE",
    "CERT",
];

/// A program runner rooted at one directory.
#[derive(Debug, Clone)]
pub struct ProcessRunner {
    root: PathBuf,
    max_output: usize,
    default_timeout: Duration,
    max_timeout: Duration,
}

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequest {
    /// The program: a path inside the application, or a bare name the system resolves.
    pub program: String,
    /// The arguments, passed as an argument vector and never through a shell.
    #[serde(default)]
    pub args: Vec<String>,
    /// What to write to the child's standard input, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// How long the child may run, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// What running it produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOutput {
    /// The program as the caller named it.
    pub program: String,
    /// The arguments as the caller gave them.
    pub args: Vec<String>,
    /// The exit code, or `-1` when the child was killed by a signal.
    pub status: i32,
    /// The child's standard output, decoded as UTF-8 with invalid bytes replaced.
    pub stdout: String,
    /// The child's standard error, decoded as UTF-8 with invalid bytes replaced.
    pub stderr: String,
    /// Whether the child was stopped because it ran past its time.
    pub timed_out: bool,
    /// Whether either stream was longer than the cap and was cut.
    pub truncated: bool,
}

impl ProcessRunner {
    /// A runner rooted at `root`.
    ///
    /// # Errors
    ///
    /// Returns an error when the root does not exist, is not a directory, or cannot be
    /// canonicalized.
    pub fn new(root: &Path) -> std::io::Result<Self> {
        let canonical = root.canonicalize()?;
        if !canonical.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a directory", canonical.display()),
            ));
        }
        Ok(Self {
            root: canonical,
            max_output: MAX_OUTPUT_BYTES,
            default_timeout: DEFAULT_TIMEOUT,
            max_timeout: MAX_TIMEOUT,
        })
    }

    /// The directory relative program paths are contained by.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Runs a program and waits for it, or for its time to run out.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessError::OutsideRoot`] for a path that escapes the root or is
    /// absolute, [`ProcessError::NotFound`] for a program that is not there, and
    /// [`ProcessError::SpawnFailed`] when the operating system refuses to start it.
    pub fn run(&self, request: &RunRequest) -> Result<RunOutput, ProcessError> {
        let program = self.resolve_program(&request.program)?;
        let timeout = request
            .timeout_ms
            .map_or(self.default_timeout, Duration::from_millis)
            .min(self.max_timeout);
        run_command(
            &program,
            &request.program,
            &request.args,
            request.stdin.as_deref(),
            timeout,
            self.max_output,
        )
    }

    /// Resolves the program to start.
    fn resolve_program(&self, program: &str) -> Result<PathBuf, ProcessError> {
        if program.trim().is_empty() {
            return Err(ProcessError::NotExecutable {
                program: program.to_owned(),
            });
        }
        // An absolute path, a rooted path, or a drive-qualified one names a file outside
        // the application, and is refused before anything else looks at it.
        if program.starts_with('/')
            || program.starts_with('\\')
            || has_drive_prefix(program)
            || Path::new(program).is_absolute()
        {
            return Err(ProcessError::OutsideRoot {
                program: program.to_owned(),
                reason: "only a path inside the application, or a bare program name, may be \
                         started"
                    .to_owned(),
            });
        }

        // A bare name is resolved by the operating system through `PATH`. That is what
        // lets an application use `git` or `ffmpeg` without shipping it, and the
        // `process.spawn` capability is what makes that the application's decision.
        if !program.contains('/') && !program.contains('\\') {
            return Ok(PathBuf::from(program));
        }

        let resolved =
            SafePath::resolve(&self.root, program).map_err(|error| ProcessError::OutsideRoot {
                program: program.to_owned(),
                reason: error.to_string(),
            })?;
        let canonical = resolved
            .canonicalize()
            .map_err(|_| ProcessError::NotFound {
                program: program.to_owned(),
            })?;
        if !SafePath::is_contained(&self.root, &canonical) {
            return Err(ProcessError::OutsideRoot {
                program: program.to_owned(),
                reason: "the path resolves outside the application through a link".to_owned(),
            });
        }
        if !canonical.is_file() {
            return Err(ProcessError::NotExecutable {
                program: program.to_owned(),
            });
        }
        Ok(canonical)
    }
}

/// Runs a prepared program with the environment scrub, a time limit and bounded output.
///
/// Shared by the page-facing runner and by the capabilities the framework implements
/// itself: the clipboard is read and written by a platform tool, and that tool gets the
/// same environment treatment a page's program does.
pub(crate) fn run_command(
    program: &Path,
    label: &str,
    args: &[String],
    stdin: Option<&str>,
    timeout: Duration,
    max_output: usize,
) -> Result<RunOutput, ProcessError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for (name, value) in scrubbed_environment() {
        command.env(name, value);
    }
    // A console program started from a GUI process would otherwise open a console window
    // the user did not ask for and cannot use.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    // On Unix the child leads its own process group, which is what lets a timeout stop
    // everything the program started rather than only the program. Windows needs no
    // equivalent here: `taskkill /T` walks the parent chain instead.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }

    let mut child = command.spawn().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ProcessError::NotFound {
            program: label.to_owned(),
        },
        _ => ProcessError::SpawnFailed {
            program: label.to_owned(),
            reason: error.to_string(),
        },
    })?;

    // Standard input is written on its own thread: a child that writes a lot while we are
    // still writing to it would otherwise fill its output pipe and block, and we would
    // block writing to a child that never reads.
    if let Some(input) = stdin.map(ToOwned::to_owned) {
        let mut sink = child.stdin.take();
        let _ = std::thread::Builder::new()
            .name("ncui-process-stdin".to_owned())
            .spawn(move || {
                use std::io::Write as _;
                if let Some(sink) = sink.as_mut() {
                    let _ = sink.write_all(input.as_bytes());
                }
            });
    } else {
        // Closing the child's standard input is what tells a program that reads from it
        // that there is nothing more.
        drop(child.stdin.take());
    }

    let stdout = child.stdout.take().map(|pipe| {
        std::thread::Builder::new()
            .name("ncui-process-stdout".to_owned())
            .spawn(move || capture(pipe, max_output))
    });
    let stderr = child.stderr.take().map(|pipe| {
        std::thread::Builder::new()
            .name("ncui-process-stderr".to_owned())
            .spawn(move || capture(pipe, max_output))
    });
    let (Some(Ok(stdout)), Some(Ok(stderr))) = (stdout, stderr) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ProcessError::SpawnFailed {
            program: label.to_owned(),
            reason: "the child's output could not be read".to_owned(),
        });
    };

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    terminate_tree(&mut child);
                    break child.wait().map_err(|error| ProcessError::SpawnFailed {
                        program: label.to_owned(),
                        reason: error.to_string(),
                    })?;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(ProcessError::SpawnFailed {
                    program: label.to_owned(),
                    reason: error.to_string(),
                });
            }
        }
    };

    let stdout = join_capture(stdout);
    let stderr = join_capture(stderr);

    Ok(RunOutput {
        program: label.to_owned(),
        args: args.to_vec(),
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
        timed_out,
        truncated: stdout.truncated || stderr.truncated,
    })
}

/// Stops a program and everything it started.
///
/// Killing the direct child is not enough, and the gap is easy to miss because the direct
/// child does stop. A build tool that shells out to a compiler, a script that starts a
/// server, a launcher that spawns the program the user actually meant — all of them leave
/// work running that the caller believes it has stopped. The user asked for the program to
/// end, so the program's whole tree ends.
///
/// Both platform tools are named here as constants rather than taken from a caller: this is
/// the host stopping its own child, not a page asking for a program.
fn terminate_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // `/T` walks the parent chain, and `/F` is required: a console program with no window
        // has no polite request to answer. The pid is the runner's own child, so it is a number
        // this process was handed rather than a string anyone else can influence.
        let pid = child.id().to_string();
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", pid.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .status();
    }
    #[cfg(unix)]
    {
        // The child leads its own process group, so its group id is its pid and one signal
        // reaches every process in the group.
        let group = format!("-{}", child.id());
        let _ = Command::new("kill")
            .args(["-KILL", group.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    // A last direct kill, for the case where the platform tool is missing or refused: the
    // caller's own child must not survive its timeout even then.
    let _ = child.kill();
}

/// Whether a program name starts with a drive letter and a colon.
fn has_drive_prefix(program: &str) -> bool {
    let bytes = program.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// The environment a child receives: the ambient environment with secret-shaped names
/// removed.
///
/// The filter is on the *name*, because a value cannot be recognised as a secret without
/// knowing the secret. It is deliberately broad: dropping a harmless variable costs a
/// program an option, while keeping a secret costs the user the secret.
pub fn scrubbed_environment() -> Vec<(String, String)> {
    scrubbed_from(std::env::vars())
}

/// The scrub, over an environment supplied by the caller.
///
/// Split from [`scrubbed_environment`] so the rule can be tested against a known set
/// instead of whichever variables the machine running the tests happens to have.
pub fn scrubbed_from<I>(variables: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    variables
        .into_iter()
        .filter(|(name, _)| !is_secret_shaped(name))
        .collect()
}

/// Whether a variable's name marks it as a secret.
#[must_use]
pub fn is_secret_shaped(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_NAME_PATTERNS
        .iter()
        .any(|pattern| upper.contains(pattern))
}

/// The output of one stream, bounded.
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Reads a pipe to its end, keeping at most `limit` bytes.
///
/// The pipe is drained even after the limit is reached, because a child whose output is
/// not read blocks forever — stopping at the limit would turn "too much output" into "the
/// program hangs".
fn capture(mut pipe: impl std::io::Read + Send + 'static, limit: usize) -> Captured {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let room = limit.saturating_sub(kept.len());
                if read > room {
                    truncated = true;
                }
                kept.extend_from_slice(&buffer[..read.min(room)]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    Captured {
        bytes: kept,
        truncated,
    }
}

/// Waits for a capture thread and returns what it kept.
fn join_capture(handle: std::thread::JoinHandle<Captured>) -> Captured {
    handle.join().unwrap_or(Captured {
        bytes: Vec::new(),
        truncated: false,
    })
}

/// A program that could not be started, or a path that could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessError {
    /// The program named a path outside the application, or an absolute one.
    OutsideRoot {
        /// The program as the caller named it.
        program: String,
        /// Why it was refused.
        reason: String,
    },
    /// The program does not exist.
    NotFound {
        /// The program as the caller named it.
        program: String,
    },
    /// The program names something that is not a file.
    NotExecutable {
        /// The program as the caller named it.
        program: String,
    },
    /// The operating system refused to start the program.
    SpawnFailed {
        /// The program as the caller named it.
        program: String,
        /// What the operating system said.
        reason: String,
    },
}

impl ProcessError {
    /// The error code this failure is reported under at the page seam.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::OutsideRoot { .. } => "capability_denied",
            Self::NotFound { .. } => "not_found",
            Self::NotExecutable { .. } => "invalid_params",
            Self::SpawnFailed { .. } => "internal",
        }
    }

    /// Whether retrying the same call could succeed.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::SpawnFailed { .. })
    }
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideRoot { program, reason } => {
                write!(f, "`{program}` cannot be started: {reason}")
            }
            Self::NotFound { program } => write!(f, "`{program}` does not exist"),
            Self::NotExecutable { program } => write!(f, "`{program}` is not a file"),
            Self::SpawnFailed { program, reason } => {
                write!(f, "`{program}` could not be started: {reason}")
            }
        }
    }
}

impl std::error::Error for ProcessError {}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    fn runner() -> ProcessRunner {
        ProcessRunner::new(&std::env::temp_dir()).expect("the system temp directory is a root")
    }

    /// A program that prints `text` and exits zero, on the platform it is running on.
    fn echo_request(text: &str) -> RunRequest {
        if cfg!(windows) {
            RunRequest {
                program: "cmd".to_owned(),
                args: vec!["/C".to_owned(), format!("echo {text}")],
                stdin: None,
                timeout_ms: None,
            }
        } else {
            RunRequest {
                program: "sh".to_owned(),
                args: vec!["-c".to_owned(), format!("echo {text}")],
                stdin: None,
                timeout_ms: None,
            }
        }
    }

    /// A directory this test owns and removes.
    ///
    /// The suite owns what it acquires: a scratch directory left behind makes the next run
    /// differ from this one, and this test writes two scripts into it.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default();
            let path =
                std::env::temp_dir().join(format!("ncui-tree-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).expect("a scratch directory is creatable");
            Self { path }
        }

        fn heartbeat(&self) -> PathBuf {
            self.path.join("heartbeat.txt")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// How large a file is, or zero when it does not exist yet.
    fn size_of(path: &Path) -> u64 {
        std::fs::metadata(path).map_or(0, |meta| meta.len())
    }

    /// Waits for a file to have content, up to `budget`, and reports its size either way.
    ///
    /// The wait exists because the grandchild is a separate process that has to start: a test
    /// that read the file once, immediately, would be measuring how fast the operating system
    /// launches PowerShell rather than whether the tree was stopped.
    fn wait_for_content(path: &Path, budget: Duration) -> u64 {
        let deadline = Instant::now() + budget;
        loop {
            let size = size_of(path);
            if size > 0 || Instant::now() >= deadline {
                return size;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn a_timeout_stops_the_program_and_everything_it_started() {
        // The gap this closes is easy to miss because the direct child does stop. Here the
        // direct child starts a grandchild that appends to a file once a second and then waits
        // for two minutes; whether the grandchild is still alive is therefore a fact the test
        // can read, rather than something the test has to take on trust.
        let scratch = Scratch::new();
        let heartbeat = scratch.heartbeat();
        let (program, args) = child_with_grandchild(&scratch, &heartbeat);
        let runner = ProcessRunner::new(&std::env::temp_dir()).expect("a root");

        let output = runner
            .run(&RunRequest {
                program,
                args,
                stdin: None,
                timeout_ms: Some(8_000),
            })
            .expect("the run is reported, not failed");
        assert!(
            output.timed_out,
            "the run must have been stopped by its time limit"
        );

        let first = wait_for_content(&heartbeat, Duration::from_secs(3));
        assert!(
            first > 0,
            "the grandchild must have written its heartbeat, or this test proves nothing"
        );

        // Four seconds is four heartbeats: a grandchild that survived the timeout is visible
        // as growth, and one that was stopped leaves the file exactly as it was.
        std::thread::sleep(Duration::from_secs(4));
        let after = size_of(&heartbeat);
        assert_eq!(
            after,
            first,
            "the grandchild outlived the timeout: it wrote {} more bytes after the run was \
             stopped",
            after.saturating_sub(first)
        );
    }

    /// A program that starts a grandchild and then waits, for the platform under test.
    ///
    /// Both scripts are written to files rather than passed as arguments, because the nesting
    /// of quoting through two shells is exactly the kind of construction that turns a test of
    /// process trees into a test of escaping.
    fn child_with_grandchild(scratch: &Scratch, heartbeat: &Path) -> (String, Vec<String>) {
        if cfg!(windows) {
            let grandchild = scratch.path.join("grandchild.ps1");
            std::fs::write(
                &grandchild,
                format!(
                    "for($i=0;$i -lt 120;$i++){{Add-Content -LiteralPath '{}' -Value $i; \
                     Start-Sleep -Seconds 1}}\n",
                    heartbeat.display()
                ),
            )
            .expect("the grandchild script is writable");
            let parent = scratch.path.join("parent.ps1");
            std::fs::write(
                &parent,
                format!(
                    "Start-Process -FilePath 'powershell' -ArgumentList '-NoProfile',\
                     '-ExecutionPolicy','Bypass','-File','{}' | Out-Null\n\
                     Start-Sleep -Seconds 120\n",
                    grandchild.display()
                ),
            )
            .expect("the parent script is writable");
            (
                "powershell".to_owned(),
                vec![
                    "-NoProfile".to_owned(),
                    "-ExecutionPolicy".to_owned(),
                    "Bypass".to_owned(),
                    "-File".to_owned(),
                    parent.display().to_string(),
                ],
            )
        } else {
            let grandchild = scratch.path.join("grandchild.sh");
            std::fs::write(
                &grandchild,
                format!(
                    "i=0\nwhile [ $i -lt 120 ]; do echo $i >> '{}'; i=$((i+1)); sleep 1; done\n",
                    heartbeat.display()
                ),
            )
            .expect("the grandchild script is writable");
            (
                "sh".to_owned(),
                vec![
                    "-c".to_owned(),
                    format!("sh '{}' & sleep 120", grandchild.display()),
                ],
            )
        }
    }

    #[test]
    fn a_program_runs_and_its_output_is_returned() {
        let output = runner()
            .run(&echo_request("hello"))
            .expect("the program runs");
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout.trim(), "hello");
        assert!(!output.timed_out);
        assert!(!output.truncated);
    }

    #[test]
    fn the_exit_code_is_reported_rather_than_treated_as_a_failure() {
        // A non-zero exit is a result, not an error: an application routinely runs a tool
        // that reports "no" through its exit code.
        let request = if cfg!(windows) {
            RunRequest {
                program: "cmd".to_owned(),
                args: vec!["/C".to_owned(), "exit 3".to_owned()],
                stdin: None,
                timeout_ms: None,
            }
        } else {
            RunRequest {
                program: "sh".to_owned(),
                args: vec!["-c".to_owned(), "exit 3".to_owned()],
                stdin: None,
                timeout_ms: None,
            }
        };
        let output = runner().run(&request).expect("the program runs");
        assert_eq!(output.status, 3);
    }

    #[test]
    fn a_program_that_runs_too_long_is_stopped() {
        // The long-running program is started directly, not through a shell: a shell would
        // be the process this runner stops, and the program it started would outlive it.
        // The runner's contract is that it stops the program it started.
        let request = if cfg!(windows) {
            RunRequest {
                program: "ping".to_owned(),
                args: vec!["-n".to_owned(), "20".to_owned(), "127.0.0.1".to_owned()],
                stdin: None,
                timeout_ms: Some(300),
            }
        } else {
            RunRequest {
                program: "sleep".to_owned(),
                args: vec!["20".to_owned()],
                stdin: None,
                timeout_ms: Some(300),
            }
        };
        let started = Instant::now();
        let output = runner().run(&request).expect("the run finishes");
        assert!(output.timed_out, "the run must be reported as timed out");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the child must be stopped rather than waited for"
        );
    }

    #[test]
    fn the_scrub_keeps_ordinary_variables_and_drops_secret_shaped_ones() {
        let kept = scrubbed_from([
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("HOME".to_owned(), "/home/someone".to_owned()),
            ("GITHUB_TOKEN".to_owned(), "ghp_example".to_owned()),
            ("AWS_SECRET_ACCESS_KEY".to_owned(), "aws_example".to_owned()),
            ("HTTP_COOKIE".to_owned(), "session=example".to_owned()),
            ("DATABASE_PASSWORD".to_owned(), "db_example".to_owned()),
        ]);
        let names: Vec<&str> = kept.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["PATH", "HOME"], "only the ordinary names survive");
        assert!(
            kept.iter().all(|(_, value)| !value.contains("example")),
            "no secret value may survive the scrub"
        );
    }

    #[test]
    fn a_secret_shaped_environment_variable_is_dropped() {
        // The name filter is what keeps a harness credential out of a tool's environment.
        for name in [
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "NPM_TOKEN",
            "DATABASE_PASSWORD",
            "SSH_PRIVATE_KEY",
            "HTTP_COOKIE",
            "NODE_EXTRA_CA_CERT",
            "SOME_AUTH",
            "SESSION_ID",
        ] {
            assert!(is_secret_shaped(name), "{name} must be treated as a secret");
        }
        for name in ["PATH", "HOME", "USERPROFILE", "SystemRoot", "LANG", "TEMP"] {
            assert!(!is_secret_shaped(name), "{name} must survive the scrub");
        }
    }

    #[test]
    fn a_child_receives_the_scrubbed_environment_rather_than_an_empty_one() {
        // The child's environment is the scrubbed one, not nothing: a tool that cannot see
        // `PATH` cannot find anything, and a scrub that broke every program would be a
        // denial of service rather than a security control.
        let request = if cfg!(windows) {
            RunRequest {
                program: "cmd".to_owned(),
                args: vec!["/C".to_owned(), "echo [%PATH%]".to_owned()],
                stdin: None,
                timeout_ms: None,
            }
        } else {
            RunRequest {
                program: "sh".to_owned(),
                args: vec!["-c".to_owned(), "echo [$PATH]".to_owned()],
                stdin: None,
                timeout_ms: None,
            }
        };
        let output = runner().run(&request).expect("the program runs");
        assert_ne!(
            output.stdout.trim(),
            "[]",
            "PATH must reach the child; got: {}",
            output.stdout
        );
    }

    #[test]
    fn standard_input_is_delivered_to_the_child() {
        let request = if cfg!(windows) {
            RunRequest {
                program: "cmd".to_owned(),
                args: vec!["/C".to_owned(), "findstr .".to_owned()],
                stdin: Some("from the page\n".to_owned()),
                timeout_ms: None,
            }
        } else {
            RunRequest {
                program: "cat".to_owned(),
                args: Vec::new(),
                stdin: Some("from the page\n".to_owned()),
                timeout_ms: None,
            }
        };
        let output = runner().run(&request).expect("the program runs");
        assert_eq!(output.stdout.trim(), "from the page");
    }

    #[test]
    fn an_absolute_program_path_is_refused() {
        let request = if cfg!(windows) {
            RunRequest {
                program: "C:\\Windows\\System32\\cmd.exe".to_owned(),
                args: Vec::new(),
                stdin: None,
                timeout_ms: None,
            }
        } else {
            RunRequest {
                program: "/bin/sh".to_owned(),
                args: Vec::new(),
                stdin: None,
                timeout_ms: None,
            }
        };
        let error = runner()
            .run(&request)
            .expect_err("an absolute path is refused");
        assert_eq!(error.code(), "capability_denied", "got: {error}");
    }

    #[test]
    fn a_relative_path_that_leaves_the_root_is_refused() {
        let request = RunRequest {
            program: "../../evil".to_owned(),
            args: Vec::new(),
            stdin: None,
            timeout_ms: None,
        };
        let error = runner().run(&request).expect_err("an escape is refused");
        assert_eq!(error.code(), "capability_denied", "got: {error}");
    }

    #[test]
    fn a_program_that_does_not_exist_is_reported_as_missing() {
        let request = if cfg!(windows) {
            RunRequest {
                program: "ncui-no-such-program.exe".to_owned(),
                args: Vec::new(),
                stdin: None,
                timeout_ms: None,
            }
        } else {
            RunRequest {
                program: "ncui-no-such-program".to_owned(),
                args: Vec::new(),
                stdin: None,
                timeout_ms: None,
            }
        };
        let error = runner().run(&request).expect_err("a missing program");
        assert_eq!(error.code(), "not_found", "got: {error}");
    }

    #[test]
    fn output_over_the_cap_is_cut_and_reported_as_cut() {
        // The program is asked to produce far more output than the cap, and the runner is
        // constructed with a small cap so the test stays quick.
        let mut runner = runner();
        runner.max_output = 16;
        let request = if cfg!(windows) {
            RunRequest {
                program: "cmd".to_owned(),
                args: vec![
                    "/C".to_owned(),
                    "for /L %i in (1,1,200) do @echo 0123456789".to_owned(),
                ],
                stdin: None,
                timeout_ms: None,
            }
        } else {
            RunRequest {
                program: "sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    "i=0; while [ $i -lt 200 ]; do echo 0123456789; i=$((i+1)); done".to_owned(),
                ],
                stdin: None,
                timeout_ms: None,
            }
        };
        let output = runner.run(&request).expect("the program runs");
        assert!(output.truncated, "the output must be reported as truncated");
        assert!(
            output.stdout.len() <= 16,
            "the kept output must respect the cap; got {} bytes",
            output.stdout.len()
        );
    }

    #[test]
    fn a_request_with_no_program_is_refused() {
        let request = RunRequest {
            program: "   ".to_owned(),
            args: Vec::new(),
            stdin: None,
            timeout_ms: None,
        };
        let error = runner().run(&request).expect_err("an empty program");
        assert_eq!(error.code(), "invalid_params");
    }
}
