//! The identity an application is registered under.
//!
//! Windows delivers a notification against an *application identity* — an `AppUserModelID` — and
//! drops one whose identity it does not know. The identity is therefore part of installing an
//! application rather than something a running program can invent, and this module is the one
//! place it is validated and written.
//!
//! # Why the value is validated rather than escaped
//!
//! It is written into a registry key and, in a bundle, into a generated script. Both are places
//! where a character that is not part of an identity can change what the value *means* rather
//! than what it says, so the module keeps an allow-list: letters, digits and the punctuation an
//! identity is made of. There is no legitimate identity that needs anything else, which is what
//! makes an allow-list the right shape here and escaping the wrong one.
//!
//! # What is not promised
//!
//! That the identity is *displayed* as the application's name. Windows shows whatever the
//! registration's `DisplayName` says, and a notification's own text is the application's
//! business. What this module promises is narrower and checkable: a link's or a toast's identity
//! is the one the application registered, and [`registered`] reads back what the platform holds.

// Used by the Windows arm only: the other platforms register an identity by writing a desktop
// entry, and reach no tool for it.
#[cfg(windows)]
use std::path::Path;

#[cfg(windows)]
use crate::process::{ProcessError, run_command};

/// How long a platform tool may take before it is stopped.
pub const IDENTITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The longest an identity may be.
pub const MAX_IDENTITY_BYTES: usize = 256;

/// Why an identity could not be validated, written or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityError {
    /// The value is not an application identity.
    Invalid {
        /// What was given.
        identity: String,
        /// Why it is not one.
        reason: String,
    },
    /// The platform keeps no such thing.
    Unsupported {
        /// What it keeps instead, if anything.
        instead: &'static str,
    },
    /// The platform tool is not available.
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

impl IdentityError {
    /// The stable code a caller sees on the wire.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => "identity_invalid",
            Self::Unsupported { .. } => "identity_unsupported",
            Self::Unavailable { .. } => "identity_unavailable",
            Self::Failed { .. } => "identity_failed",
        }
    }
}

impl core::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Invalid { identity, reason } => {
                write!(f, "`{identity}` is not an application identity: {reason}")
            }
            Self::Unsupported { instead } => {
                write!(f, "this platform has no application identity; {instead}")
            }
            Self::Unavailable { tool } => write!(
                f,
                "this system has no `{tool}`, which is how this platform's identities are written"
            ),
            Self::Failed { tool, reason } => write!(f, "`{tool}` refused: {reason}"),
        }
    }
}

impl std::error::Error for IdentityError {}

/// Whether `identity` is an application identity.
///
/// # Errors
///
/// [`IdentityError::Invalid`] naming the first character that is not part of one.
pub fn check(identity: &str) -> Result<(), IdentityError> {
    if identity.is_empty() || identity.len() > MAX_IDENTITY_BYTES {
        return Err(IdentityError::Invalid {
            identity: identity.to_owned(),
            reason: format!("it must be between 1 and {MAX_IDENTITY_BYTES} bytes"),
        });
    }
    if let Some(bad) = identity.chars().find(|character| {
        !(character.is_ascii_alphanumeric()
            || matches!(
                character,
                '{' | '}' | '\\' | '.' | '-' | '_' | ' ' | '!' | '~'
            ))
    }) {
        return Err(IdentityError::Invalid {
            identity: identity.to_owned(),
            reason: format!("`{bad}` is not a character an identity may hold"),
        });
    }
    Ok(())
}

/// Whether this platform keeps an application identity at all.
#[must_use]
pub const fn is_supported() -> bool {
    cfg!(windows)
}

/// Registers `identity` under `display_name`.
///
/// # Errors
///
/// [`IdentityError::Invalid`] before anything is written,
/// [`IdentityError::Unsupported`] on a platform that keeps no such thing, and
/// [`IdentityError::Unavailable`] or [`IdentityError::Failed`] when the platform tool cannot do
/// it.
pub fn register(identity: &str, display_name: &str) -> Result<(), IdentityError> {
    check(identity)?;
    if display_name.contains('"') || display_name.contains('\\') {
        return Err(IdentityError::Invalid {
            identity: display_name.to_owned(),
            reason: "a display name is a name, not a path or a quoted string".to_owned(),
        });
    }
    platform_register(identity, display_name)
}

/// Removes the registration for `identity`.
///
/// # Errors
///
/// As [`register`], except that an identity that was never registered is not an error: removing
/// what is not there leaves the machine as the caller wanted it.
pub fn unregister(identity: &str) -> Result<(), IdentityError> {
    check(identity)?;
    platform_unregister(identity)
}

/// The name the platform shows for `identity`, or `None` when it holds nothing.
///
/// # Errors
///
/// [`IdentityError::Invalid`], [`IdentityError::Unsupported`] or [`IdentityError::Unavailable`].
pub fn registered(identity: &str) -> Result<Option<String>, IdentityError> {
    check(identity)?;
    platform_registered(identity)
}

/// Maps a process failure onto the identity error shape.
#[cfg(windows)]
fn tool_error(tool: &'static str, error: &ProcessError) -> IdentityError {
    match error {
        ProcessError::NotFound { .. } => IdentityError::Unavailable { tool },
        other => IdentityError::Failed {
            tool,
            reason: other.to_string(),
        },
    }
}

#[cfg(windows)]
const REG: &str = "reg";

