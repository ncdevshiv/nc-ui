//! A self-contained installer: one file a user can run.
//!
//! A bundle is a directory — the runtime, the application, a digest manifest and the registration
//! scripts. A directory is not something a person downloads and double-clicks. This module closes
//! that gap without a dependency and without an archive library: the installer is the `ncui` binary
//! itself, with the bundle appended to it and a footer that says where the bundle starts.
//!
//! # Why append rather than wrap
//!
//! The alternative — a second binary that unpacks a bundle — would need the runtime beside it, or a
//! copy of it, which is the same bytes plus a second program to keep in step. Appending to the
//! runtime means the installer *is* the runtime: the code that installs an application and the code
//! that runs one cannot disagree about what a bundle is, because they are one program.
//!
//! # What makes it safe to append
//!
//! An executable is a file the operating system loads and then ignores the rest of, so bytes after
//! the last section are carried along untouched. The footer is at the very end — where a reader
//! looks first — and it is checked before anything is allocated or written: a file whose last eight
//! bytes are not the magic is simply a runtime, and a file whose footer names a range outside the
//! file is refused rather than read.
//!
//! # What the archive is
//!
//! A length-prefixed stream, written in sorted order so two builds of one tree are byte-identical:
//! a sixteen-bit path length, the path, a sixty-four-bit data length, the data. Every length is
//! checked against the bytes that are actually there, and every path is checked to be a relative
//! path that stays inside the directory it is extracted into — an installer that wrote outside its
//! own directory would be an installer that could overwrite anything.

use std::path::{Component, Path, PathBuf};

/// The eight bytes that end an installer.
pub const FOOTER_MAGIC: [u8; 8] = *b"NCUISET1";

/// The size of the footer on the wire: the archive's offset and length, and the magic.
pub const FOOTER_BYTES: usize = 24;

/// The largest installer this will write or read.
///
/// A bundle is an application and a runtime; past this it is not one, and a reader that trusted the
/// footer's length would be reading whatever a damaged file claimed.
pub const MAX_INSTALLER_BYTES: u64 = 512 * 1024 * 1024;

/// The most files one archive may hold.
pub const MAX_ENTRIES: usize = 20_000;

/// The longest path one entry may name.
pub const MAX_PATH_BYTES: usize = 4_096;

/// One file of a bundle, as the archive holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The path, relative to the bundle's root, with `/` separators.
    pub path: String,
    /// The file's bytes.
    pub data: Vec<u8>,
}

/// A bundle as an installer carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    /// The files, in sorted path order.
    pub entries: Vec<Entry>,
}

impl Archive {
    /// The archive's total size on the wire.
    #[must_use]
    pub fn encoded_len(&self) -> u64 {
        self.entries
            .iter()
            .map(|entry| 2 + entry.path.len() as u64 + 8 + entry.data.len() as u64)
            .sum()
    }
}

/// Why an installer could not be written, read or unpacked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallerError {
    /// A file could not be read or written.
    Io {
        /// The path that failed.
        path: PathBuf,
        /// What the operating system said.
        reason: String,
    },
    /// The bundle holds more files than an archive may.
    TooManyEntries {
        /// How many it holds.
        count: usize,
    },
    /// A path is not one an archive may name.
    BadPath {
        /// The path.
        path: String,
        /// Why it cannot be used.
        reason: String,
    },
    /// The archive ends before the file it describes does.
    Truncated {
        /// How many bytes were available.
        available: usize,
        /// How many were needed.
        needed: usize,
    },
    /// The installer is larger than [`MAX_INSTALLER_BYTES`].
    TooLarge {
        /// How large it is.
        bytes: u64,
    },
    /// The footer names a range that is not in the file.
    BadFooter {
        /// Where the archive was said to start.
        offset: u64,
        /// How long it was said to be.
        length: u64,
        /// How long the file is.
        file: u64,
    },
}

