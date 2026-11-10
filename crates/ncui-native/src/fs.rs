//! Filesystem access for a page, contained to one root.
//!
//! A page that may read files has, in every framework that offers it, the application's
//! own authority over the user's disk. That is why the capability exists and why it is
//! dangerous: `filesystem.read` with a naive join is a read of the user's SSH key, and
//! `filesystem.write` with a naive join is a write to a startup directory.
//!
//! So the containment is the same containment the rest of the project uses, applied twice:
//!
//! 1. [`SafePath::resolve`] resolves the request lexically against the root and refuses
//!    traversal, absolute paths, drive-qualified paths and NUL bytes.
//! 2. The resolved path is canonicalized and checked again, which is what refuses a
//!    symlink planted inside the root and pointing outside it.
//!
//! A write is checked differently from a read, because the file does not exist yet. The
//! **parent** directory is canonicalized and checked, and an existing target is
//! canonicalized and checked as well, so a write cannot follow a symlink out of the root.
//!
//! # Bounds
//!
//! Every operation is bounded. A page that can read a file of unbounded size can exhaust
//! the process's memory, and a page that can ask for a directory listing of unbounded
//! length can do the same with names. The limits are constants rather than configuration:
//! an application that needs a larger limit needs a different capability, not a bigger
//! number.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use ncui_security::path::SafePath;
use serde::{Deserialize, Serialize};

/// The largest file a page may read, in bytes.
pub const MAX_READ_BYTES: usize = 8 * 1024 * 1024;

/// The largest file a page may write, in bytes.
pub const MAX_WRITE_BYTES: usize = 8 * 1024 * 1024;

/// The most entries a directory listing will return.
pub const MAX_DIRECTORY_ENTRIES: usize = 4096;

/// A filesystem rooted at one directory.
///
/// The root is canonicalized once, at construction, so every later containment check
/// compares against the same absolute path and a root that does not exist is reported
/// before the first request rather than on it.
#[derive(Debug, Clone)]
pub struct FileSystem {
    root: PathBuf,
    max_read: usize,
    max_write: usize,
}

