//! The clipboard, read and written through the platform's own tool.
//!
//! # Why the platform's tool rather than a binding
//!
//! The clipboard is the one native surface with no `std` interface and no safe Rust path:
//! on Windows it is a window-message protocol (`OpenClipboard`, `SetClipboardData`), on
//! macOS an Objective-C object, and on Linux an X11 or Wayland protocol. Every library
//! that offers it in Rust is a binding over one of those, and this workspace forbids
//! `unsafe` outside a dependency.
//!
//! So this module uses the tool each platform already ships: `clip` and PowerShell on
//! Windows, `pbcopy`/`pbpaste` on macOS, and `wl-copy`/`wl-paste` or `xclip` on Linux. The
//! cost is a process start per call — tens of milliseconds on the Unix tools and a few
//! hundred on PowerShell — and the cost is real; `docs/known-gaps.md` records it, and a
//! direct binding is the fix when the dependency policy admits one.
//!
//! # What is guaranteed
//!
//! * **The text is the payload, not part of a command line.** It is written to the tool's
//!   standard input. On Windows that matters most: `clip` reads the text, and PowerShell
//!   is invoked with `-Command Get-Clipboard -Raw` and never sees the text at all.
//! * **The environment is scrubbed.** The tools are started with the same filtered
//!   environment a page's program gets, so a clipboard tool cannot observe a secret-shaped
//!   variable.
//! * **The read is bounded** and the wait is limited, so a clipboard that never answers
//!   does not hang the caller.
//! * **A missing tool is reported, not hidden.** An empty clipboard and an unavailable
//!   clipboard are different answers, and a caller that treats them the same will paste an
//!   empty document. On Linux both a Wayland and an X11 tool are tried before the answer is
//!   "unavailable". A tool that starts but cannot reach its display also falls through to the
//!   next backend, so a Wayland session failure does not hide a working X11 clipboard.

use std::path::Path;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::{
    io::Write as _,
    process::{Command, Stdio},
    time::Instant,
};

use serde::{Deserialize, Serialize};

use crate::process::{ProcessError, run_command};

/// The largest amount of text a read will keep.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// How long a clipboard tool may take before it is stopped.
pub const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(10);

/// One program that reads or writes the clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tool {
    /// The program name the operating system resolves.
    program: &'static str,
    /// Its arguments.
    args: &'static [&'static str],
}

impl Tool {
    /// The arguments as the command runner takes them.
    fn arguments(&self) -> Vec<String> {
        self.args.iter().map(|arg| (*arg).to_owned()).collect()
    }
}

/// The tools that write the clipboard, in the order they are tried.
#[cfg(windows)]
const WRITE_TOOLS: &[Tool] = &[Tool {
    // `clip` is shipped with Windows and reads the text from standard input.
    program: "clip",
    args: &[],
}];

/// The tools that read the clipboard, in the order they are tried.
#[cfg(windows)]
const READ_TOOLS: &[Tool] = &[Tool {
    // PowerShell is shipped with Windows. `-Raw` keeps line breaks as they are, and
    // `[Console]::Out.Write` avoids the newline the pipeline would otherwise append to the
    // text — a clipboard reader that invents a trailing line break is a bug the user sees
    // when they paste.
    program: "powershell",
    args: &[
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Console]::Out.Write((Get-Clipboard -Raw))",
    ],
}];

/// The tools that write the clipboard, in the order they are tried.
#[cfg(target_os = "macos")]
const WRITE_TOOLS: &[Tool] = &[Tool {
    program: "pbcopy",
    args: &[],
}];

/// The tools that read the clipboard, in the order they are tried.
#[cfg(target_os = "macos")]
const READ_TOOLS: &[Tool] = &[Tool {
    program: "pbpaste",
    args: &[],
}];

/// The tools that write the clipboard, in the order they are tried.
#[cfg(not(any(windows, target_os = "macos")))]
const WRITE_TOOLS: &[Tool] = &[
    Tool {
        program: "wl-copy",
        args: &[],
    },
    Tool {
        program: "xclip",
        args: &["-selection", "clipboard"],
    },
];

/// The tools that read the clipboard, in the order they are tried.
#[cfg(not(any(windows, target_os = "macos")))]
const READ_TOOLS: &[Tool] = &[
    Tool {
        program: "wl-paste",
        args: &["--no-newline"],
    },
    Tool {
        program: "xclip",
        args: &["-selection", "clipboard", "-o"],
    },
];