impl core::fmt::Display for InstallerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io { path, reason } => write!(f, "{}: {reason}", path.display()),
            Self::TooManyEntries { count } => write!(
                f,
                "the bundle holds {count} files, more than the {MAX_ENTRIES} an installer may carry"
            ),
            Self::BadPath { path, reason } => {
                write!(f, "`{path}` is not a path an installer may write: {reason}")
            }
            Self::Truncated { available, needed } => write!(
                f,
                "the archive ends early: {needed} bytes are needed and {available} are there"
            ),
            Self::TooLarge { bytes } => write!(
                f,
                "the installer is {bytes} bytes, over the {MAX_INSTALLER_BYTES} one may be"
            ),
            Self::BadFooter {
                offset,
                length,
                file,
            } => write!(
                f,
                "the footer names an archive at {offset} of {length} bytes, which does not fit in a \
                 file of {file} bytes"
            ),
        }
    }
}

impl std::error::Error for InstallerError {}

/// The path an installer for `name` is written to, inside `out`.
#[must_use]
pub fn installer_path(out: &Path, name: &str) -> PathBuf {
    let suffix = if cfg!(windows) {
        "-setup.exe"
    } else {
        "-setup"
    };
    out.join(format!("{name}{suffix}"))
}

/// Packs a bundle directory into an archive.
///
/// # Errors
///
/// Returns [`InstallerError::Io`] when a file cannot be read, [`InstallerError::TooManyEntries`] when
/// the directory holds more files than an archive may, and [`InstallerError::BadPath`] when a file's
/// path cannot be carried.
pub fn pack(bundle: &Path) -> Result<Archive, InstallerError> {
    let mut files = Vec::new();
    collect(bundle, bundle, &mut files)?;
    if files.len() > MAX_ENTRIES {
        return Err(InstallerError::TooManyEntries { count: files.len() });
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut entries = Vec::with_capacity(files.len());
    for (path, full) in files {
        let data = std::fs::read(&full).map_err(|error| InstallerError::Io {
            path: full.clone(),
            reason: error.to_string(),
        })?;
        entries.push(Entry { path, data });
    }
    Ok(Archive { entries })
}

/// Collects every file under `directory`, as (relative path, full path).
fn collect(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), InstallerError> {
    let entries = std::fs::read_dir(directory).map_err(|error| InstallerError::Io {
        path: directory.to_path_buf(),
        reason: error.to_string(),
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| InstallerError::Io {
            path: directory.to_path_buf(),
            reason: error.to_string(),
        })?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|error| InstallerError::Io {
            path: path.clone(),
            reason: error.to_string(),
        })?;
        if kind.is_dir() {
            collect(root, &path, files)?;
        } else if kind.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| InstallerError::BadPath {
                    path: path.display().to_string(),
                    reason: "it is not under the bundle's root".to_owned(),
                })?;
            let mut name = String::new();
            for component in relative.components() {
                let Component::Normal(part) = component else {
                    return Err(InstallerError::BadPath {
                        path: relative.display().to_string(),
                        reason: "it is not a plain relative path".to_owned(),
                    });
                };
                if !name.is_empty() {
                    name.push('/');
                }
                name.push_str(&part.to_string_lossy());
            }
            if name.len() > MAX_PATH_BYTES {
                return Err(InstallerError::BadPath {
                    path: name,
                    reason: format!("it is longer than the {MAX_PATH_BYTES} bytes a path may be"),
                });
            }
            files.push((name, path));
        }
    }
    Ok(())
}

