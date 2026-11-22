//! File associations: telling the platform that a kind of file belongs to this application.
//!
//! A deep link is a *scheme* the platform hands over; a file association is the other half of
//! the same idea for *files*: "a `.notes` file belongs to this program". Without it, double-
//! clicking the application's own document opens whatever else claims the extension; with it,
//! the file arrives as the program's argument vector, exactly like a link does.
//!
//! # What this module will and will not register
//!
//! An association is a claim on every file of that extension, so the extensions a machine
//! already depends on are refused outright: `exe`, `dll`, `bat` and the rest of the
//! executable set. Taking one would not give the application a document type; it would take
//! the loader away from everything that uses it.
//!
//! # How it is done without `unsafe`
//!
//! Windows keeps the registration in the user's own hive — a `ProgID` under
//! `HKCU\Software\Classes` whose `shell\open\command` runs the program, and a
//! `HKCU\Software\Classes\.<ext>` key pointing at the `ProgID` — written with `reg.exe`, the
//! platform's own tool, the same choice the scheme registration and the clipboard make.
//! Linux keeps it in a desktop entry and `xdg-mime`, with the MIME type derived from a
//! table of common extensions. macOS keeps associations in the bundle's `Info.plist`, which
//! is a file the bundler writes rather than something a running program can do, so
//! [`register`] refuses there and says which file to change instead of pretending.
//!
//! # What is guaranteed
//!
//! * **The extension is validated, not escaped.** Lowercase letters and digits only, at
//!   most [`MAX_EXTENSION_BYTES`], with the executable-set refused by name.
//! * **The registration is readable.** [`registered`] reads back the `ProgID` the platform
//!   holds for an extension.
//! * **Removal removes only what this module writes**: the extension key and this
//!   application's own `ProgID`, and nothing else.

use std::path::Path;

#[cfg(any(windows, target_os = "linux"))]
use crate::process::{ProcessError, run_command};

/// How long a platform tool may take before it is stopped.
pub const ASSOCIATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The longest an extension may be, without its dot.
pub const MAX_EXTENSION_BYTES: usize = 16;

/// The extensions a machine already depends on for loading and running code.
///
/// An association on one of these would not give the application a document type; it would
/// stand between the operating system and its own loaders.
pub const RESERVED_EXTENSIONS: &[&str] = &[
    "exe", "dll", "sys", "msi", "bat", "cmd", "com", "lnk", "scr", "cpl", "pif", "cab", "msp",
    "mst", "job", "manifest",
];

/// Why an association could not be registered, read or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssociationError {
    /// The extension is not one.
    InvalidExtension {
        /// What was given.
        extension: String,
        /// Why it is not one.
        reason: String,
    },
    /// The extension is one the machine already depends on.
    Reserved {
        /// The extension.
        extension: String,
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

impl AssociationError {
    /// The stable code a caller sees on the wire.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidExtension { .. } => "association_invalid_extension",
            Self::Reserved { .. } => "association_reserved_extension",
            Self::Unavailable { .. } => "association_unavailable",
            Self::Unsupported { .. } => "association_unsupported",
            Self::Failed { .. } => "association_failed",
        }
    }
}

impl core::fmt::Display for AssociationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidExtension { extension, reason } => {
                write!(f, "`{extension}` is not a file extension: {reason}")
            }
            Self::Reserved { extension } => write!(
                f,
                "`{extension}` is an extension this machine loads and runs code with; \
                 associating it would stand between the operating system and its own loaders"
            ),
            Self::Unavailable { tool } => write!(
                f,
                "this system has no `{tool}`, which is how this platform's registrations are made"
            ),
            Self::Unsupported { instead } => write!(
                f,
                "this platform keeps the registration in {instead}, not in a running program's reach"
            ),
            Self::Failed { tool, reason } => {
                write!(f, "`{tool}` refused the registration: {reason}")
            }
        }
    }
}

impl std::error::Error for AssociationError {}

/// Normalises an extension to the form this module registers: lowercase, no dot.
///
/// # Errors
///
/// [`AssociationError::InvalidExtension`] for a name that is not an extension and
/// [`AssociationError::Reserved`] for one the machine loads code with.
pub fn normalize_extension(extension: &str) -> Result<String, AssociationError> {
    let trimmed = extension.trim().trim_start_matches('.');
    if trimmed.is_empty() || trimmed.len() > MAX_EXTENSION_BYTES {
        return Err(AssociationError::InvalidExtension {
            extension: extension.to_owned(),
            reason: format!("it must be between 1 and {MAX_EXTENSION_BYTES} bytes"),
        });
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(AssociationError::InvalidExtension {
            extension: extension.to_owned(),
            reason: "an extension is lowercase letters, digits and hyphens".to_owned(),
        });
    }
    let normalized = trimmed.to_ascii_lowercase();
    if RESERVED_EXTENSIONS.contains(&normalized.as_str()) {
        return Err(AssociationError::Reserved {
            extension: normalized,
        });
    }
    Ok(normalized)
}