impl FileSystem {
    /// A filesystem rooted at `root`.
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
            max_read: MAX_READ_BYTES,
            max_write: MAX_WRITE_BYTES,
        })
    }

    /// The directory every request is contained by.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reads a text file.
    ///
    /// # Errors
    ///
    /// Returns [`FsError::OutsideRoot`] for a path that escapes the root,
    /// [`FsError::NotFound`] for a file that is not there, [`FsError::TooLarge`] for a file
    /// over the read limit, and [`FsError::NotText`] for a file that is not valid UTF-8.
    pub fn read_text(&self, request: &str) -> Result<FileContent, FsError> {
        let path = self.existing(request)?;
        let metadata = std::fs::metadata(&path).map_err(|error| Self::io_error(request, &error))?;
        if !metadata.is_file() {
            return Err(FsError::NotAFile {
                path: request.to_owned(),
            });
        }
        if metadata.len() > self.max_read as u64 {
            return Err(FsError::TooLarge {
                path: request.to_owned(),
                limit: self.max_read,
            });
        }
        let mut file =
            std::fs::File::open(&path).map_err(|error| Self::io_error(request, &error))?;
        // The metadata length is a claim; the read is bounded independently of it, so a
        // file that grows between the check and the read cannot exceed the limit.
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        let read = file
            .by_ref()
            .take(self.max_read as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| Self::io_error(request, &error))?;
        if read > self.max_read {
            return Err(FsError::TooLarge {
                path: request.to_owned(),
                limit: self.max_read,
            });
        }
        let text = String::from_utf8(bytes).map_err(|_| FsError::NotText {
            path: request.to_owned(),
        })?;
        Ok(FileContent {
            path: request.to_owned(),
            bytes: text.len(),
            text,
        })
    }

    /// Writes a text file, replacing it when it exists.
    ///
    /// # Errors
    ///
    /// Returns [`FsError::OutsideRoot`] for a path that escapes the root,
    /// [`FsError::TooLarge`] for contents over the write limit, and
    /// [`FsError::MissingParent`] when the containing directory does not exist — a write
    /// does not create directories, because a page that can create them can also create
    /// the ones a later write would use.
    pub fn write_text(&self, request: &str, contents: &str) -> Result<FileWritten, FsError> {
        if contents.len() > self.max_write {
            return Err(FsError::TooLarge {
                path: request.to_owned(),
                limit: self.max_write,
            });
        }
        let path = self.writable(request)?;
        std::fs::write(&path, contents).map_err(|error| Self::io_error(request, &error))?;
        Ok(FileWritten {
            path: request.to_owned(),
            bytes: contents.len(),
        })
    }

    /// Lists a directory.
    ///
    /// Entries are sorted by name, so two identical calls answer identically. A listing is
    /// bounded by [`MAX_DIRECTORY_ENTRIES`], and a directory with more entries is reported
    /// as [`FsError::TooManyEntries`] rather than silently truncated: a truncated listing
    /// that looks complete is a bug the caller cannot see.
    ///
    /// # Errors
    ///
    /// Returns [`FsError::NotADirectory`] for a file, and the same containment errors as
    /// [`FileSystem::read_text`].
    pub fn list(&self, request: &str) -> Result<DirectoryListing, FsError> {
        let path = self.existing(request)?;
        let metadata = std::fs::metadata(&path).map_err(|error| Self::io_error(request, &error))?;
        if !metadata.is_dir() {
            return Err(FsError::NotADirectory {
                path: request.to_owned(),
            });
        }
        let mut entries = Vec::new();
        let read = std::fs::read_dir(&path).map_err(|error| Self::io_error(request, &error))?;
        for entry in read {
            let entry = entry.map_err(|error| Self::io_error(request, &error))?;
            if entries.len() >= MAX_DIRECTORY_ENTRIES {
                return Err(FsError::TooManyEntries {
                    path: request.to_owned(),
                    limit: MAX_DIRECTORY_ENTRIES,
                });
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = entry
                .metadata()
                .map_err(|error| Self::io_error(request, &error))?;
            entries.push(Entry {
                name,
                directory: metadata.is_dir(),
                bytes: metadata.len(),
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(DirectoryListing {
            path: request.to_owned(),
            entries,
        })
    }

    /// Describes a file or directory.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`FileSystem::read_text`] when the path does not exist
    /// or escapes the root.
    pub fn stat(&self, request: &str) -> Result<Stat, FsError> {
        let path = self.existing(request)?;
        let metadata = std::fs::metadata(&path).map_err(|error| Self::io_error(request, &error))?;
        Ok(Stat {
            path: request.to_owned(),
            directory: metadata.is_dir(),
            bytes: metadata.len(),
            readonly: metadata.permissions().readonly(),
        })
    }

    /// Resolves a request that must already name something inside the root.
    fn existing(&self, request: &str) -> Result<PathBuf, FsError> {
        // The root itself is a legitimate subject for `stat` and `list`. `SafePath`
        // refuses a request that resolves to its own root, because for a *file* operation
        // that is always a caller bug, so the one legitimate spelling is handled here.
        if request.is_empty() || request == "." {
            return Ok(self.root.clone());
        }
        let resolved =
            SafePath::resolve(&self.root, request).map_err(|error| FsError::OutsideRoot {
                path: request.to_owned(),
                reason: error.to_string(),
            })?;
        let canonical = match resolved.canonicalize() {
            Ok(canonical) => canonical,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(FsError::NotFound {
                    path: request.to_owned(),
                });
            }
            Err(error) => return Err(Self::io_error(request, &error)),
        };
        if !SafePath::is_contained(&self.root, &canonical) {
            return Err(FsError::OutsideRoot {
                path: request.to_owned(),
                reason: "the path resolves outside the root through a link".to_owned(),
            });
        }
        Ok(canonical)
    }

    /// Resolves a request that names a file to write.
    ///
    /// The target need not exist, so the containment check is made on the parent — which
    /// must exist — and on the target itself when it does exist.
    fn writable(&self, request: &str) -> Result<PathBuf, FsError> {
        if request.is_empty() || request == "." {
            return Err(FsError::NotAFile {
                path: request.to_owned(),
            });
        }
        let resolved =
            SafePath::resolve(&self.root, request).map_err(|error| FsError::OutsideRoot {
                path: request.to_owned(),
                reason: error.to_string(),
            })?;
        let Some(parent) = resolved.parent() else {
            return Err(FsError::OutsideRoot {
                path: request.to_owned(),
                reason: "the path has no parent directory".to_owned(),
            });
        };
        let canonical_parent = parent.canonicalize().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FsError::MissingParent {
                    path: request.to_owned(),
                }
            } else {
                Self::io_error(request, &error)
            }
        })?;
        if !SafePath::is_contained(&self.root, &canonical_parent) {
            return Err(FsError::OutsideRoot {
                path: request.to_owned(),
                reason: "the containing directory resolves outside the root".to_owned(),
            });
        }
        // An existing target is canonicalized too: writing through a symlink that points
        // outside the root is a write outside the root, however contained the link is.
        if resolved.exists() {
            let canonical = resolved
                .canonicalize()
                .map_err(|error| Self::io_error(request, &error))?;
            if !SafePath::is_contained(&self.root, &canonical) {
                return Err(FsError::OutsideRoot {
                    path: request.to_owned(),
                    reason: "the file resolves outside the root through a link".to_owned(),
                });
            }
            if canonical.is_dir() {
                return Err(FsError::NotAFile {
                    path: request.to_owned(),
                });
            }
        }
        Ok(resolved)
    }

    /// Builds the error for a failed filesystem call.
    fn io_error(request: &str, error: &std::io::Error) -> FsError {
        match error.kind() {
            std::io::ErrorKind::NotFound => FsError::NotFound {
                path: request.to_owned(),
            },
            std::io::ErrorKind::PermissionDenied => FsError::Denied {
                path: request.to_owned(),
                reason: error.to_string(),
            },
            _ => FsError::Io {
                path: request.to_owned(),
                reason: error.to_string(),
            },
        }
    }
}