/// Writes an installer: `binary`, then the bundle, then the footer.
///
/// # Errors
///
/// Returns [`InstallerError`] when the binary or a bundle file cannot be read, when the bundle is not
/// packable, or when the result would be larger than [`MAX_INSTALLER_BYTES`].
pub fn write(bundle: &Path, binary: &Path, out: &Path) -> Result<PathBuf, InstallerError> {
    let program = std::fs::read(binary).map_err(|error| InstallerError::Io {
        path: binary.to_path_buf(),
        reason: error.to_string(),
    })?;
    let archive = pack(bundle)?;
    let length = archive.encoded_len();
    let total = program.len() as u64 + length + FOOTER_BYTES as u64;
    if total > MAX_INSTALLER_BYTES {
        return Err(InstallerError::TooLarge { bytes: total });
    }
    let name = bundle.file_name().map_or_else(
        || "application".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let path = installer_path(out, &name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| InstallerError::Io {
            path: parent.to_path_buf(),
            reason: error.to_string(),
        })?;
    }
    let mut bytes = Vec::with_capacity(usize::try_from(total).unwrap_or(0));
    bytes.extend_from_slice(&program);
    let offset = bytes.len() as u64;
    encode(&archive, &mut bytes)?;
    bytes.extend_from_slice(&offset.to_le_bytes());
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&FOOTER_MAGIC);
    std::fs::write(&path, &bytes).map_err(|error| InstallerError::Io {
        path: path.clone(),
        reason: error.to_string(),
    })?;
    // On Unix a file is not a program until it is marked executable, and `fs::write` creates a
    // file only its owner may read. An installer nobody can run is not an installer.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(
            |error| InstallerError::Io {
                path: path.clone(),
                reason: error.to_string(),
            },
        )?;
    }
    Ok(path)
}

/// Writes a bundle as a bare archive, which is what a release publishes.
///
/// The same bytes an installer carries after its program: an application that fetches a release and
/// one that was handed a self-contained installer read the same archive, so a release pipeline
/// cannot produce something the installer would not accept.
///
/// # Errors
///
/// Returns what [`pack`] and the installer's own `write` report: a bundle that cannot be read, too many files, or a
/// path that cannot be carried.
pub fn write_archive(bundle: &Path, out: &Path) -> Result<PathBuf, InstallerError> {
    let archive = pack(bundle)?;
    let mut bytes = Vec::with_capacity(usize::try_from(archive.encoded_len()).unwrap_or(0));
    encode(&archive, &mut bytes)?;
    let path = out.join("bundle.tar");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| InstallerError::Io {
            path: parent.to_path_buf(),
            reason: error.to_string(),
        })?;
    }
    std::fs::write(&path, &bytes).map_err(|error| InstallerError::Io {
        path: path.clone(),
        reason: error.to_string(),
    })?;
    Ok(path)
}

