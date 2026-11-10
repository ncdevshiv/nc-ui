//! URL scheme registration: telling the platform that a link belongs to this application.
//!
//! A deep link is a URL the operating system hands to an application — `myapp://open/notes`. What
//! makes it work is a registration: the platform has to be told that the scheme belongs to this
//! program, and what to run when a link of that shape is clicked. Without it, clicking a link
//! opens a browser's error page, and with it the link arrives as the program's argument vector —
//! which is exactly the shape the single-instance hand-off already carries, so a link reaches the
//! window that is running rather than starting a second one.
//!
//! # What this module will and will not register
//!
//! Registering a scheme is a claim on *every* link of that shape, system-wide, so the schemes a
//! user's machine already depends on are refused outright: `http`, `https`, `file`, `mailto`,
//! `ftp` and the rest of the well-known set. A program that took `https` would not have a deep
//! link; it would have broken the user's browser. The refusal is a list rather than a rule
//! because the list is the honest form of the fact: these names are spoken for.
//!
//! # How it is done without `unsafe`
//!
//! Windows keeps the registration in the user's own hive, under
//! `HKCU\Software\Classes\<scheme>`, and this module writes it with `reg.exe` — the platform's
//! own tool, the same choice the clipboard and the notifier make, and for the same reason.
//! Linux keeps it in a desktop entry and `xdg-mime`. macOS keeps it in the *bundle's*
//! `Info.plist`, which is a file the bundler writes rather than something a running program can
//! do, so [`register`] refuses there and says which file to change instead of pretending.
//!
//! # What is guaranteed
//!
//! * **The scheme is validated, not escaped.** A scheme that is not a scheme is refused before
//!   anything is written, and the command line that receives the link quotes the executable and
//!   the field, so a path with a space cannot become two arguments.
//! * **The registration is readable.** [`registered`] reads back what the platform holds, so a
//!   caller can see what a link will run rather than trust that it was set.
//! * **Removal removes only what this module writes.** [`unregister`] deletes the scheme's key
//!   and nothing else.

use std::path::Path;

use crate::process::{ProcessError, run_command};

/// How long a platform tool may take before it is stopped.
pub const PROTOCOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The longest a scheme may be.
pub const MAX_SCHEME_BYTES: usize = 64;

/// The schemes a user's machine already depends on.
///
/// A registration is a system-wide claim, so these are refused: taking one would not give the
/// application a link, it would take a link away from everything else that uses it.
pub const RESERVED_SCHEMES: &[&str] = &[
    "http",
    "https",
    "file",
    "ftp",
    "ftps",
    "mailto",
    "tel",
    "sms",
    "ssh",
    "ws",
    "wss",
    "data",
    "javascript",
    "about",
    "chrome",
    "edge",
    "ms-appx",
    "ms-appinstaller",
    "shell",
];

/// Why a scheme could not be registered, read or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// The scheme is not a scheme.
    InvalidScheme {
        /// What was given.
        scheme: String,
        /// Why it is not one.
        reason: String,
    },
    /// The scheme is one the platform already uses.
    Reserved {
        /// The scheme.
        scheme: String,
    },
    /// The platform tool is not available.
    Unavailable {
        /// What was looked for.
        tool: &'static str,
    },
    /// The platform does not keep this registration where a running program can write it.
    Unsupported {
        /// Where it is kept instead.
        instead: &'static str,
    },
    /// The tool ran and refused.
    Failed {
        /// The tool.
        tool: &'static str,
        /// What it said.
        reason: String,
    },
}

impl ProtocolError {
    /// The stable code a caller sees on the wire.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidScheme { .. } => "protocol_invalid_scheme",
            Self::Reserved { .. } => "protocol_reserved_scheme",
            Self::Unavailable { .. } => "protocol_unavailable",
            Self::Unsupported { .. } => "protocol_unsupported",
            Self::Failed { .. } => "protocol_failed",
        }
    }
}