/// The contents of a file a page read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileContent {
    /// The path the caller asked for, as it asked for it.
    pub path: String,
    /// The file's length in bytes.
    pub bytes: usize,
    /// The file's text.
    pub text: String,
}

/// What a write did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileWritten {
    /// The path the caller asked for, as it asked for it.
    pub path: String,
    /// How many bytes were written.
    pub bytes: usize,
}

/// One entry in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The entry's name, not its path.
    pub name: String,
    /// Whether the entry is a directory.
    pub directory: bool,
    /// The entry's size in bytes, or zero for a directory.
    pub bytes: u64,
}

/// A directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryListing {
    /// The path the caller asked for, as it asked for it.
    pub path: String,
    /// The entries, sorted by name.
    pub entries: Vec<Entry>,
}

/// A file or directory's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stat {
    /// The path the caller asked for, as it asked for it.
    pub path: String,
    /// Whether the path names a directory.
    pub directory: bool,
    /// The size in bytes.
    pub bytes: u64,
    /// Whether the entry is readonly.
    pub readonly: bool,
}

/// A filesystem operation that could not be performed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    /// The requested path is not inside the root.
    OutsideRoot {
        /// The path the caller asked for.
        path: String,
        /// Why it was refused.
        reason: String,
    },
    /// The requested path does not exist.
    NotFound {
        /// The path the caller asked for.
        path: String,
    },
    /// The requested path names a directory where a file was expected, or the reverse.
    NotAFile {
        /// The path the caller asked for.
        path: String,
    },
    /// The requested path names a file where a directory was expected.
    NotADirectory {
        /// The path the caller asked for.
        path: String,
    },
    /// The write's containing directory does not exist.
    MissingParent {
        /// The path the caller asked for.
        path: String,
    },
    /// The file is larger than the operation's limit.
    TooLarge {
        /// The path the caller asked for.
        path: String,
        /// The limit that was exceeded, in bytes.
        limit: usize,
    },
    /// The directory holds more entries than a listing will return.
    TooManyEntries {
        /// The path the caller asked for.
        path: String,
        /// The limit that was exceeded.
        limit: usize,
    },
    /// The file is not valid UTF-8.
    NotText {
        /// The path the caller asked for.
        path: String,
    },
    /// The operating system refused the operation.
    Denied {
        /// The path the caller asked for.
        path: String,
        /// What the operating system said.
        reason: String,
    },
    /// The operating system failed the operation.
    Io {
        /// The path the caller asked for.
        path: String,
        /// What the operating system said.
        reason: String,
    },
}

