//! Importing a browser's data: the migration half of becoming someone's default.
//!
//! An application that replaces a browser is adopted faster when its bookmarks come with it.
//! This module reads Chrome's own bookmark file — the format is a JSON document the browser
//! writes in the profile directory — and flattens it into a list a page or a CLI caller can
//! hand to the user. It reads; it does not write anywhere, and it does not touch anything
//! but the file it was pointed at.
//!
//! # What is imported, and what is not
//!
//! Bookmarks are plain JSON, so they are read directly. Passwords are encrypted to the
//! user's session and cookies are a SQLite database under lock, so neither is read here —
//! a migration that needed them would say so rather than reach for a decryption key.
//!
//! # Where the profile is
//!
//! `NCUI_CHROME_PROFILE` names a profile directory when set (which is also how the tests
//! point this at a fixture); otherwise the platform's own Chrome location is used. The
//! import is read-only and bounded: a bookmark file past its bound, or one with more than
//! [`MAX_BOOKMARKS`] entries, is refused rather than trusted.

use std::path::{Path, PathBuf};

/// The most bookmarks one import will return.
pub const MAX_BOOKMARKS: usize = 10_000;

/// The longest a bookmark's title or URL may be, so one pathological entry cannot fill
/// the result with a megabyte of one field.
pub const MAX_FIELD_BYTES: usize = 4 * 1024;

/// The environment variable that names the Chrome profile to import from.
pub const CHROME_PROFILE_ENV: &str = "NCUI_CHROME_PROFILE";

/// Why an import did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// The profile directory, or the bookmark file in it, is not there.
    NotFound {
        /// The path that was looked for.
        path: PathBuf,
    },
    /// The bookmark file is not the document Chrome writes.
    Malformed {
        /// Why.
        reason: String,
    },
    /// The bookmark file, or its contents, are past this module's bounds.
    TooLarge {
        /// The bound that was passed.
        limit: usize,
    },
    /// The file could not be read.
    Io {
        /// The path that failed.
        path: PathBuf,
        /// What the system said.
        reason: String,
    },
}

impl ImportError {
    /// The stable code a caller sees on the wire.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "import_not_found",
            Self::Malformed { .. } => "import_malformed",
            Self::TooLarge { .. } => "import_too_large",
            Self::Io { .. } => "import_io",
        }
    }
}

impl core::fmt::Display for ImportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotFound { path } => {
                write!(f, "no Chrome bookmarks were found at {}", path.display())
            }
            Self::Malformed { reason } => {
                write!(f, "the bookmark file is not one Chrome wrote: {reason}")
            }
            Self::TooLarge { limit } => {
                write!(f, "the bookmark file holds more than {limit} entries")
            }
            Self::Io { path, reason } => write!(f, "{}: {reason}", path.display()),
        }
    }
}

impl std::error::Error for ImportError {}

/// One bookmark, flattened out of Chrome's folder tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ImportedBookmark {
    /// The bookmark's title.
    pub title: String,
    /// The URL it points at.
    pub url: String,
    /// The folders it sits in, root to leaf, joined by `/` — e.g. `Bookmarks bar/Work`.
    pub folder: String,
}

/// The Chrome profile directory this machine has, or `None` when there is none.
#[must_use]
pub fn chrome_profile_dir() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os(CHROME_PROFILE_ENV) {
        let path = PathBuf::from(named);
        return path.is_dir().then_some(path);
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|local| {
            PathBuf::from(local)
                .join("Google")
                .join("Chrome")
                .join("User Data")
                .join("Default")
        })
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home).join("Library/Application Support/Google/Chrome/Default")
        })
    } else {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".config/google-chrome/Default"))
    }?;
    base.is_dir().then_some(base)
}

/// Reads the bookmarks out of a Chrome profile directory.
///
/// # Errors
///
/// [`ImportError::NotFound`] when the profile or its bookmark file is missing,
/// [`ImportError::Malformed`] when the file is not the document Chrome writes, and
/// [`ImportError::TooLarge`] when it holds more than [`MAX_BOOKMARKS`] entries.
pub fn import_chrome_bookmarks(profile: &Path) -> Result<Vec<ImportedBookmark>, ImportError> {
    let file = profile.join("Bookmarks");
    if !file.is_file() {
        return Err(ImportError::NotFound { path: file });
    }
    let bytes = std::fs::read(&file).map_err(|error| ImportError::Io {
        path: file.clone(),
        reason: error.to_string(),
    })?;
    let document: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| ImportError::Malformed {
            reason: error.to_string(),
        })?;
    let roots = document
        .get("roots")
        .ok_or_else(|| ImportError::Malformed {
            reason: "it has no `roots`".to_owned(),
        })?;
    let mut bookmarks = Vec::new();
    let empty_roots = serde_json::Map::new();
    for (root_name, node) in roots.as_object().unwrap_or(&empty_roots) {
        walk(node, &root_name.replace('_', " "), &mut bookmarks)?;
    }
    if bookmarks.len() > MAX_BOOKMARKS {
        return Err(ImportError::TooLarge {
            limit: MAX_BOOKMARKS,
        });
    }
    Ok(bookmarks)
}