/// Appends an archive's entries to `bytes`, in the format [`unpack`] reads.
fn encode(archive: &Archive, bytes: &mut Vec<u8>) -> Result<(), InstallerError> {
    for entry in &archive.entries {
        let path_len = u16::try_from(entry.path.len()).map_err(|_| InstallerError::BadPath {
            path: entry.path.clone(),
            reason: format!("it is longer than the {MAX_PATH_BYTES} bytes a path may be"),
        })?;
        bytes.extend_from_slice(&path_len.to_le_bytes());
        bytes.extend_from_slice(entry.path.as_bytes());
        bytes.extend_from_slice(&(entry.data.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&entry.data);
    }
    Ok(())
}

/// The archive an installer carries, or `None` when the bytes are just a runtime.
///
/// # Errors
///
/// Returns [`InstallerError`] when the footer is there but does not describe an archive this can
/// read. A file with no footer is `None`, not an error: every runtime is such a file.
pub fn read(bytes: &[u8]) -> Option<Result<Archive, InstallerError>> {
    if bytes.len() < FOOTER_BYTES {
        return None;
    }
    let footer = &bytes[bytes.len() - FOOTER_BYTES..];
    if footer[16..24] != FOOTER_MAGIC {
        return None;
    }
    Some(read_footer(bytes, footer))
}

/// Reads the archive a footer that has already been found describes.
fn read_footer(bytes: &[u8], footer: &[u8]) -> Result<Archive, InstallerError> {
    let offset = u64::from_le_bytes([
        footer[0], footer[1], footer[2], footer[3], footer[4], footer[5], footer[6], footer[7],
    ]);
    let length = u64::from_le_bytes([
        footer[8], footer[9], footer[10], footer[11], footer[12], footer[13], footer[14],
        footer[15],
    ]);
    let file = bytes.len() as u64;
    let end = offset
        .checked_add(length)
        .ok_or(InstallerError::BadFooter {
            offset,
            length,
            file,
        })?;
    if end > file.saturating_sub(FOOTER_BYTES as u64) {
        return Err(InstallerError::BadFooter {
            offset,
            length,
            file,
        });
    }
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    let size = usize::try_from(length).unwrap_or(usize::MAX);
    let body = bytes
        .get(start..start.saturating_add(size))
        .ok_or(InstallerError::BadFooter {
            offset,
            length,
            file,
        })?;
    unpack(body)
}

/// Unpacks the archive in `body`.
///
/// # Errors
///
/// Returns [`InstallerError::Truncated`] when the bytes end before the entry they describe does, and
/// [`InstallerError::TooManyEntries`] when the archive holds more entries than one may.
pub fn unpack(body: &[u8]) -> Result<Archive, InstallerError> {
    let mut entries = Vec::new();
    let mut at = 0_usize;
    while at < body.len() {
        if entries.len() >= MAX_ENTRIES {
            return Err(InstallerError::TooManyEntries {
                count: entries.len(),
            });
        }
        let path_len = read_u16(body, at)?;
        at += 2;
        let path_end = at.saturating_add(usize::from(path_len));
        let path_bytes = body.get(at..path_end).ok_or(InstallerError::Truncated {
            available: body.len(),
            needed: path_end,
        })?;
        let path = String::from_utf8(path_bytes.to_vec()).map_err(|_| InstallerError::BadPath {
            path: String::from_utf8_lossy(path_bytes).into_owned(),
            reason: "it is not UTF-8".to_owned(),
        })?;
        at = path_end;
        let data_len = read_u64(body, at)?;
        at += 8;
        let data_end = at.saturating_add(usize::try_from(data_len).unwrap_or(usize::MAX));
        let data = body.get(at..data_end).ok_or(InstallerError::Truncated {
            available: body.len(),
            needed: data_end,
        })?;
        at = data_end;
        entries.push(Entry {
            path,
            data: data.to_vec(),
        });
    }
    Ok(Archive { entries })
}

/// Reads a little-endian 16-bit value at `at`.
fn read_u16(bytes: &[u8], at: usize) -> Result<u16, InstallerError> {
    let pair = bytes
        .get(at..at.saturating_add(2))
        .ok_or(InstallerError::Truncated {
            available: bytes.len(),
            needed: at.saturating_add(2),
        })?;
    Ok(u16::from_le_bytes([pair[0], pair[1]]))
}

/// Reads a little-endian 64-bit value at `at`.
fn read_u64(bytes: &[u8], at: usize) -> Result<u64, InstallerError> {
    let word = bytes
        .get(at..at.saturating_add(8))
        .ok_or(InstallerError::Truncated {
            available: bytes.len(),
            needed: at.saturating_add(8),
        })?;
    Ok(u64::from_le_bytes([
        word[0], word[1], word[2], word[3], word[4], word[5], word[6], word[7],
    ]))
}

/// Writes an archive's files under `into`, creating the directories they need.
///
/// # Errors
///
/// Returns [`InstallerError::BadPath`] for an entry whose path is absolute, climbs out of `into`, or
/// names something other than a plain relative path, and [`InstallerError::Io`] when a file cannot be
/// written.
pub fn extract(archive: &Archive, into: &Path) -> Result<usize, InstallerError> {
    let mut written = 0_usize;
    for entry in &archive.entries {
        let relative = safe_relative(&entry.path)?;
        let target = into.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| InstallerError::Io {
                path: parent.to_path_buf(),
                reason: error.to_string(),
            })?;
        }
        std::fs::write(&target, &entry.data).map_err(|error| InstallerError::Io {
            path: target.clone(),
            reason: error.to_string(),
        })?;
        written += 1;
    }
    Ok(written)
}