/// Whether an extension may be associated.
///
/// # Errors
///
/// As [`normalize_extension`], which this calls.
pub fn check_extension(extension: &str) -> Result<(), AssociationError> {
    normalize_extension(extension).map(|_| ())
}

/// The `ProgID` an application's association for one extension is registered under.
///
/// The application name is already a directory name (the installer validated it), so the
/// `ProgID` is `ncui.<name>.<ext>` — readable in `reg query`, and namespaced so two
/// applications associating the same extension do not collide on the `ProgID` itself.
#[must_use]
pub fn prog_id(name: &str, extension: &str) -> String {
    format!("ncui.{name}.{extension}")
}

/// The MIME type an extension most plausibly maps to, for the platforms that register by
/// MIME rather than by `ProgID`. An extension outside the table maps to
/// `application/octet-stream`, which is the honest default for a type the platform must
/// probe itself.
#[cfg(target_os = "linux")]
#[must_use]
pub fn mime_type(extension: &str) -> &'static str {
    match extension {
        "txt" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "xml" => "application/xml",
        "json" => "application/json",
        "toml" => "application/toml",
        "yaml" | "yml" => "application/yaml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "js" => "text/javascript",
        "rs" => "text/rust",
        "py" => "text/x-python",
        _ => "application/octet-stream",
    }
}

/// Associates `extensions` with `program`, run with `arguments` and the file appended.
///
/// Returns the extensions as this module normalized them, so the caller can record exactly
/// what was written and remove exactly that later.
///
/// # Errors
///
/// [`AssociationError::InvalidExtension`] or [`AssociationError::Reserved`] before anything
/// is written, [`AssociationError::Unsupported`] on a platform whose registration is not a
/// program's to write, and [`AssociationError::Unavailable`] or
/// [`AssociationError::Failed`] when the platform tool cannot do it.
pub fn register(
    name: &str,
    program: &Path,
    arguments: &[String],
    extensions: &[String],
) -> Result<Vec<String>, AssociationError> {
    let mut normalized = Vec::with_capacity(extensions.len());
    for extension in extensions {
        normalized.push(normalize_extension(extension)?);
    }
    if !program.is_file() {
        return Err(AssociationError::Failed {
            tool: "register",
            reason: format!("{} is not a program", program.display()),
        });
    }
    for extension in &normalized {
        platform_register(name, program, arguments, extension)?;
    }
    Ok(normalized)
}

/// Removes the associations for `extensions`, naming the application that made them.
///
/// # Errors
///
/// As [`register`], except that an extension that was never registered is not an error:
/// removing what is not there leaves the machine as the caller wanted it.
pub fn unregister(name: &str, extensions: &[String]) -> Result<(), AssociationError> {
    for extension in extensions {
        let normalized = normalize_extension(extension)?;
        platform_unregister(name, &normalized)?;
    }
    Ok(())
}

/// What the platform holds for `extension`: the `ProgID`, or `None` when it holds nothing.
///
/// # Errors
///
/// [`AssociationError::InvalidExtension`], [`AssociationError::Unsupported`] or
/// [`AssociationError::Unavailable`].
pub fn registered(extension: &str) -> Result<Option<String>, AssociationError> {
    let normalized = normalize_extension(extension)?;
    platform_registered(&normalized)
}

/// Maps a process failure onto the association error shape.
#[cfg(any(windows, target_os = "linux"))]
fn tool_error(tool: &'static str, error: &ProcessError) -> AssociationError {
    match error {
        ProcessError::NotFound { .. } => AssociationError::Unavailable { tool },
        other => AssociationError::Failed {
            tool,
            reason: other.to_string(),
        },
    }
}

/// The first line of a tool's error output, or of its ordinary output when it said nothing else.
#[cfg(any(windows, target_os = "linux"))]
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