impl core::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidScheme { scheme, reason } => {
                write!(f, "`{scheme}` is not a URL scheme: {reason}")
            }
            Self::Reserved { scheme } => write!(
                f,
                "`{scheme}` is a scheme this machine already uses; registering it would take \
                 every such link away from what handles it now"
            ),
            Self::Unavailable { tool } => write!(
                f,
                "this system has no `{tool}`, which is how this platform's registrations are made"
            ),
            Self::Unsupported { instead } => {
                write!(
                    f,
                    "this platform keeps the registration in {instead}, not in a running program's reach"
                )
            }
            Self::Failed { tool, reason } => {
                write!(f, "`{tool}` refused the registration: {reason}")
            }
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Whether `scheme` may be registered.
///
/// # Errors
///
/// [`ProtocolError::InvalidScheme`] for a name that is not a scheme and
/// [`ProtocolError::Reserved`] for one the machine already uses.
pub fn check_scheme(scheme: &str) -> Result<(), ProtocolError> {
    check_scheme_shape(scheme)?;
    if RESERVED_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str()) {
        return Err(ProtocolError::Reserved {
            scheme: scheme.to_owned(),
        });
    }
    Ok(())
}

/// Whether `scheme` has the shape of a scheme, without asking whether it is spoken for.
fn check_scheme_shape(scheme: &str) -> Result<(), ProtocolError> {
    if scheme.is_empty() || scheme.len() > MAX_SCHEME_BYTES {
        return Err(ProtocolError::InvalidScheme {
            scheme: scheme.to_owned(),
            reason: format!("it must be between 1 and {MAX_SCHEME_BYTES} bytes"),
        });
    }
    let mut characters = scheme.chars();
    let first = characters.next().unwrap_or('0');
    if !first.is_ascii_alphabetic() {
        return Err(ProtocolError::InvalidScheme {
            scheme: scheme.to_owned(),
            reason: "a scheme begins with a letter".to_owned(),
        });
    }
    if let Some(bad) =
        characters.find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
    {
        return Err(ProtocolError::InvalidScheme {
            scheme: scheme.to_owned(),
            reason: format!("`{bad}` is not a character a scheme may hold"),
        });
    }
    Ok(())
}

/// The command line a link is handed to, for a program and an argument prefix.
///
/// The executable and every argument are quoted, so a path with a space is one argument rather
/// than two. The link's field is *not* included: how it is written is the platform's rule —
/// platform's rule — Windows quotes it so a link cannot be read as an option, and the
/// desktop-entry specification requires a field code to stand bare — so each platform appends
/// its own.
#[must_use]
pub fn command_line(program: &Path, arguments: &[String]) -> String {
    let mut line = format!("\"{}\"", program.display());
    for argument in arguments {
        line.push(' ');
        line.push('"');
        line.push_str(&argument.replace('"', "\\\""));
        line.push('"');
    }
    line
}

/// The field Windows substitutes the link into, quoted.
#[cfg(windows)]
const LINK_FIELD: &str = " \"%1\"";

/// The field a desktop entry substitutes the link into, bare.
#[cfg(target_os = "linux")]
const LINK_FIELD: &str = " %1";

/// The field where the platform's own form is unknown.
///
/// Only macOS reaches this, and macOS does not register a scheme from a running program at all —
/// but the command line is still a value this module can describe, and a build that could not
/// describe it would fail to compile rather than fail to register.
#[cfg(not(any(windows, target_os = "linux")))]
const LINK_FIELD: &str = " %1";

/// Registers `scheme` as belonging to `program`, run with `arguments` and the link appended.
///
/// # Errors
///
/// [`ProtocolError::InvalidScheme`] or [`ProtocolError::Reserved`] before anything is written,
/// [`ProtocolError::Unsupported`] on a platform whose registration is not a program's to write,
/// and [`ProtocolError::Unavailable`] or [`ProtocolError::Failed`] when the platform tool cannot
/// do it.
pub fn register(scheme: &str, program: &Path, arguments: &[String]) -> Result<(), ProtocolError> {
    check_scheme(scheme)?;
    if !program.is_file() {
        return Err(ProtocolError::Failed {
            tool: "register",
            reason: format!("{} is not a program", program.display()),
        });
    }
    let line = format!("{}{LINK_FIELD}", command_line(program, arguments));
    platform_register(scheme, &line)
}

