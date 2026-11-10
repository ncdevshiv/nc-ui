//! Handing a target to whatever the platform uses for it.
//!
//! A link the user clicked, a document the application produced, a folder it wrote into: the
//! application should be able to *open* one and let the platform decide what that means. Electron
//! calls this `shell.openExternal` and Tauri `opener`; here it is the platform's own tool, for the
//! same reason the clipboard and the notifier are — every Rust crate that offers it is a binding
//! over `ShellExecute`, `NSWorkspace` or the freedesktop portal, and this workspace forbids
//! `unsafe` outside a dependency.
//!
//! # The target is an argument, except where the platform makes it a shell line
//!
//! On macOS and Linux the target is one argument to `open` or `xdg-open`, so nothing about it can
//! become a second command. On Windows the tool *is* `cmd`, because `start` is a `cmd` builtin,
//! and that means the target passes through a command line: [`check_target`] therefore refuses a
//! target carrying a quote, a line break or a control character, because those are the characters
//! that end the quoting `start` was given. The refusal is on the *target*, not on the tool,
//! because the tool is the platform's and cannot be replaced.
//!
//! # What is refused outright
//!
//! Schemes that mean nothing to the operating system and something to a *browser*: `javascript`,
//! `data`, `vbscript`, `blob` and `about`. Handing `javascript:…` to a platform opener is the
//! classic way an application turns a link into script execution in whatever handles it, and no
//! legitimate caller means that by "open this".
//!
//! # What a caller still has to decide
//!
//! Whether the target may be opened at all. This module answers "can the platform be asked", and
//! the capability gate answers "may this page ask" — opening a target starts whatever program the
//! platform associates with it, which is a program the page did not name, so the command that
//! exposes this is gated by the same capability as spawning one and carries the same per-call
//! consent.

use std::path::Path;

use crate::process::{ProcessError, run_command};

/// How long the platform's opener may take before it is stopped.
pub const OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The longest target that will be opened.
pub const MAX_TARGET_BYTES: usize = 4096;

/// The schemes that are not the operating system's to open.
pub const REFUSED_SCHEMES: &[&str] = &[
    "javascript",
    "data",
    "vbscript",
    "blob",
    "about",
    "chrome",
    "view-source",
];

/// Why a target could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// The target is empty, too long, or carries a character that cannot survive the tool.
    RefusedTarget {
        /// What was given, truncated.
        target: String,
        /// Why.
        reason: String,
    },
    /// The target's scheme is not the operating system's to open.
    RefusedScheme {
        /// The scheme.
        scheme: String,
    },
    /// The platform has no opener this host can use.
    Unavailable {
        /// What was looked for.
        tool: &'static str,
    },
    /// The tool ran and refused.
    Failed {
        /// The tool.
        tool: &'static str,
        /// What it said.
        reason: String,
    },
}

impl OpenError {
    /// The stable code a caller sees on the wire.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::RefusedTarget { .. } => "open_target_refused",
            Self::RefusedScheme { .. } => "open_scheme_refused",
            Self::Unavailable { .. } => "open_unavailable",
            Self::Failed { .. } => "open_failed",
        }
    }
}

impl core::fmt::Display for OpenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RefusedTarget { target, reason } => {
                write!(f, "`{target}` cannot be opened: {reason}")
            }
            Self::RefusedScheme { scheme } => write!(
                f,
                "`{scheme}` is not a scheme the operating system opens; handing it to the \
                 platform's opener is how an application turns a link into script execution"
            ),
            Self::Unavailable { tool } => write!(
                f,
                "this system has no `{tool}`, which is how this platform opens a target"
            ),
            Self::Failed { tool, reason } => {
                write!(f, "`{tool}` could not open the target: {reason}")
            }
        }
    }
}

impl std::error::Error for OpenError {}

/// Whether `target` may be handed to the platform.
///
/// # Errors
///
/// [`OpenError::RefusedTarget`] for an empty, over-long or unquotable target, and
/// [`OpenError::RefusedScheme`] for a scheme that is a browser's rather than the operating
/// system's.
pub fn check_target(target: &str) -> Result<(), OpenError> {
    if target.is_empty() {
        return Err(OpenError::RefusedTarget {
            target: target.to_owned(),
            reason: "it is empty".to_owned(),
        });
    }
    if target.len() > MAX_TARGET_BYTES {
        return Err(OpenError::RefusedTarget {
            target: target.chars().take(64).collect(),
            reason: format!("it is longer than {MAX_TARGET_BYTES} bytes"),
        });
    }
    // The characters that would end the quoting `start` is given, and the control characters that
    // would end the line. Nothing a caller legitimately opens contains one.
    if let Some(bad) = target
        .chars()
        .find(|character| matches!(*character, '"' | '\'') || character.is_control())
    {
        return Err(OpenError::RefusedTarget {
            target: target.chars().take(64).collect(),
            reason: format!("it contains {bad:?}, which cannot survive the platform's tool"),
        });
    }
    if let Some(scheme) = scheme_of(target)
        && REFUSED_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str())
    {
        return Err(OpenError::RefusedScheme { scheme });
    }
    Ok(())
}

