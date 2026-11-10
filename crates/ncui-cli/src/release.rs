//! Updating an application from a URL: fetching a release, checking it, and applying it.
//!
//! `ncui update <directory>` replaces an installed application with a bundle somebody already has.
//! This is the other half: an application that names where its releases live, so an update is one
//! command with a URL in it rather than a bundle a person had to find and download themselves.
//!
//! # What a release is
//!
//! One document and one file. The document says which application the release is for, which version
//! it installs, where the bundle is, and what the bundle's SHA-256 is; the file is the bundle in the
//! archive the installer already reads. Two checks follow from that, and both matter:
//!
//! * the **transport** digest is checked against the document, so a download that was truncated or
//!   altered in flight is refused before anything is unpacked;
//! * the **bundle's own** records are checked by the installer, file by file, so a bundle that is not
//!   what it says it is cannot replace a version that is intact.
//!
//! Neither check is redundant. The first trusts the document; the second does not have to.
//!
//! # What fetches
//!
//! The platform's own `curl`, with the environment scrubbed the way every other spawned command in
//! this workspace has it scrubbed: a fetch is a program this runtime runs, so it is treated like one.
//! A machine with no `curl` is told so rather than silently failing, and there is no second
//! implementation to keep in step — the alternative was a TLS stack of its own.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::installer::{self, InstallerError};

/// The largest release document this will read.
///
/// A document is a name, a version, a URL and a digest. Anything past this is not one, and a reader
/// that trusted a length it was given would be reading whatever a server sent.
pub const MAX_RELEASE_BYTES: u64 = 1024 * 1024;

/// The largest bundle this will download.
///
/// The same bound the installer puts on a file it writes: a bundle past this is not one.
pub const MAX_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;

/// How long one fetch may take, in seconds.
///
/// Long enough for a slow connection to deliver a bundle, short enough that a server which has
/// stopped sending does not hold the command forever.
pub const FETCH_TIMEOUT_SECONDS: u64 = 600;

/// The tool that fetches. The platform's own, so this runtime carries no TLS stack.
pub const FETCH_TOOL: &str = "curl";

/// What a release document says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// The application the release is for.
    pub name: String,
    /// The version it installs.
    pub version: String,
    /// Where the bundle is.
    pub url: String,
    /// The SHA-256 of the bundle's bytes, as lowercase hex.
    pub sha256: String,
}

/// Why an update from a URL did not happen.
#[derive(Debug)]
pub enum ReleaseError {
    /// The machine has no tool to fetch with.
    NoTool {
        /// What the tool is called.
        tool: &'static str,
        /// What the operating system said.
        reason: String,
    },
    /// The fetch itself failed.
    Fetch {
        /// What was being fetched.
        url: String,
        /// What the tool said.
        reason: String,
    },
    /// What was fetched is larger than a release document or a bundle may be.
    TooLarge {
        /// What was being fetched.
        url: String,
        /// How large it is.
        bytes: u64,
        /// The limit it passed.
        limit: u64,
    },
    /// The document is not a release document.
    Malformed {
        /// What was wrong with it.
        reason: String,
    },
    /// The release is for a different application than the one installed.
    NotForThisApplication {
        /// The application that is installed.
        wanted: String,
        /// The application the release names.
        document: String,
    },
    /// What was downloaded is not what the document said it would be.
    Mismatch {
        /// The digest the document named.
        expected: String,
        /// The digest of what arrived.
        actual: String,
    },
    /// The bundle could not be unpacked or written.
    Archive(InstallerError),
    /// The bundle is not one this runtime can install.
    Install(String),
    /// A file could not be read or written.
    Io {
        /// The path that failed.
        path: PathBuf,
        /// What the operating system said.
        reason: String,
    },
}