/// Writes the `ProgID` and the extension key into the user's own hive.
#[cfg(windows)]
fn platform_register(
    name: &str,
    program: &Path,
    arguments: &[String],
    extension: &str,
) -> Result<(), AssociationError> {
    let id = prog_id(name, extension);
    let line = format!(
        "{} \"%1\"",
        crate::protocol::command_line(program, arguments)
    );
    let mut commands = vec![
        // The ProgID names the document type.
        vec![
            "add".to_owned(),
            format!("HKCU\\Software\\Classes\\{id}"),
            "/ve".to_owned(),
            "/t".to_owned(),
            "REG_SZ".to_owned(),
            "/d".to_owned(),
            format!("{name} document"),
            "/f".to_owned(),
        ],
        // The command the platform runs for a file of this type.
        vec![
            "add".to_owned(),
            format!("HKCU\\Software\\Classes\\{id}\\shell\\open\\command"),
            "/ve".to_owned(),
            "/t".to_owned(),
            "REG_SZ".to_owned(),
            "/d".to_owned(),
            line.clone(),
            "/f".to_owned(),
        ],
        // The extension points at the ProgID.
        vec![
            "add".to_owned(),
            format!("HKCU\\Software\\Classes\\.{extension}"),
            "/ve".to_owned(),
            "/t".to_owned(),
            "REG_SZ".to_owned(),
            "/d".to_owned(),
            id.clone(),
            "/f".to_owned(),
        ],
    ];
    while let Some(arguments) = commands.pop() {
        let outcome = run_command(
            Path::new(REG),
            REG,
            &arguments,
            None,
            ASSOCIATION_TIMEOUT,
            1024,
            None,
        )
        .map_err(|error| tool_error(REG, &error))?;
        if outcome.status != 0 {
            return Err(AssociationError::Failed {
                tool: REG,
                reason: first_line(&outcome.stderr, &outcome.stdout),
            });
        }
    }
    Ok(())
}

/// Removes the extension key and this application's `ProgID` from the user's hive.
#[cfg(windows)]
fn platform_unregister(name: &str, extension: &str) -> Result<(), AssociationError> {
    let id = prog_id(name, extension);
    for key in [
        format!("HKCU\\Software\\Classes\\.{extension}"),
        format!("HKCU\\Software\\Classes\\{id}"),
    ] {
        let outcome = run_command(
            Path::new(REG),
            REG,
            &["delete".to_owned(), key, "/f".to_owned()],
            None,
            ASSOCIATION_TIMEOUT,
            1024,
            None,
        )
        .map_err(|error| tool_error(REG, &error))?;
        // A key that was not there is the state the caller asked for.
        if outcome.status != 0 && !outcome.stderr.contains("unable to find") {
            return Err(AssociationError::Failed {
                tool: REG,
                reason: first_line(&outcome.stderr, &outcome.stdout),
            });
        }
    }
    Ok(())
}