/// The bundle this program carries, extracted into a directory that removes itself.
///
/// `None` when this program is not an installer — every runtime is a program with no footer, and the
/// absence of a bundle is not an error.
///
/// # Errors
///
/// Returns [`InstallerError`] when the footer is there but the archive cannot be read or written.
pub fn self_install() -> Option<Result<TempBundle, InstallerError>> {
    let Ok(path) = std::env::current_exe() else {
        return None;
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Some(Err(InstallerError::Io {
                path,
                reason: error.to_string(),
            }));
        }
    };
    let archive = read(&bytes)?;
    Some(archive.and_then(|archive| {
        let directory = private_directory()?;
        extract(&archive, &directory.path)?;
        Ok(directory)
    }))
}

/// A directory that removes itself when it goes out of scope.
///
/// The extracted bundle is a copy the install reads from and then forgets: leaving it behind would
/// leave a second copy of the application on the user's disk, in a directory they never chose.
#[derive(Debug)]
pub struct TempBundle {
    /// Where the bundle was extracted.
    path: PathBuf,
}

impl TempBundle {
    /// The directory the bundle was extracted into.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempBundle {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A private directory with a name nobody can predict.
///
/// Private because the bundle is written into it before it is installed: on Unix the directory is
/// owner-only, so another user cannot read or replace a file between the write and the install, and
/// on Windows the system's own scratch directory is already per-user.
pub(crate) fn private_directory() -> Result<TempBundle, InstallerError> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let path = std::env::temp_dir().join(format!("ncui-setup-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&path).map_err(|error| InstallerError::Io {
        path: path.clone(),
        reason: error.to_string(),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).map_err(
            |error| InstallerError::Io {
                path: path.clone(),
                reason: error.to_string(),
            },
        )?;
    }
    Ok(TempBundle { path })
}