impl core::fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoTool { tool, reason } => write!(
                f,
                "this machine has no `{tool}` to fetch with, so an update from a URL cannot be \
                 fetched: {reason}"
            ),
            Self::Fetch { url, reason } => write!(f, "fetching {url} failed: {reason}"),
            Self::TooLarge { url, bytes, limit } => write!(
                f,
                "{url} is {bytes} bytes, over the {limit} an update may carry"
            ),
            Self::Malformed { reason } => write!(f, "that is not a release document: {reason}"),
            Self::NotForThisApplication { wanted, document } => write!(
                f,
                "this release is for `{document}` and `{wanted}` is what is installed"
            ),
            Self::Mismatch { expected, actual } => write!(
                f,
                "the download is not what the release said it would be: it should hash to \
                 {expected} and it hashes to {actual}"
            ),
            Self::Archive(error) => write!(f, "{error}"),
            Self::Install(reason) => write!(f, "the bundle could not be installed: {reason}"),
            Self::Io { path, reason } => write!(f, "{}: {reason}", path.display()),
        }
    }
}

impl std::error::Error for ReleaseError {}

/// Fetches `url` into `into`, bounded by `limit` bytes.
///
/// # Errors
///
/// Returns [`ReleaseError::NoTool`] when the machine has no `curl`, [`ReleaseError::Fetch`] when the
/// fetch fails or the server answers with an error, and [`ReleaseError::TooLarge`] when what arrived
/// is past the limit. A limit that a server ignores is caught here rather than by the reader.
pub fn fetch(url: &str, limit: u64, into: &Path) -> Result<(), ReleaseError> {
    let timeout = FETCH_TIMEOUT_SECONDS.to_string();
    let output = Command::new(FETCH_TOOL)
        // The environment a spawned command is given is scrubbed, the same as every other command
        // this runtime runs: a fetch is a program, and a program does not need the user's secrets.
        .env_clear()
        .envs(ncui_native::process::scrubbed_environment())
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            &timeout,
            // The limit is the tool's own, so a server that keeps sending is stopped rather than
            // filling the disk before anything checks how big it is.
            "--max-filesize",
            &limit.to_string(),
            "--output",
        ])
        .arg(into)
        .arg(url)
        .output()
        .map_err(|error| ReleaseError::NoTool {
            tool: FETCH_TOOL,
            reason: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(ReleaseError::Fetch {
            url: url.to_owned(),
            reason: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    // The tool's own limit is a courtesy to the server; this is the check that holds.
    let bytes = std::fs::metadata(into)
        .map_err(|error| ReleaseError::Io {
            path: into.to_path_buf(),
            reason: error.to_string(),
        })?
        .len();
    if bytes > limit {
        return Err(ReleaseError::TooLarge {
            url: url.to_owned(),
            bytes,
            limit,
        });
    }
    Ok(())
}

/// Reads a release document.
///
/// # Errors
///
/// Returns [`ReleaseError::Malformed`] when the bytes are not a release document, and
/// [`ReleaseError::Malformed`] when it names no application, no version, no URL or no digest.
pub fn release(bytes: &[u8]) -> Result<Release, ReleaseError> {
    let release: Release =
        serde_json::from_slice(bytes).map_err(|error| ReleaseError::Malformed {
            reason: error.to_string(),
        })?;
    if release.name.trim().is_empty() {
        return Err(ReleaseError::Malformed {
            reason: "it names no application".to_owned(),
        });
    }
    if release.version.trim().is_empty() {
        return Err(ReleaseError::Malformed {
            reason: "it names no version".to_owned(),
        });
    }
    if release.url.trim().is_empty() {
        return Err(ReleaseError::Malformed {
            reason: "it names no URL for the bundle".to_owned(),
        });
    }
    if release.sha256.len() != 64 || !release.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ReleaseError::Malformed {
            reason: "its digest is not a SHA-256".to_owned(),
        });
    }
    Ok(release)
}

/// The SHA-256 of a file, as lowercase hex.
///
/// # Errors
///
/// Returns [`ReleaseError::Io`] when the file cannot be read.
pub fn digest(path: &Path) -> Result<String, ReleaseError> {
    use sha2::{Digest as _, Sha256};
    let bytes = std::fs::read(path).map_err(|error| ReleaseError::Io {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = core::fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}"));
    }
    Ok(hex)
}

/// Applies a downloaded bundle to the application installed under `root`.
///
/// The bundle is unpacked into a directory of its own and removed afterwards, whatever happens: an
/// update leaves the bundle it applied nowhere but inside the application, and leaves nothing at all
/// if it did not apply.
///
/// # Errors
///
/// Returns [`ReleaseError::Mismatch`] when the archive is not what the document said, and the
/// installer's own refusals — a bundle that is not what its records say, one for a different
/// application, or one whose files cannot be written.
pub fn apply(
    root: &Path,
    wanted: &str,
    document: &Release,
    archive: &Path,
) -> Result<ncui_native::install::Updated, ReleaseError> {
    if document.name != wanted {
        return Err(ReleaseError::NotForThisApplication {
            wanted: wanted.to_owned(),
            document: document.name.clone(),
        });
    }
    let actual = digest(archive)?;
    if !actual.eq_ignore_ascii_case(&document.sha256) {
        return Err(ReleaseError::Mismatch {
            expected: document.sha256.clone(),
            actual,
        });
    }
    let bytes = std::fs::read(archive).map_err(|error| ReleaseError::Io {
        path: archive.to_path_buf(),
        reason: error.to_string(),
    })?;
    let unpacked = installer::unpack(&bytes).map_err(ReleaseError::Archive)?;
    let bundle = installer::private_directory().map_err(ReleaseError::Archive)?;
    installer::extract(&unpacked, bundle.path()).map_err(ReleaseError::Archive)?;
    let installer = ncui_native::Installer::new(root)
        .map_err(|error| ReleaseError::Install(error.to_string()))?;
    let updated = installer
        .update(bundle.path())
        .map_err(|error| ReleaseError::Install(error.to_string()))?;
    Ok(updated)
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
    fn a_release_document_is_read_and_a_malformed_one_is_refused() {
        let good = br#"{
            "name": "counter",
            "version": "2.0.0",
            "url": "https://example.test/counter.tar",
            "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
        }"#;
        let document = release(good).expect("a release document");
        assert_eq!(document.name, "counter");
        assert_eq!(document.version, "2.0.0");

        for raw in [
            // Not JSON.
            &b"not a document"[..],
            // No application.
            br#"{"name":"","version":"2.0.0","url":"u","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
            // No version.
            br#"{"name":"counter","version":"","url":"u","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
            // No URL.
            br#"{"name":"counter","version":"2.0.0","url":"","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
            // A digest that is not one.
            br#"{"name":"counter","version":"2.0.0","url":"u","sha256":"nope"}"#,
        ] {
            let error = release(raw).expect_err("a malformed document is refused");
            assert!(
                matches!(error, ReleaseError::Malformed { .. }),
                "{error}"
            );
        }
    }

    #[test]
    fn a_download_that_is_not_what_the_document_said_is_refused() {
        let scratch = std::env::temp_dir().join(format!("ncui-release-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("the scratch directory is creatable");
        let archive = scratch.join("counter.tar");
        std::fs::write(&archive, b"not the bundle that was promised").expect("writable");

        let document = Release {
            name: "counter".to_owned(),
            version: "2.0.0".to_owned(),
            url: "https://example.test/counter.tar".to_owned(),
            sha256: "0".repeat(64),
        };
        let error = apply(&scratch, "counter", &document, &archive)
            .expect_err("a download that does not match is refused");
        assert!(matches!(error, ReleaseError::Mismatch { .. }), "{error}");
        assert!(error.to_string().contains("hashes to"), "{error}");

        // A release for another application is refused before anything is hashed.
        let error = apply(&scratch, "something-else", &document, &archive)
            .expect_err("a release for another application is refused");
        assert!(
            matches!(error, ReleaseError::NotForThisApplication { .. }),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