impl FsError {
    /// The error code this failure is reported under at the page seam.
    ///
    /// The codes are the ones the bridge already uses, so a page handles a refused
    /// filesystem call the same way it handles any other refusal.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::OutsideRoot { .. } | Self::Denied { .. } => "capability_denied",
            Self::NotFound { .. }
            | Self::NotAFile { .. }
            | Self::NotADirectory { .. }
            | Self::MissingParent { .. } => "not_found",
            Self::TooLarge { .. } | Self::TooManyEntries { .. } => "limit_exceeded",
            Self::NotText { .. } => "invalid_params",
            Self::Io { .. } => "internal",
        }
    }

    /// Whether retrying the same call could succeed.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::Io { .. })
    }
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideRoot { path, reason } => {
                write!(f, "`{path}` is outside the filesystem root: {reason}")
            }
            Self::NotFound { path } => write!(f, "`{path}` does not exist"),
            Self::NotAFile { path } => write!(f, "`{path}` is not a file"),
            Self::NotADirectory { path } => write!(f, "`{path}` is not a directory"),
            Self::MissingParent { path } => {
                write!(f, "the directory containing `{path}` does not exist")
            }
            Self::TooLarge { path, limit } => {
                write!(f, "`{path}` is larger than the {limit}-byte limit")
            }
            Self::TooManyEntries { path, limit } => {
                write!(f, "`{path}` holds more than {limit} entries")
            }
            Self::NotText { path } => write!(f, "`{path}` is not valid UTF-8 text"),
            Self::Denied { path, reason } => {
                write!(f, "the operating system refused `{path}`: {reason}")
            }
            Self::Io { path, reason } => write!(f, "`{path}` failed: {reason}"),
        }
    }
}