/// Writes the identity under the user's own hive.
#[cfg(windows)]
fn platform_register(identity: &str, display_name: &str) -> Result<(), IdentityError> {
    let key = format!("HKCU\\Software\\Classes\\AppUserModelId\\{identity}");
    let outcome = run_command(
        Path::new(REG),
        REG,
        &[
            "add".to_owned(),
            key,
            "/v".to_owned(),
            "DisplayName".to_owned(),
            "/t".to_owned(),
            "REG_SZ".to_owned(),
            "/d".to_owned(),
            display_name.to_owned(),
            "/f".to_owned(),
        ],
        None,
        IDENTITY_TIMEOUT,
        1024,
    )
    .map_err(|error| tool_error(REG, &error))?;
    if outcome.status != 0 {
        return Err(IdentityError::Failed {
            tool: REG,
            reason: first_line(&outcome.stderr, &outcome.stdout),
        });
    }
    Ok(())
}

/// Removes the identity's key.
#[cfg(windows)]
fn platform_unregister(identity: &str) -> Result<(), IdentityError> {
    let key = format!("HKCU\\Software\\Classes\\AppUserModelId\\{identity}");
    let outcome = run_command(
        Path::new(REG),
        REG,
        &["delete".to_owned(), key, "/f".to_owned()],
        None,
        IDENTITY_TIMEOUT,
        1024,
    )
    .map_err(|error| tool_error(REG, &error))?;
    if outcome.status != 0 && !outcome.stderr.contains("unable to find") {
        return Err(IdentityError::Failed {
            tool: REG,
            reason: first_line(&outcome.stderr, &outcome.stdout),
        });
    }
    Ok(())
}

/// Reads back the name the platform shows for the identity.
#[cfg(windows)]
fn platform_registered(identity: &str) -> Result<Option<String>, IdentityError> {
    let key = format!("HKCU\\Software\\Classes\\AppUserModelId\\{identity}");
    let outcome = run_command(
        Path::new(REG),
        REG,
        &[
            "query".to_owned(),
            key,
            "/v".to_owned(),
            "DisplayName".to_owned(),
        ],
        None,
        IDENTITY_TIMEOUT,
        4096,
    )
    .map_err(|error| tool_error(REG, &error))?;
    if outcome.status != 0 {
        return Ok(None);
    }
    Ok(outcome
        .stdout
        .lines()
        .find(|line| line.contains("REG_SZ"))
        .and_then(|line| line.split("REG_SZ").nth(1))
        .map(|value| value.trim().to_owned()))
}

/// A platform that keeps no application identity.
#[cfg(not(windows))]
fn platform_register(_identity: &str, _display_name: &str) -> Result<(), IdentityError> {
    Err(IdentityError::Unsupported {
        instead: "a notification is attributed by the desktop's own notification service",
    })
}

/// A platform that keeps no application identity.
#[cfg(not(windows))]
fn platform_unregister(_identity: &str) -> Result<(), IdentityError> {
    Err(IdentityError::Unsupported {
        instead: "a notification is attributed by the desktop's own notification service",
    })
}

/// A platform that keeps no application identity.
#[cfg(not(windows))]
fn platform_registered(_identity: &str) -> Result<Option<String>, IdentityError> {
    Err(IdentityError::Unsupported {
        instead: "a notification is attributed by the desktop's own notification service",
    })
}

/// The first line of a tool's error output, or of its ordinary output when it said nothing else.
#[cfg(windows)]
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
    fn an_identity_is_checked_before_it_is_written() {
        assert!(check("com.ncui.counter").is_ok());
        assert!(check("My App 2").is_ok());
        assert!(check("").is_err());
        assert!(check(&"a".repeat(MAX_IDENTITY_BYTES + 1)).is_err());
        // A character that is not part of an identity is refused rather than escaped: the value
        // reaches a registry key, and a key is not a place to be clever about quoting.
        for hostile in ["a'; Remove-Item C:\\;'b", "a\"b", "a\nb", "a$b"] {
            let error = check(hostile).expect_err("not an identity");
            assert_eq!(error.code(), "identity_invalid", "{hostile}");
        }
    }

    #[test]
    fn a_display_name_that_is_not_a_name_is_refused() {
        let error = register("com.ncui.counter", "a\"b").expect_err("not a name");
        assert_eq!(error.code(), "identity_invalid");
        let error = register("com.ncui.counter", "a\\b").expect_err("not a name");
        assert_eq!(error.code(), "identity_invalid");
    }

    #[cfg(windows)]
    #[test]
    fn an_identity_can_be_written_read_back_and_removed() {
        // The real path on this platform: the user's own hive, the platform's own tool, and an
        // identity this test owns so it cannot collide with anything the machine uses.
        let identity = "com.ncui.test-identity";
        unregister(identity).expect("nothing to remove is not a failure");
        assert_eq!(registered(identity).expect("readable"), None);

        register(identity, "NC-UI test").expect("the identity registers");
        assert_eq!(
            registered(identity).expect("readable").as_deref(),
            Some("NC-UI test"),
            "the name the platform shows must be the one that was registered"
        );

        unregister(identity).expect("the identity is removed");
        assert_eq!(registered(identity).expect("still readable"), None);
    }

    #[test]
    fn the_platform_says_whether_it_keeps_an_identity_at_all() {
        assert_eq!(is_supported(), cfg!(windows));
        if !is_supported() {
            assert!(matches!(
                register("com.ncui.counter", "Counter"),
                Err(IdentityError::Unsupported { .. })
            ));
        }
    }
}