/// The scheme a target names, if it names one.
///
/// A Windows path begins `C:`, which looks like a scheme and is not one: a scheme is letters and
/// the scheme's punctuation, and a drive letter is followed by a separator.
#[must_use]
pub fn scheme_of(target: &str) -> Option<String> {
    let colon = target.find(':')?;
    let candidate = &target[..colon];
    if candidate.is_empty() || candidate.len() > 32 {
        return None;
    }
    if !candidate
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.'))
    {
        return None;
    }
    // `C:\…` and `C:/…` are paths; `c:relative` is neither a scheme nor a path this module opens.
    if candidate.len() == 1 && target[colon + 1..].starts_with(['\\', '/']) {
        return None;
    }
    Some(candidate.to_owned())
}

/// Opens `target` with whatever the platform uses for it.
///
/// # Errors
///
/// [`OpenError::RefusedTarget`] or [`OpenError::RefusedScheme`] before anything is started,
/// [`OpenError::Unavailable`] when the platform's opener cannot be found, and
/// [`OpenError::Failed`] when it runs and refuses.
pub fn open_with_default(target: &str) -> Result<(), OpenError> {
    check_target(target)?;
    platform_open(target)
}

/// Maps a process failure onto the open error shape.
fn tool_error(tool: &'static str, error: &ProcessError) -> OpenError {
    match error {
        ProcessError::NotFound { .. } => OpenError::Unavailable { tool },
        other => OpenError::Failed {
            tool,
            reason: other.to_string(),
        },
    }
}

/// The first line of a tool's error output, or of its ordinary output when it said nothing else.
fn first_line(stderr: &str, stdout: &str) -> String {
    stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("it reported no reason")
        .chars()
        .take(400)
        .collect()
}

/// The `cmd` line that hands a target to its associated program.
///
/// `start` takes a window title before the target, and an empty title is how a target that is
/// itself quoted is not mistaken for one. The target arrives as one argument of the vector this
/// module builds, and `check_target` has already refused anything that could end the quoting.
#[cfg(windows)]
#[must_use]
pub fn windows_command_line(target: &str) -> String {
    format!("start \"\" \"{target}\"")
}

/// Opens the target through `cmd`'s `start`, which is what the platform's association is for.
#[cfg(windows)]
fn platform_open(target: &str) -> Result<(), OpenError> {
    let outcome = run_command(
        Path::new("cmd"),
        "cmd",
        &["/C".to_owned(), windows_command_line(target)],
        None,
        OPEN_TIMEOUT,
        4096,
    )
    .map_err(|error| tool_error("cmd", &error))?;
    if outcome.status != 0 {
        // The status is included: `start` fails for a target the platform has no handler for, and
        // a refusal that says only "it reported no reason" sends the reader looking in the wrong
        // place — the exit code is the part that says *that* it refused.
        return Err(OpenError::Failed {
            tool: "cmd",
            reason: format!(
                "exit {}: {}",
                outcome.status,
                first_line(&outcome.stderr, &outcome.stdout)
            ),
        });
    }
    Ok(())
}

/// Opens the target through `open`.
#[cfg(target_os = "macos")]
fn platform_open(target: &str) -> Result<(), OpenError> {
    let outcome = run_command(
        Path::new("open"),
        "open",
        &[target.to_owned()],
        None,
        OPEN_TIMEOUT,
        4096,
    )
    .map_err(|error| tool_error("open", &error))?;
    if outcome.status != 0 {
        return Err(OpenError::Failed {
            tool: "open",
            reason: first_line(&outcome.stderr, &outcome.stdout),
        });
    }
    Ok(())
}