/// The clipboard, as the runtime offers it to a page.
#[derive(Debug, Clone)]
pub struct Clipboard {
    /// The tools that write, tried in order.
    write_tools: Vec<Tool>,
    /// The tools that read, tried in order.
    read_tools: Vec<Tool>,
    /// The largest read that is kept.
    max_bytes: usize,
}

impl Clipboard {
    /// The clipboard of this platform, using its own tools.
    #[must_use]
    pub fn new() -> Self {
        #[cfg(target_os = "linux")]
        let mut write_tools = WRITE_TOOLS.to_vec();
        #[cfg(not(target_os = "linux"))]
        let write_tools = WRITE_TOOLS.to_vec();
        #[cfg(target_os = "linux")]
        let mut read_tools = READ_TOOLS.to_vec();
        #[cfg(not(target_os = "linux"))]
        let read_tools = READ_TOOLS.to_vec();
        #[cfg(target_os = "linux")]
        if std::env::var_os("WAYLAND_DISPLAY").is_none_or(|value| value.is_empty()) {
            write_tools.retain(|tool| tool.program != "wl-copy");
            read_tools.retain(|tool| tool.program != "wl-paste");
        }
        Self {
            write_tools,
            read_tools,
            max_bytes: MAX_CLIPBOARD_BYTES,
        }
    }

    /// A clipboard backed by the named tools.
    ///
    /// Used by the tests to exercise the unavailability and failure paths without
    /// uninstalling anything, and by a platform whose tool is not the default one.
    #[must_use]
    pub fn with_tools(
        write: &'static str,
        write_args: &'static [&'static str],
        read: &'static str,
        read_args: &'static [&'static str],
    ) -> Self {
        Self {
            write_tools: vec![Tool {
                program: write,
                args: write_args,
            }],
            read_tools: vec![Tool {
                program: read,
                args: read_args,
            }],
            max_bytes: MAX_CLIPBOARD_BYTES,
        }
    }

    /// Reads the clipboard's text.
    ///
    /// # Errors
    ///
    /// Returns [`ClipboardError::Unavailable`] when no tool for this platform is installed,
    /// [`ClipboardError::Failed`] when the tool failed, and [`ClipboardError::TooLarge`]
    /// when the clipboard holds more than the read limit.
    pub fn read_text(&self) -> Result<ClipboardText, ClipboardError> {
        let mut unavailable = None;
        let mut failure = None;
        for tool in &self.read_tools {
            match run_command(
                Path::new(tool.program),
                tool.program,
                &tool.arguments(),
                None,
                CLIPBOARD_TIMEOUT,
                self.max_bytes + 1,
                None,
            ) {
                Ok(output) if output.status == 0 => {
                    if output.stdout.len() > self.max_bytes {
                        return Err(ClipboardError::TooLarge {
                            limit: self.max_bytes,
                        });
                    }
                    return Ok(ClipboardText {
                        bytes: output.stdout.len(),
                        text: output.stdout,
                    });
                }
                Ok(output) => {
                    failure = Some(ClipboardError::Failed {
                        tool: tool.program.to_owned(),
                        reason: first_line(&output.stderr),
                    });
                }
                Err(ProcessError::NotFound { .. }) => {
                    unavailable = Some(ClipboardError::Unavailable {
                        tool: tool.program.to_owned(),
                    });
                }
                Err(other) => {
                    failure = Some(ClipboardError::Failed {
                        tool: tool.program.to_owned(),
                        reason: other.to_string(),
                    });
                }
            }
        }
        Err(failure.unwrap_or_else(|| {
            unavailable.unwrap_or(ClipboardError::Unavailable {
                tool: "the platform's clipboard tool".to_owned(),
            })
        }))
    }

    /// Writes text to the clipboard.
    ///
    /// # Errors
    ///
    /// Returns [`ClipboardError::Unavailable`] when no tool for this platform is installed,
    /// [`ClipboardError::TooLarge`] when the text is over the limit, and
    /// [`ClipboardError::Failed`] when the tool failed.
    pub fn write_text(&self, text: &str) -> Result<ClipboardWritten, ClipboardError> {
        if text.len() > self.max_bytes {
            return Err(ClipboardError::TooLarge {
                limit: self.max_bytes,
            });
        }
        let mut unavailable = None;
        let mut failure = None;
        for tool in &self.write_tools {
            match run_writer(tool, text) {
                Ok(None) => return Ok(ClipboardWritten { bytes: text.len() }),
                Ok(Some(reason)) => {
                    failure = Some(ClipboardError::Failed {
                        tool: tool.program.to_owned(),
                        reason,
                    });
                }
                Err(ProcessError::NotFound { .. }) => {
                    unavailable = Some(ClipboardError::Unavailable {
                        tool: tool.program.to_owned(),
                    });
                }
                Err(other) => {
                    failure = Some(ClipboardError::Failed {
                        tool: tool.program.to_owned(),
                        reason: other.to_string(),
                    });
                }
            }
        }
        Err(failure.unwrap_or_else(|| {
            unavailable.unwrap_or(ClipboardError::Unavailable {
                tool: "the platform's clipboard tool".to_owned(),
            })
        }))
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new()
    }
}