impl std::error::Error for FsError {}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    /// A directory that is removed when the test ends.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn with_files(files: &[(&str, &str)]) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "ncui-native-fs-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("the scratch directory is created");
            for (name, contents) in files {
                let target = path.join(name);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).expect("the parent is created");
                }
                std::fs::write(target, contents).expect("the fixture is written");
            }
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn a_file_inside_the_root_is_read() {
        let scratch = Scratch::with_files(&[("notes/today.txt", "hello")]);
        let fs = FileSystem::new(scratch.path()).expect("the filesystem starts");
        let content = fs.read_text("notes/today.txt").expect("the file is read");
        assert_eq!(content.text, "hello");
        assert_eq!(content.bytes, 5);
        assert_eq!(content.path, "notes/today.txt");
    }

    #[test]
    fn a_file_outside_the_root_is_refused() {
        let scratch = Scratch::with_files(&[("inside.txt", "in")]);
        let fs = FileSystem::new(scratch.path()).expect("the filesystem starts");
        for request in ["../outside.txt", "..\\outside.txt", "/etc/passwd"] {
            let error = fs.read_text(request).expect_err("an escape is refused");
            assert_eq!(error.code(), "capability_denied", "{request}: {error}");
        }
    }

    #[test]
    fn a_missing_file_is_reported_as_missing() {
        let scratch = Scratch::with_files(&[]);
        let fs = FileSystem::new(scratch.path()).expect("the filesystem starts");
        let error = fs.read_text("absent.txt").expect_err("a missing file");
        assert_eq!(error.code(), "not_found");
    }

    #[test]
    fn a_file_is_written_and_read_back() {
        let scratch = Scratch::with_files(&[]);
        let fs = FileSystem::new(scratch.path()).expect("the filesystem starts");
        let written = fs
            .write_text("out.txt", "written")
            .expect("the file is written");
        assert_eq!(written.bytes, 7);
        assert_eq!(fs.read_text("out.txt").expect("read back").text, "written");
    }

    #[test]
    fn a_write_outside_the_root_is_refused() {
        let scratch = Scratch::with_files(&[]);
        let fs = FileSystem::new(scratch.path()).expect("the filesystem starts");
        let error = fs
            .write_text("../escape.txt", "x")
            .expect_err("a write escape");
        assert_eq!(error.code(), "capability_denied");
        assert!(
            !scratch
                .path()
                .parent()
                .expect("a parent")
                .join("escape.txt")
                .exists(),
            "the refused write must not have produced a file"
        );
    }

    #[test]
    fn a_write_without_its_parent_directory_is_refused() {
        let scratch = Scratch::with_files(&[]);
        let fs = FileSystem::new(scratch.path()).expect("the filesystem starts");
        let error = fs
            .write_text("missing/out.txt", "x")
            .expect_err("no parent");
        assert_eq!(error.code(), "not_found");
    }

    #[test]
    fn a_directory_is_listed_in_name_order() {
        let scratch = Scratch::with_files(&[("b.txt", "b"), ("a.txt", "a"), ("sub/c.txt", "c")]);
        let fs = FileSystem::new(scratch.path()).expect("the filesystem starts");
        let listing = fs.list(".").expect("the directory is listed");
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.txt", "b.txt", "sub"]);
        assert!(listing.entries[2].directory);
        assert!(!listing.entries[0].directory);
    }

    #[test]
    fn a_file_cannot_be_listed_as_a_directory() {
        let scratch = Scratch::with_files(&[("a.txt", "a")]);
        let fs = FileSystem::new(scratch.path()).expect("the filesystem starts");
        let error = fs.list("a.txt").expect_err("not a directory");
        assert_eq!(error.code(), "not_found");
    }

    #[test]
    fn stat_reports_what_the_entry_is() {
        let scratch = Scratch::with_files(&[("a.txt", "abc")]);
        let fs = FileSystem::new(scratch.path()).expect("the filesystem starts");
        let file = fs.stat("a.txt").expect("the file is described");
        assert!(!file.directory);
        assert_eq!(file.bytes, 3);
        let directory = fs.stat(".").expect("the root is described");
        assert!(directory.directory);
    }

    #[test]
    fn a_file_over_the_read_limit_is_refused() {
        let scratch = Scratch::with_files(&[]);
        let mut fs = FileSystem::new(scratch.path()).expect("the filesystem starts");
        fs.max_read = 8;
        std::fs::write(scratch.path().join("big.txt"), "more than eight bytes")
            .expect("the fixture is written");
        let error = fs.read_text("big.txt").expect_err("too large");
        assert_eq!(error.code(), "limit_exceeded");
    }

    #[test]
    fn a_write_over_the_limit_is_refused_before_it_reaches_the_disk() {
        let scratch = Scratch::with_files(&[]);
        let mut fs = FileSystem::new(scratch.path()).expect("the filesystem starts");
        fs.max_write = 4;
        let error = fs
            .write_text("big.txt", "more than four bytes")
            .expect_err("too large");
        assert_eq!(error.code(), "limit_exceeded");
        assert!(!scratch.path().join("big.txt").exists());
    }

    #[test]
    fn a_file_that_is_not_text_is_refused_rather_than_lossily_decoded() {
        let scratch = Scratch::with_files(&[]);
        let fs = FileSystem::new(scratch.path()).expect("the filesystem starts");
        std::fs::write(scratch.path().join("raw.bin"), [0xFF, 0xFE, 0x00])
            .expect("the fixture is written");
        let error = fs.read_text("raw.bin").expect_err("not text");
        assert_eq!(error.code(), "invalid_params");
    }
}
