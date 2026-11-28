//! The user-data directory: where the application's state lives, separated by
//! what the state is.
//!
//! A desktop application accumulates three kinds of state, and they are not
//! the same thing. Settings are small and precious — the user expects them to
//! follow the account. Data is the application's own — databases, indexes,
//! downloaded models — large, machine-local, painful but possible to rebuild.
//! Cache is regenerable by definition: losing it costs time, never truth.
//! Treating them as one directory is how an application ends up roaming
//! gigabytes or backing up what the network can re-fetch.
//!
//! On Windows the separation is the platform's own: `APPDATA` roams with a
//! domain account, `LOCALAPPDATA` does not. This module does not invent a
//! convention; it applies the one the operating system already maintains and
//! scopes it to the application's identity.
//!
//! # What this module is not
//!
//! It does not decide what the application stores. It answers where, creates
//! the directories on demand, and refuses honestly when the environment
//! cannot answer — a service session without a profile is a real shape, and
//! falling back silently to a temp directory is how state becomes invisible.

use std::io;
use std::path::PathBuf;

/// The application's directories, separated by what the state is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    /// Small, precious, roams with the account: settings and preferences.
    config: PathBuf,
    /// Machine-local application state: databases, indexes, anything large.
    data: PathBuf,
    /// Regenerable by definition: losing it costs time, never truth.
    cache: PathBuf,
}

impl AppDirs {
    /// The roaming configuration directory.
    #[must_use]
    pub fn config(&self) -> &std::path::Path {
        &self.config
    }

    /// The machine-local data directory — where the instance lock, the logs,
    /// and the databases belong.
    #[must_use]
    pub fn data(&self) -> &std::path::Path {
        &self.data
    }

    /// The regenerable cache directory.
    #[must_use]
    pub fn cache(&self) -> &std::path::Path {
        &self.cache
    }

    /// Creates all three directories. Idempotent; failure is honest —
    /// an application that cannot create its state directory cannot know
    /// where its state is.
    ///
    /// # Errors
    ///
    /// Any directory that cannot be created, with the path in the reason.
    pub fn ensure(&self) -> io::Result<()> {
        for dir in [&self.config, &self.data, &self.cache] {
            std::fs::create_dir_all(dir).map_err(|error| {
                io::Error::new(error.kind(), format!("{}: {error}", dir.display()))
            })?;
        }
        Ok(())
    }
}

/// Resolves the application's directories for `identity` from the platform's
/// own conventions, without creating anything.
///
/// The identity must be a single path component — it names the application's
/// folder inside each root, and a separator would let one application's state
/// land inside another's.
///
/// # Errors
///
/// The environment not answering — no profile, no known folders — is an
/// honest failure, because the alternative is guessing where state lives.
#[cfg(windows)]
pub fn app_dirs(identity: &str) -> io::Result<AppDirs> {
    if identity.is_empty() || identity.contains(['/', '\\']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the application identity must be a single path component",
        ));
    }
    let roaming = std::env::var_os("APPDATA")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "APPDATA is not set — no roaming profile",
            )
        })?;
    let local = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "LOCALAPPDATA is not set — no local profile",
            )
        })?;
    let data = PathBuf::from(local).join(identity);
    Ok(AppDirs {
        config: PathBuf::from(roaming).join(identity),
        cache: data.join("Cache"),
        data,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
mod tests {
    use super::*;

    #[test]
    fn the_directories_are_separated_by_what_the_state_is() {
        let dirs = app_dirs("nc-test").expect("the environment answers on a real machine");
        assert_ne!(dirs.config(), dirs.data(), "config roams; data does not");
        assert!(
            dirs.cache().starts_with(dirs.data()),
            "cache is machine-local, inside data"
        );
        assert!(
            dirs.config().ends_with("nc-test"),
            "the identity scopes the config root"
        );
        assert!(
            dirs.data().ends_with("nc-test"),
            "the identity scopes the data root"
        );
        assert!(
            dirs.cache()
                .ends_with(std::path::Path::new("nc-test").join("Cache")),
            "cache lives inside the scoped data root"
        );
    }

    #[test]
    fn ensure_creates_and_is_idempotent() {
        let dirs = app_dirs("nc-test-ensure").expect("the environment answers");
        dirs.ensure().expect("the first creation succeeds");
        dirs.ensure().expect("the second creation is a no-op");
        assert!(dirs.config().is_dir());
        assert!(dirs.data().is_dir());
        assert!(dirs.cache().is_dir());
        let _ = std::fs::remove_dir_all(dirs.config());
        let _ = std::fs::remove_dir_all(dirs.data());
    }

    #[test]
    fn a_separator_in_the_identity_is_refused() {
        assert!(
            app_dirs("one/other").is_err(),
            "a separator would nest one application's state inside another's"
        );
        assert!(app_dirs("").is_err(), "an empty identity names nothing");
    }
}