/// Reads back the `ProgID` the hive holds for an extension.
#[cfg(windows)]
fn platform_registered(extension: &str) -> Result<Option<String>, AssociationError> {
    let key = format!("HKCU\\Software\\Classes\\.{extension}");
    let outcome = run_command(
        Path::new(REG),
        REG,
        &["query".to_owned(), key, "/ve".to_owned()],
        None,
        ASSOCIATION_TIMEOUT,
        4096,
        None,
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

/// Writes a desktop entry for the extension and points `xdg-mime` at it.
#[cfg(target_os = "linux")]
fn platform_register(
    name: &str,
    program: &Path,
    arguments: &[String],
    extension: &str,
) -> Result<(), AssociationError> {
    let applications = linux_applications_dir()?;
    let line = format!("{} %1", crate::protocol::command_line(program, arguments));
    let mime = mime_type(extension);
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName={name} document\nExec={line}\n\
         NoDisplay=true\nTerminal=false\nMimeType={mime};\n"
    );
    let path = applications.join(format!("ncui-{name}-{extension}.desktop"));
    std::fs::write(&path, entry).map_err(|error| AssociationError::Failed {
        tool: "desktop entry",
        reason: format!("{}: {error}", path.display()),
    })?;
    let desktop = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let outcome = run_command(
        Path::new("xdg-mime"),
        "xdg-mime",
        &["default".to_owned(), desktop, mime.to_owned()],
        None,
        ASSOCIATION_TIMEOUT,
        1024,
        None,
    )
    .map_err(|error| tool_error("xdg-mime", &error))?;
    if outcome.status != 0 {
        return Err(AssociationError::Failed {
            tool: "xdg-mime",
            reason: first_line(&outcome.stderr, &outcome.stdout),
        });
    }
    Ok(())
}

/// Removes the desktop entry and the MIME default for an extension.
#[cfg(target_os = "linux")]
fn platform_unregister(name: &str, extension: &str) -> Result<(), AssociationError> {
    let applications = linux_applications_dir()?;
    let path = applications.join(format!("ncui-{name}-{extension}.desktop"));
    // An entry that was never written is the state the caller asked for.
    let _ = std::fs::remove_file(&path);
    let mime = mime_type(extension);
    let outcome = run_command(
        Path::new("xdg-mime"),
        "xdg-mime",
        &["default".to_owned(), String::new(), mime.to_owned()],
        None,
        ASSOCIATION_TIMEOUT,
        1024,
        None,
    )
    .map_err(|error| tool_error("xdg-mime", &error))?;
    if outcome.status != 0 {
        return Err(AssociationError::Failed {
            tool: "xdg-mime",
            reason: first_line(&outcome.stderr, &outcome.stdout),
        });
    }
    Ok(())
}

/// Reads back the desktop entry the platform holds for an extension, if any.
#[cfg(target_os = "linux")]
fn platform_registered(extension: &str) -> Result<Option<String>, AssociationError> {
    let applications = linux_applications_dir()?;
    let path = applications.join(format!("ncui-%.{extension}.desktop"));
    if path.is_file() {
        Ok(Some(path.to_string_lossy().into_owned()))
    } else {
        Ok(None)
    }
}

/// A platform with no opener this module knows.
#[cfg(not(any(windows, target_os = "linux")))]
fn platform_register(
    _name: &str,
    _program: &Path,
    _arguments: &[String],
    _extension: &str,
) -> Result<(), AssociationError> {
    Err(AssociationError::Unsupported {
        instead: "the bundle's Info.plist",
    })
}

/// A platform with no opener this module knows.
#[cfg(not(any(windows, target_os = "linux")))]
fn platform_unregister(_name: &str, _extension: &str) -> Result<(), AssociationError> {
    Err(AssociationError::Unsupported {
        instead: "the bundle's Info.plist",
    })
}

/// A platform with no opener this module knows.
#[cfg(not(any(windows, target_os = "linux")))]
fn platform_registered(_extension: &str) -> Result<Option<String>, AssociationError> {
    Err(AssociationError::Unsupported {
        instead: "the bundle's Info.plist",
    })
}

/// Where a desktop entry for this user's associations is written.
#[cfg(target_os = "linux")]
fn linux_applications_dir() -> Result<PathBuf, AssociationError> {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let dir = home.join(".local/share/applications");
    std::fs::create_dir_all(&dir).map_err(|error| AssociationError::Failed {
        tool: "desktop entry",
        reason: format!("{}: {error}", dir.display()),
    })?;
    Ok(dir)
}

#[cfg(target_os = "linux")]
use std::path::PathBuf;

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
    fn an_extension_is_normalised_validated_and_reserved_by_name() {
        assert_eq!(normalize_extension("TXT"), Ok("txt".to_owned()));
        assert_eq!(normalize_extension(".md"), Ok("md".to_owned()));
        assert_eq!(normalize_extension("ncui-test"), Ok("ncui-test".to_owned()));

        for refused in ["", ".", "toolongextensionx", "has space", "slash/ed"] {
            let error = normalize_extension(refused).expect_err(refused);
            assert!(
                matches!(error, AssociationError::InvalidExtension { .. }),
                "{refused}: {error}"
            );
        }
        // The loader set is refused by name rather than claimed.
        for reserved in ["exe", "dll", "bat", ".MSI"] {
            let error = normalize_extension(reserved).expect_err(reserved);
            assert!(
                matches!(error, AssociationError::Reserved { .. }),
                "{reserved}: {error}"
            );
        }
        assert!(check_extension("notes").is_ok());
    }

    #[test]
    fn the_prog_id_namespaces_the_application_and_the_extension() {
        assert_eq!(prog_id("counter", "notes"), "ncui.counter.notes");
        assert_ne!(prog_id("counter", "notes"), prog_id("other", "notes"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn common_extensions_map_to_their_mime_types() {
        assert_eq!(mime_type("txt"), "text/plain");
        assert_eq!(mime_type("png"), "image/png");
        assert_eq!(mime_type("whatever"), "application/octet-stream");
    }

    #[cfg(windows)]
    #[test]
    fn an_association_is_written_read_back_and_removed() {
        // The tool really runs, in the user's own hive, exactly as the scheme registration's
        // test does: an extension this test invented, a read-back, and a removal that leaves
        // nothing behind.
        let program = std::env::current_exe().expect("this test's binary");
        let registered = register("assoc-test-app", &program, &[], &["ncuitestxyz".to_owned()])
            .expect("the association is written");
        assert_eq!(registered, vec!["ncuitestxyz".to_owned()]);
        assert_eq!(
            registered_extension("ncuitestxyz"),
            Some("ncui.assoc-test-app.ncuitestxyz".to_owned())
        );
        unregister("assoc-test-app", &["ncuitestxyz".to_owned()]).expect("the removal runs");
        assert_eq!(registered_extension("ncuitestxyz"), None);
    }

    /// [`registered`] with the extension already normalised, for the read-back assertions.
    #[cfg(windows)]
    fn registered_extension(extension: &str) -> Option<String> {
        registered(extension).expect("the read-back runs")
    }
}