/// Opens the target through `xdg-open`.
#[cfg(target_os = "linux")]
fn platform_open(target: &str) -> Result<(), OpenError> {
    let outcome = run_command(
        Path::new("xdg-open"),
        "xdg-open",
        &[target.to_owned()],
        None,
        OPEN_TIMEOUT,
        4096,
    )
    .map_err(|error| tool_error("xdg-open", &error))?;
    if outcome.status != 0 {
        return Err(OpenError::Failed {
            tool: "xdg-open",
            reason: first_line(&outcome.stderr, &outcome.stdout),
        });
    }
    Ok(())
}

/// A platform with no opener this module knows.
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn platform_open(_target: &str) -> Result<(), OpenError> {
    Err(OpenError::Unavailable { tool: "none" })
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
    fn a_target_that_cannot_survive_the_tool_is_refused() {
        // The characters that end the quoting `start` is given. A target containing one would not
        // be opened; it would be a command line with two commands on it.
        for hostile in [
            "https://example.test/\" & calc & \"",
            "https://example.test/'",
            "https://example.test/\ncalc",
            "",
        ] {
            let error = check_target(hostile).expect_err("refused");
            assert_eq!(error.code(), "open_target_refused", "{hostile}");
        }
        assert!(check_target(&"a".repeat(MAX_TARGET_BYTES + 1)).is_err());
        // And what a caller legitimately opens is not.
        for ordinary in [
            "https://example.test/a?b=c",
            "file:///tmp/a%20b.txt",
            "ncui-counter://open/notes",
            "C:\\Users\\someone\\notes.txt", // ncui-allow-hygiene: a Windows path fixture, not a path on any machine
            "/home/someone/notes.txt",
        ] {
            assert!(check_target(ordinary).is_ok(), "{ordinary}");
        }
    }

    #[test]
    fn a_scheme_that_belongs_to_a_browser_is_refused() {
        // Handing `javascript:` to the platform's opener is how an application turns a link into
        // script execution in whatever handles it.
        for hostile in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox",
            "view-source:https://example.test",
        ] {
            let error = check_target(hostile).expect_err("refused");
            assert_eq!(error.code(), "open_scheme_refused", "{hostile}");
            assert!(error.to_string().contains("script execution"), "{error}");
        }
    }

    #[test]
    fn a_windows_path_is_not_a_scheme_and_a_scheme_is_not_a_path() {
        // `C:\…` looks like a scheme and is not one; treating it as one would refuse every
        // Windows path, and treating `javascript:` as a path would open it.
        assert_eq!(scheme_of("C:\\Users\\someone\\notes.txt"), None); // ncui-allow-hygiene: a Windows path fixture, not a path on any machine
        assert_eq!(scheme_of("C:/Users/someone/notes.txt"), None); // ncui-allow-hygiene: a Windows path fixture, not a path on any machine
        assert_eq!(scheme_of("/home/someone/notes.txt"), None);
        assert_eq!(scheme_of("https://example.test"), Some("https".to_owned()));
        assert_eq!(
            scheme_of("ncui-counter://open/notes"),
            Some("ncui-counter".to_owned())
        );
        assert_eq!(
            scheme_of("javascript:alert(1)"),
            Some("javascript".to_owned())
        );
    }

    #[test]
    fn the_target_is_one_argument_of_the_command_the_platform_is_given() {
        // On Windows the tool is `cmd`, so the target passes through a command line: the empty
        // title is what keeps a quoted target from being read as the window title.
        #[cfg(windows)]
        assert_eq!(
            windows_command_line("C:\\Users\\someone\\notes.txt"), // ncui-allow-hygiene: a Windows path fixture, not a path on any machine
            "start \"\" \"C:\\Users\\someone\\notes.txt\"" // ncui-allow-hygiene: a Windows path fixture, not a path on any machine
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_platform_is_asked_and_its_refusal_is_reported() {
        // The tool really runs: a target no handler is registered for is refused by the platform,
        // and the refusal reaches the caller with the exit status that says so.
        //
        // What this deliberately does *not* assert is that a handler runs. Windows resolves a
        // scheme through an association cache the shell refreshes on its own schedule, so a scheme
        // registered during this session is not reliably live in it — a test that asserted the
        // handler ran would be asserting the shell's refresh timing rather than this module.
        // `crate::protocol`'s own test asserts what the platform *holds* for a scheme, which is
        // the part that is deterministic.
        let error = open_with_default("ncui-not-a-registered-scheme://x")
            .expect_err("a scheme nothing handles is refused");
        assert_eq!(error.code(), "open_failed");
        assert!(error.to_string().contains("exit 1"), "{error}");
    }
}