/// The text a page read from the clipboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardText {
    /// The clipboard's text.
    pub text: String,
    /// Its length in bytes.
    pub bytes: usize,
}

/// What a write did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardWritten {
    /// How many bytes were written.
    pub bytes: usize,
}

/// A clipboard operation that could not be performed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardError {
    /// The platform's clipboard tool is not installed.
    Unavailable {
        /// The tool that was looked for.
        tool: String,
    },
    /// The clipboard holds more text than the read limit.
    TooLarge {
        /// The limit, in bytes.
        limit: usize,
    },
    /// The tool failed.
    Failed {
        /// The tool that failed.
        tool: String,
        /// What it said, or why it could not be started.
        reason: String,
    },
}

impl ClipboardError {
    /// The error code this failure is reported under at the page seam.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => "unavailable",
            Self::TooLarge { .. } => "limit_exceeded",
            Self::Failed { .. } => "internal",
        }
    }

    /// Whether retrying the same call could succeed.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { tool } => write!(
                f,
                "the clipboard is unavailable: `{tool}` is not installed on this system"
            ),
            Self::TooLarge { limit } => {
                write!(
                    f,
                    "the clipboard text is larger than the {limit}-byte limit"
                )
            }
            Self::Failed { tool, reason } => write!(f, "`{tool}` failed: {reason}"),
        }
    }
}

impl std::error::Error for ClipboardError {}

/// The first line of a tool's error output, or nothing at all when it said nothing.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.is_empty() {
        "the tool reported a failure without saying why".to_owned()
    } else {
        line.chars().take(200).collect()
    }
}

/// Runs a clipboard writer, returning its failure reason when it did not succeed.
fn run_writer(tool: &Tool, text: &str) -> Result<Option<String>, ProcessError> {
    #[cfg(target_os = "linux")]
    {
        run_linux_writer(tool, text)
    }
    #[cfg(not(target_os = "linux"))]
    {
        run_command(
            Path::new(tool.program),
            tool.program,
            &tool.arguments(),
            Some(text),
            CLIPBOARD_TIMEOUT,
            MAX_CLIPBOARD_BYTES,
            None,
        )
        .map(|output| {
            if output.status == 0 {
                None
            } else {
                Some(first_line(&output.stderr))
            }
        })
    }
}