/// Checks that `path` is a relative path that stays inside the directory it is joined onto.
fn safe_relative(path: &str) -> Result<PathBuf, InstallerError> {
    if path.is_empty() {
        return Err(InstallerError::BadPath {
            path: path.to_owned(),
            reason: "it is empty".to_owned(),
        });
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(InstallerError::BadPath {
            path: path.to_owned(),
            reason: format!("it is longer than the {MAX_PATH_BYTES} bytes a path may be"),
        });
    }
    let mut relative = PathBuf::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(InstallerError::BadPath {
                path: path.to_owned(),
                reason: "it must be a plain relative path with no empty, `.` or `..` component"
                    .to_owned(),
            });
        }
        // A backslash is a separator on Windows and a character on Unix; refusing it everywhere
        // keeps one archive from meaning two different paths on two platforms.
        if part.contains(['\\', ':', '\0']) {
            return Err(InstallerError::BadPath {
                path: path.to_owned(),
                reason: "it names a separator, a drive or a NUL byte".to_owned(),
            });
        }
        relative.push(part);
    }
    Ok(relative)
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
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A directory this test owns and removes.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "ncui-installer-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("the scratch directory is creatable");
            Self { path }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// A bundle: a manifest, an entry page and a nested file.
    fn bundle(root: &Path) {
        std::fs::create_dir_all(root.join("public").join("pages")).expect("the tree is created");
        std::fs::write(root.join("bundle.json"), b"{\"name\":\"counter\"}").expect("written");
        std::fs::write(root.join("public").join("index.html"), b"<h1>hi</h1>").expect("written");
        std::fs::write(
            root.join("public").join("pages").join("a.html"),
            b"<p>a</p>",
        )
        .expect("written");
    }

    /// A stand-in for the runtime binary: bytes that are not a program, which is all the installer
    /// needs of them.
    fn program(scratch: &Path) -> PathBuf {
        let path = scratch.join("ncui");
        std::fs::write(
            &path,
            b"MZ-not-really-a-program-but-the-installer-does-not-care",
        )
        .expect("written");
        path
    }

    #[test]
    fn an_installer_carries_the_bundle_after_the_program() {
        let scratch = Scratch::new("round-trip");
        let source = scratch.path.join("bundle");
        bundle(&source);
        let program = program(&scratch.path);
        let out = scratch.path.join("dist");
        let installer = write(&source, &program, &out).expect("an installer is written");
        let bytes = std::fs::read(&installer).expect("the installer is readable");

        // The program's own bytes come first, unchanged: the file is still the runtime.
        let program_bytes = std::fs::read(&program).expect("the program is readable");
        assert_eq!(&bytes[..program_bytes.len()], program_bytes.as_slice());

        let archive = read(&bytes)
            .expect("the installer has a footer")
            .expect("it reads");
        assert_eq!(archive.entries.len(), 3);
        assert_eq!(archive.entries[0].path, "bundle.json", "entries are sorted");
        assert_eq!(archive.entries[1].path, "public/index.html");
        assert_eq!(archive.entries[2].path, "public/pages/a.html");

        // Extraction reproduces the tree.
        let unpacked = scratch.path.join("unpacked");
        let written = extract(&archive, &unpacked).expect("the archive extracts");
        assert_eq!(written, 3);
        assert_eq!(
            std::fs::read(unpacked.join("public").join("pages").join("a.html")).expect("read"),
            b"<p>a</p>"
        );
        assert_eq!(
            std::fs::read(unpacked.join("bundle.json")).expect("read"),
            b"{\"name\":\"counter\"}"
        );
    }

    #[test]
    fn a_runtime_with_no_bundle_has_no_installer() {
        // Every runtime is a file without a footer, and reading one must not mistake it for an
        // installer: the absence of a bundle is not a damaged bundle.
        assert!(read(b"just a program, no footer at all").is_none());
        assert!(read(&[]).is_none());
        assert!(read(&[0_u8; 8]).is_none());
    }

    #[test]
    fn a_footer_that_points_outside_the_file_is_refused() {
        let mut bytes = b"program".to_vec();
        bytes.extend_from_slice(&1_000_000_u64.to_le_bytes());
        bytes.extend_from_slice(&1_000_000_u64.to_le_bytes());
        bytes.extend_from_slice(&FOOTER_MAGIC);
        let error = read(&bytes)
            .expect("a footer is there")
            .expect_err("it is refused");
        assert!(matches!(error, InstallerError::BadFooter { .. }), "{error}");
    }

    #[test]
    fn an_archive_that_ends_early_is_refused() {
        let mut body = Vec::new();
        body.extend_from_slice(&4_u16.to_le_bytes());
        body.extend_from_slice(b"a.tx");
        body.extend_from_slice(&100_u64.to_le_bytes());
        let error = unpack(&body).expect_err("a short archive is refused");
        assert!(matches!(error, InstallerError::Truncated { .. }), "{error}");
    }

    #[test]
    fn a_path_that_leaves_the_directory_is_refused() {
        for path in [
            "../outside",
            "/absolute",
            "a/../../b",
            "",
            "a//b",
            "a\\b",
            "C:/x",
        ] {
            let archive = Archive {
                entries: vec![Entry {
                    path: path.to_owned(),
                    data: b"x".to_vec(),
                }],
            };
            let scratch = Scratch::new("escape");
            let error = extract(&archive, &scratch.path)
                .expect_err("a path that leaves the directory is refused");
            assert!(
                matches!(error, InstallerError::BadPath { .. }),
                "{path}: {error}"
            );
        }
        // Nothing was written outside the directory, and nothing inside it either.
        let scratch = Scratch::new("escape-clean");
        assert_eq!(
            std::fs::read_dir(&scratch.path).expect("readable").count(),
            0
        );
    }

    #[test]
    fn a_program_that_is_not_an_installer_carries_no_bundle() {
        // The test binary is a program with no footer, which is exactly what every runtime is: the
        // absence of a bundle must not be mistaken for a damaged one.
        assert!(self_install().is_none());
    }

    #[test]
    fn two_builds_of_one_tree_are_byte_identical() {
        let scratch = Scratch::new("deterministic");
        let source = scratch.path.join("bundle");
        bundle(&source);
        let program = program(&scratch.path);
        let first = write(&source, &program, &scratch.path.join("one")).expect("written");
        let second = write(&source, &program, &scratch.path.join("two")).expect("written");
        assert_eq!(
            std::fs::read(&first).expect("readable"),
            std::fs::read(&second).expect("readable"),
            "the archive is sorted, so the same tree gives the same bytes"
        );
    }
}