/// Walks one node of Chrome's bookmark tree, collecting URL leaves.
fn walk(
    node: &serde_json::Value,
    folder: &str,
    into: &mut Vec<ImportedBookmark>,
) -> Result<(), ImportError> {
    let kind = node.get("type").and_then(serde_json::Value::as_str);
    match kind {
        Some("url") => {
            let title = bounded_text(node.get("name"));
            let url = bounded_text(node.get("url"));
            if url.is_empty() {
                // A bookmark with no URL is not one; Chrome writes such placeholders only
                // mid-edit, and importing a pointer to nowhere would be worse than skipping it.
                return Ok(());
            }
            into.push(ImportedBookmark {
                title,
                url,
                folder: folder.to_owned(),
            });
            if into.len() > MAX_BOOKMARKS {
                return Err(ImportError::TooLarge {
                    limit: MAX_BOOKMARKS,
                });
            }
            Ok(())
        }
        Some("folder") => {
            let Some(children) = node.get("children").and_then(serde_json::Value::as_array) else {
                return Ok(());
            };
            for child in children {
                // A folder child extends the path its own bookmarks and nested folders
                // are reported under; a URL child is reported under this folder's path.
                let child_folder = match child.get("type").and_then(serde_json::Value::as_str) {
                    Some("folder") => format!("{folder}/{}", bounded_text(child.get("name"))),
                    _ => folder.to_owned(),
                };
                walk(child, &child_folder, into)?;
            }
            Ok(())
        }
        // A node of an unknown kind is skipped rather than failing the import: Chrome adds
        // kinds across versions, and one unknown node should not hide the rest.
        _ => Ok(()),
    }
}

/// A node's text field, bounded, with control characters stripped.
fn bounded_text(field: Option<&serde_json::Value>) -> String {
    let text = field
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let cleaned: String = text
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_FIELD_BYTES)
        .collect();
    cleaned.trim().to_owned()
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::{ImportError, import_chrome_bookmarks};
    use std::path::PathBuf;

    /// A scratch directory this test owns.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("ncui-import-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("the scratch is creatable");
            Self { path }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    const BOOKMARKS: &str = r#"{
        "roots": {
            "bookmark_bar": {
                "type": "folder", "name": "Bookmarks bar",
                "children": [
                    {"type": "url", "name": "NC-UI", "url": "https://nc-ui.test/"},
                    {"type": "folder", "name": "Work", "children": [
                        {"type": "url", "name": "Issue\ttracker", "url": "https://issues.test/"}
                    ]},
                    {"type": "url", "name": "no url here"}
                ]
            },
            "other": {"type": "folder", "name": "Other", "children": [
                {"type": "url", "name": "Read later", "url": "https://later.test/"}
            ]},
            "synced": {"type": "folder", "name": "Synced", "children": []}
        }
    }"#;

    #[test]
    fn a_chrome_bookmark_file_flattens_into_bookmarks_with_their_folders() {
        let scratch = Scratch::new("bookmarks");
        std::fs::write(scratch.path.join("Bookmarks"), BOOKMARKS).expect("writable");

        let imported = import_chrome_bookmarks(&scratch.path).expect("the import runs");
        assert_eq!(imported.len(), 3, "{imported:?}");
        assert_eq!(imported[0].title, "NC-UI");
        assert_eq!(imported[0].url, "https://nc-ui.test/");
        assert_eq!(imported[0].folder, "bookmark bar");
        assert_eq!(imported[1].folder, "bookmark bar/Work");
        // A tab in the title is control-stripped, not carried into JSON.
        assert_eq!(imported[1].title, "Issuetracker");
        assert_eq!(imported[2].title, "Read later");
        assert_eq!(imported[2].folder, "other");
    }

    #[test]
    fn a_missing_profile_or_file_is_a_not_found() {
        let scratch = Scratch::new("missing");
        let error = import_chrome_bookmarks(&scratch.path).expect_err("no Bookmarks file is there");
        assert!(matches!(error, ImportError::NotFound { .. }), "{error}");
    }

    #[test]
    fn a_file_that_is_not_chromes_is_malformed() {
        let scratch = Scratch::new("malformed");
        std::fs::write(scratch.path.join("Bookmarks"), "not chrome").expect("writable");
        let error = import_chrome_bookmarks(&scratch.path).expect_err("not a bookmark document");
        assert!(matches!(error, ImportError::Malformed { .. }), "{error}");

        std::fs::write(scratch.path.join("Bookmarks"), "{\"roots\": {}}").expect("writable");
        let empty_roots = import_chrome_bookmarks(&scratch.path).expect("empty roots import");
        assert!(empty_roots.is_empty(), "no roots, no bookmarks");
    }

    #[test]
    fn an_import_past_its_bound_is_refused() {
        let scratch = Scratch::new("bound");
        // 200 folders of 100 bookmarks each: far past the 10,000 bound.
        let mut folders = Vec::new();
        for folder in 0..200 {
            let bookmarks: Vec<String> = (0..100)
                .map(|bookmark| {
                    format!(
                        "{{\"type\":\"url\",\"name\":\"b{folder}-{bookmark}\",\
                         \"url\":\"https://x.test/{folder}/{bookmark}\"}}"
                    )
                })
                .collect();
            folders.push(format!(
                "{{\"type\":\"folder\",\"name\":\"f{folder}\",\"children\":[{}]}}",
                bookmarks.join(",")
            ));
        }
        let children = folders.join(",");
        let document = format!(
            "{{\"roots\":{{\"bookmark_bar\":{{\"type\":\"folder\",\"name\":\"bar\",\
             \"children\":[{children}]}}}}}}"
        );
        std::fs::write(scratch.path.join("Bookmarks"), document).expect("writable");
        let error = import_chrome_bookmarks(&scratch.path).expect_err("past the bound");
        assert!(matches!(error, ImportError::TooLarge { .. }), "{error}");
    }
}