/// Removes the registration for `scheme`.
///
/// # Errors
///
/// As [`register`], except that a scheme that was never registered is not an error: removing
/// what is not there leaves the machine as the caller wanted it.
pub fn unregister(scheme: &str) -> Result<(), ProtocolError> {
    check_scheme(scheme)?;
    platform_unregister(scheme)
}

/// What the platform holds for `scheme`, or `None` when it holds nothing.
///
/// # Errors
///
/// [`ProtocolError::InvalidScheme`], [`ProtocolError::Unsupported`] or
/// [`ProtocolError::Unavailable`].
pub fn registered(scheme: &str) -> Result<Option<String>, ProtocolError> {
    // Only the shape is checked here: asking what handles `https` is a reasonable question, and
    // refusing it would make this function less useful than the platform it reads.
    check_scheme_shape(scheme)?;
    platform_registered(scheme)
}

/// Maps a process failure onto the protocol error shape.
fn tool_error(tool: &'static str, error: &ProcessError) -> ProtocolError {
    match error {
        ProcessError::NotFound { .. } => ProtocolError::Unavailable { tool },
        other => ProtocolError::Failed {
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

#[cfg(windows)]
const REG: &str = "reg";

/// Registers the scheme in the user's own hive.
#[cfg(windows)]
fn platform_register(scheme: &str, line: &str) -> Result<(), ProtocolError> {
    let key = format!("HKCU\\Software\\Classes\\{scheme}");
    // `URL Protocol` is the value that tells Windows this is a link handler rather than a file
    // type; an empty one is what the platform expects and is not a mistake.
    for (name, value) in [("URL Protocol", ""), ("", line)] {
        let mut arguments = vec!["add".to_owned(), key.clone(), "/f".to_owned()];
        if name.is_empty() {
            arguments.push("/ve".to_owned());
        } else {
            arguments.push("/v".to_owned());
            arguments.push(name.to_owned());
        }
        arguments.push("/t".to_owned());
        arguments.push("REG_SZ".to_owned());
        arguments.push("/d".to_owned());
        arguments.push(value.to_owned());
        let outcome = run_command(
            Path::new(REG),
            REG,
            &arguments,
            None,
            PROTOCOL_TIMEOUT,
            1024,
        )
        .map_err(|error| tool_error(REG, &error))?;
        if outcome.status != 0 {
            return Err(ProtocolError::Failed {
                tool: REG,
                reason: first_line(&outcome.stderr, &outcome.stdout),
            });
        }
    }
    // The command Windows runs lives one level deeper: `shell\open\command`.
    let command_key = format!("{key}\\shell\\open\\command");
    let outcome = run_command(
        Path::new(REG),
        REG,
        &[
            "add".to_owned(),
            command_key,
            "/ve".to_owned(),
            "/t".to_owned(),
            "REG_SZ".to_owned(),
            "/d".to_owned(),
            line.to_owned(),
            "/f".to_owned(),
        ],
        None,
        PROTOCOL_TIMEOUT,
        1024,
    )
    .map_err(|error| tool_error(REG, &error))?;
    if outcome.status != 0 {
        return Err(ProtocolError::Failed {
            tool: REG,
            reason: first_line(&outcome.stderr, &outcome.stdout),
        });
    }
    Ok(())
}

/// Removes the scheme's key from the user's hive.
#[cfg(windows)]
fn platform_unregister(scheme: &str) -> Result<(), ProtocolError> {
    let key = format!("HKCU\\Software\\Classes\\{scheme}");
    let outcome = run_command(
        Path::new(REG),
        REG,
        &["delete".to_owned(), key, "/f".to_owned()],
        None,
        PROTOCOL_TIMEOUT,
        1024,
    )
    .map_err(|error| tool_error(REG, &error))?;
    // A key that was not there is the state the caller asked for.
    if outcome.status != 0 && !outcome.stderr.contains("unable to find") {
        return Err(ProtocolError::Failed {
            tool: REG,
            reason: first_line(&outcome.stderr, &outcome.stdout),
        });
    }
    Ok(())
}

/// Reads back the command Windows would run for a link of this scheme.
#[cfg(windows)]
fn platform_registered(scheme: &str) -> Result<Option<String>, ProtocolError> {
    let key = format!("HKCU\\Software\\Classes\\{scheme}\\shell\\open\\command");
    let outcome = run_command(
        Path::new(REG),
        REG,
        &["query".to_owned(), key, "/ve".to_owned()],
        None,
        PROTOCOL_TIMEOUT,
        4096,
    )
    .map_err(|error| tool_error(REG, &error))?;
    if outcome.status != 0 {
        return Ok(None);
    }
    // `reg query` prints a table; the value is what follows the type name.
    Ok(outcome
        .stdout
        .lines()
        .find(|line| line.contains("REG_SZ"))
        .and_then(|line| line.split("REG_SZ").nth(1))
        .map(|value| value.trim().to_owned()))
}

/// Registers the scheme through a desktop entry and `xdg-mime`.
#[cfg(target_os = "linux")]
fn platform_register(scheme: &str, line: &str) -> Result<(), ProtocolError> {
    let applications = linux_applications_dir()?;
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=NC-UI link handler\nExec={line}\n\
         NoDisplay=true\nTerminal=false\nMimeType=x-scheme-handler/{scheme};\n"
    );
    let path = applications.join(format!("ncui-{scheme}.desktop"));
    std::fs::write(&path, entry).map_err(|error| ProtocolError::Failed {
        tool: "desktop entry",
        reason: format!("{}: {error}", path.display()),
    })?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let outcome = run_command(
        Path::new("xdg-mime"),
        "xdg-mime",
        &[
            "default".to_owned(),
            name,
            format!("x-scheme-handler/{scheme}"),
        ],
        None,
        PROTOCOL_TIMEOUT,
        1024,
    )
    .map_err(|error| tool_error("xdg-mime", &error))?;
    if outcome.status != 0 {
        return Err(ProtocolError::Failed {
            tool: "xdg-mime",
            reason: first_line(&outcome.stderr, &outcome.stdout),
        });
    }
    Ok(())
}

/// Removes the desktop entry and the association.
#[cfg(target_os = "linux")]
fn platform_unregister(scheme: &str) -> Result<(), ProtocolError> {
    let path = linux_applications_dir()?.join(format!("ncui-{scheme}.desktop"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ProtocolError::Failed {
            tool: "desktop entry",
            reason: format!("{}: {error}", path.display()),
        }),
    }
}

/// Reads back the desktop entry a scheme is handled by.
#[cfg(target_os = "linux")]
fn platform_registered(scheme: &str) -> Result<Option<String>, ProtocolError> {
    let path = linux_applications_dir()?.join(format!("ncui-{scheme}.desktop"));
    match std::fs::read_to_string(&path) {
        Ok(entry) => Ok(entry
            .lines()
            .find_map(|line| line.strip_prefix("Exec=").map(str::to_owned))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ProtocolError::Failed {
            tool: "desktop entry",
            reason: format!("{}: {error}", path.display()),
        }),
    }
}

/// Where a user's desktop entries live.
#[cfg(target_os = "linux")]
fn linux_applications_dir() -> Result<std::path::PathBuf, ProtocolError> {
    let home = std::env::var("HOME").map_err(|_| ProtocolError::Failed {
        tool: "desktop entry",
        reason: "HOME is not set, so the applications directory is unknown".to_owned(),
    })?;
    let directory = Path::new(&home).join(".local/share/applications");
    std::fs::create_dir_all(&directory).map_err(|error| ProtocolError::Failed {
        tool: "desktop entry",
        reason: format!("{}: {error}", directory.display()),
    })?;
    Ok(directory)
}

/// macOS keeps a scheme in the bundle's `Info.plist`.
#[cfg(target_os = "macos")]
fn platform_register(_scheme: &str, _line: &str) -> Result<(), ProtocolError> {
    Err(ProtocolError::Unsupported {
        instead: "the bundle's Info.plist, which the bundler writes",
    })
}

/// macOS keeps a scheme in the bundle's `Info.plist`.
#[cfg(target_os = "macos")]
fn platform_unregister(_scheme: &str) -> Result<(), ProtocolError> {
    Err(ProtocolError::Unsupported {
        instead: "the bundle's Info.plist, which the bundler writes",
    })
}

/// macOS keeps a scheme in the bundle's `Info.plist`.
#[cfg(target_os = "macos")]
fn platform_registered(_scheme: &str) -> Result<Option<String>, ProtocolError> {
    Err(ProtocolError::Unsupported {
        instead: "the bundle's Info.plist, which the bundler writes",
    })
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
    fn a_scheme_that_is_not_a_scheme_is_refused() {
        for scheme in [
            "",
            "1app",
            "app://",
            "a b",
            "app/x",
            "-app",
            "a".repeat(65).as_str(),
        ] {
            let error = check_scheme(scheme).expect_err("not a scheme");
            assert_eq!(error.code(), "protocol_invalid_scheme", "{scheme}");
        }
        // The ones that are schemes are accepted.
        for scheme in ["ncui", "my-app", "app.v2", "x+y"] {
            assert!(check_scheme(scheme).is_ok(), "{scheme}");
        }
    }

    #[test]
    fn a_scheme_the_machine_already_uses_is_refused() {
        // Registering `https` would not give this application a link; it would take every link
        // the user has away from the browser that handles them.
        for scheme in ["http", "https", "HTTPS", "mailto", "file"] {
            let error = check_scheme(scheme).expect_err("reserved");
            assert_eq!(error.code(), "protocol_reserved_scheme", "{scheme}");
            assert!(error.to_string().contains("already uses"), "{error}");
        }
        // Reading what handles one of them is still allowed: that is a question about the
        // machine rather than a claim on it.
        assert!(check_scheme_shape("https").is_ok());
    }

    #[test]
    fn the_command_line_quotes_the_program_and_the_link() {
        let base = command_line(
            Path::new("C:\\Program Files\\NC-UI\\ncui.exe"),
            &["--app".to_owned(), "C:\\My Apps\\counter".to_owned()],
        );
        assert_eq!(
            base,
            "\"C:\\Program Files\\NC-UI\\ncui.exe\" \"--app\" \"C:\\My Apps\\counter\""
        );
        // Every argument is quoted, so a path with a space is one argument rather than two.
        assert_eq!(base.matches('"').count(), 6);
        // The link's field is the platform's own form: quoted on Windows, so a link cannot be read
        // as an option, and bare in a desktop entry, which requires a field code to stand unquoted.
        // Each platform asserts its own form, because asserting one of them everywhere is what made
        // this test fail on the platform it was not written for.
        #[cfg(windows)]
        assert_eq!(format!("{base}{LINK_FIELD}").matches('"').count(), 8);
        #[cfg(target_os = "linux")]
        {
            let field = format!("{base}{LINK_FIELD}");
            assert!(
                field.ends_with(" %1"),
                "a desktop entry's field code stands unquoted: {field}"
            );
            assert_eq!(
                field.matches('"').count(),
                6,
                "the link itself is not quoted in a desktop entry: {field}"
            );
        }
    }

    #[test]
    fn a_quote_inside_an_argument_cannot_end_it() {
        let line = command_line(Path::new("ncui"), &["a\"b".to_owned()]);
        assert_eq!(line, "\"ncui\" \"a\\\"b\"");
    }

    #[cfg(windows)]
    #[test]
    fn a_registration_can_be_written_read_back_and_removed() {
        // The real path on this platform: the user's own hive, the platform's own tool, and a
        // scheme this test owns so it cannot collide with anything the machine uses.
        let scheme = "ncui-test-protocol";
        let program = std::env::current_exe().expect("the test binary has a path");
        unregister(scheme).expect("nothing to remove is not a failure");

        register(
            scheme,
            &program,
            &["--app".to_owned(), "somewhere".to_owned()],
        )
        .expect("the scheme registers");
        let registered_line = registered(scheme)
            .expect("the registration is readable")
            .expect("the registration is there");
        assert!(
            registered_line.contains(&program.display().to_string()),
            "what a link runs must name this program: {registered_line}"
        );
        assert!(registered_line.ends_with("\"%1\""), "{registered_line}");

        unregister(scheme).expect("the scheme is removed");
        assert_eq!(
            registered(scheme).expect("still readable"),
            None,
            "a removed registration is gone"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_reserved_scheme_is_refused_before_the_platform_is_touched() {
        let program = std::env::current_exe().expect("the test binary has a path");
        let error = register("https", &program, &[]).expect_err("reserved");
        assert_eq!(error.code(), "protocol_reserved_scheme");
    }
}