/// Linux clipboard owners daemonize while retaining the selection, so their inherited capture
/// pipes must not keep the runner waiting until its timeout kills the new clipboard owner.
#[cfg(target_os = "linux")]
fn run_linux_writer(tool: &Tool, text: &str) -> Result<Option<String>, ProcessError> {
    use std::os::unix::process::CommandExt as _;

    let mut command = Command::new(tool.program);
    command
        .args(tool.arguments())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .process_group(0);
    for (name, value) in crate::process::scrubbed_environment() {
        command.env(name, value);
    }
    let mut child = command.spawn().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ProcessError::NotFound {
            program: tool.program.to_owned(),
        },
        _ => ProcessError::SpawnFailed {
            program: tool.program.to_owned(),
            reason: error.to_string(),
        },
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        let input = text.to_owned();
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok((!status.success()).then(|| format!("exited with {status}")));
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let group = format!("-{}", child.id());
                let _ = Command::new("kill")
                    .args(["-KILL", group.as_str()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = child.kill();
                let _ = child.wait();
                return Ok(Some(
                    "timed out while taking ownership of the clipboard".to_owned(),
                ));
            }
            Err(error) => {
                return Err(ProcessError::SpawnFailed {
                    program: tool.program.to_owned(),
                    reason: error.to_string(),
                });
            }
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
    use super::*;

    #[test]
    fn text_written_to_the_clipboard_is_read_back() {
        // The real clipboard, on whichever platform runs this. The text carries the
        // process id so a leftover clipboard from another run cannot make the test pass.
        let clipboard = Clipboard::new();
        let text = format!("ncui clipboard test {}", std::process::id());
        match clipboard.write_text(&text) {
            Ok(written) => assert_eq!(written.bytes, text.len()),
            // A machine with no clipboard tool is a fact about the machine rather than a defect in
            // this crate — a container, a bare server, a desktop without `xclip`. The GPU tests
            // report the same thing the same way: what is skipped is the part only a real platform
            // can answer, and the refusal path is covered by the test below.
            Err(ClipboardError::Unavailable { tool }) => {
                assert!(
                    std::env::var_os("NCUI_REQUIRE_CLIPBOARD").is_none(),
                    "clipboard evidence was required, but no tool is available: {tool}"
                );
                return;
            }
            Err(error) => panic!("the write failed: {error}"),
        }
        let read = clipboard.read_text().expect("the text is read back");
        assert_eq!(read.text, text, "the clipboard must round-trip the text");
    }

    #[test]
    fn a_missing_tool_is_reported_as_unavailable_rather_than_empty() {
        // An empty clipboard and an unavailable clipboard are different answers. A caller
        // that cannot tell them apart pastes an empty document.
        let clipboard = Clipboard::with_tools(
            "ncui-no-such-clipboard-tool",
            &[],
            "ncui-no-such-clipboard-tool",
            &[],
        );
        let error = clipboard
            .read_text()
            .expect_err("a missing tool is not an empty clipboard");
        assert_eq!(error.code(), "unavailable", "got: {error}");
        let error = clipboard
            .write_text("x")
            .expect_err("a missing tool cannot write");
        assert_eq!(error.code(), "unavailable", "got: {error}");
    }

    #[test]
    fn text_over_the_limit_is_refused_before_the_tool_is_reached() {
        let mut clipboard = Clipboard::new();
        clipboard.max_bytes = 4;
        match clipboard.write_text("more than four bytes") {
            Err(ClipboardError::TooLarge { limit }) => assert_eq!(limit, 4),
            other => panic!("expected a size refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_failing_tool_is_reported_with_its_own_message() {
        // A tool that exists but fails must not be reported as an empty clipboard either.
        let (read, read_args): (&'static str, &'static [&'static str]) = if cfg!(windows) {
            ("cmd", &["/C", "exit 4"])
        } else {
            ("sh", &["-c", "echo refused >&2; exit 4"])
        };
        let clipboard = Clipboard::with_tools("cmd", &[], read, read_args);
        let error = clipboard.read_text().expect_err("a failing tool");
        assert_eq!(error.code(), "internal", "got: {error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_clipboard_tool_does_not_hide_the_next_backend() {
        let clipboard = Clipboard {
            write_tools: vec![
                Tool {
                    program: "sh",
                    args: &["-c", "exit 4"],
                },
                Tool {
                    program: "sh",
                    args: &["-c", "cat >/dev/null"],
                },
            ],
            read_tools: vec![
                Tool {
                    program: "sh",
                    args: &["-c", "exit 4"],
                },
                Tool {
                    program: "sh",
                    args: &["-c", "printf fallback"],
                },
            ],
            max_bytes: MAX_CLIPBOARD_BYTES,
        };
        clipboard
            .write_text("payload")
            .expect("the fallback writes");
        assert_eq!(
            clipboard.read_text().expect("the fallback reads").text,
            "fallback"
        );
    }

    #[test]
    fn every_platform_has_a_tool_to_try() {
        // The list is what makes "unavailable" a real answer rather than a default. A
        // platform with no tools would report every clipboard call as unavailable.
        assert!(!Clipboard::new().read_tools.is_empty());
        assert!(!Clipboard::new().write_tools.is_empty());
    }
}
