//! Installing a bundle: putting it somewhere it can run from, and registering what makes it an
//! application rather than a directory of files.
//!
//! # What an install is
//!
//! Three things, in this order, and the order is the whole design:
//!
//! 1. **The files.** The bundle is copied into a directory named after the application, under a
//!    root the caller chooses. Nothing is written outside that root.
//! 2. **The registrations.** An application identity, so the platform knows the name to attribute
//!    its notifications to, and a URL scheme, so a link finds it. Both are written *after* the
//!    files are in place, because both name the installed program's path — a registration written
//!    first would point at a directory that does not exist yet.
//! 3. **A record of what was done.** `installed.json` names the identity and the scheme that were
//!    registered, so [`Installer::uninstall`] removes exactly what this install created rather
//!    than guessing from the current state of the machine.
//!
//! # Why the record matters
//!
//! An uninstall that re-derived the registrations from the bundle would be an uninstall that
//! removed whatever the *bundle* says — which is a different thing from what was installed, if
//! the bundle changed after it was installed. The record is what makes removal exact.
//!
//! # What is not here
//!
//! No MSI, no per-machine install, and no upgrade path: this installs for one user into one
//! directory, and `docs/known-gaps.md` records the rest. An install that replaced files while an
//! application was running would also need to know what was running, which is the running
//! instance's business rather than this module's. The installer *executable* is written by
//! `ncui build --installer`, which carries a bundle and installs it with this module rather than
//! with a second implementation.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::identity;
use crate::protocol;

/// The bundle's own manifest, written by the bundler.
pub const BUNDLE_MANIFEST_NAME: &str = "bundle.json";

/// What an install records about itself.
pub const INSTALL_RECORD_NAME: &str = "installed.json";

/// The most entries a bundle may describe.
pub const MAX_BUNDLE_FILES: usize = 4096;

/// Why an install, an uninstall or a listing failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// The directory is not a bundle this installer recognises.
    NotABundle {
        /// What was wrong.
        reason: String,
    },
    /// The application's name cannot be a directory name.
    NameUnusable {
        /// The name.
        name: String,
    },
    /// Something is already installed under that name.
    AlreadyInstalled {
        /// The name.
        name: String,
    },
    /// Nothing is installed under that name.
    NotInstalled {
        /// The name.
        name: String,
    },
    /// A file is not what the bundle says it is.
    Damaged {
        /// The path within the bundle.
        path: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The filesystem refused.
    Io {
        /// The path involved.
        path: String,
        /// What failed.
        reason: String,
    },
    /// A registration was refused.
    Registration {
        /// What was being registered.
        what: &'static str,
        /// Why.
        reason: String,
    },
}

impl InstallError {
    /// The stable code a caller sees on the wire.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotABundle { .. } => "install_not_a_bundle",
            Self::NameUnusable { .. } => "install_name_unusable",
            Self::AlreadyInstalled { .. } => "install_already_installed",
            Self::NotInstalled { .. } => "install_not_installed",
            Self::Damaged { .. } => "install_damaged",
            Self::Io { .. } => "install_io",
            Self::Registration { .. } => "install_registration_refused",
        }
    }
}

impl core::fmt::Display for InstallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotABundle { reason } => write!(f, "this is not an application bundle: {reason}"),
            Self::NameUnusable { name } => {
                write!(f, "`{name}` cannot be an application's directory name")
            }
            Self::AlreadyInstalled { name } => write!(
                f,
                "`{name}` is already installed here; uninstall it first rather than installing \
                 over it"
            ),
            Self::NotInstalled { name } => write!(f, "`{name}` is not installed here"),
            Self::Damaged { path, reason } => {
                write!(f, "`{path}` is not what the bundle says it is: {reason}")
            }
            Self::Io { path, reason } => write!(f, "{path}: {reason}"),
            Self::Registration { what, reason } => {
                write!(f, "the {what} could not be registered: {reason}")
            }
        }
    }
}

impl std::error::Error for InstallError {}

/// The bundle's manifest, as the installer reads it.
///
/// A subset of what the bundler writes: the fields an install needs. The rest of the bundle's
/// manifest describes files and digests, which belong to whoever verifies the bundle rather than
/// to whoever installs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleManifest {
    /// The application's name.
    pub name: String,
    /// Its version.
    #[serde(default)]
    pub version: String,
    /// The runtime binary's file name inside the bundle.
    pub runtime: String,
    /// The identity the bundle registers, when it names one.
    #[serde(default)]
    pub identity: Option<String>,
    /// The URL scheme the bundle registers, when it names one.
    #[serde(default)]
    pub scheme: Option<String>,
    /// Every file the bundle says it holds, with the size and digest it should have.
    ///
    /// Required rather than optional: a bundle that does not say what its files are is a bundle
    /// whose contents nobody can check, and installing one would be installing whatever bytes
    /// happened to be on disk.
    pub files: Vec<BundleFile>,
}

/// One file a bundle says it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleFile {
    /// The path within the bundle, with forward slashes.
    pub path: String,
    /// How many bytes it is.
    pub bytes: u64,
    /// Its SHA-256, as lowercase hex.
    pub sha256: String,
}

/// What an install created, and what an uninstall removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    /// The application's name.
    pub name: String,
    /// Its version.
    pub version: String,
    /// Where it was installed.
    pub directory: String,
    /// The binary that runs it.
    pub program: String,
    /// The identity registered, if any.
    #[serde(default)]
    pub identity: Option<String>,
    /// The scheme registered, if any.
    #[serde(default)]
    pub scheme: Option<String>,
    /// What this install did, in the order it did it, for the record a person reads.
    #[serde(default)]
    pub registrations: Vec<String>,
}

/// What an update replaced, and what it installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Updated {
    /// What was installed before.
    pub previous: Installed,
    /// What is installed now.
    pub installed: Installed,
}

/// Where an application is installed for this user, when no root is given.
///
/// Per user, and under a directory the platform already treats as this user's own: an install
/// that needed an administrator to write one directory would be an install most people cannot
/// perform. The layout is the platform's convention rather than one this project invents.
#[must_use]
pub fn default_root() -> PathBuf {
    if cfg!(windows) {
        let base =
            std::env::var("LOCALAPPDATA").map_or_else(|_| std::env::temp_dir(), PathBuf::from);
        return base.join("NC-UI").join("Applications");
    }
    if cfg!(target_os = "macos") {
        let home = std::env::var("HOME").map_or_else(|_| std::env::temp_dir(), PathBuf::from);
        return home.join("Library/Application Support/NC-UI/Applications");
    }
    let home = std::env::var("HOME").map_or_else(|_| std::env::temp_dir(), PathBuf::from);
    home.join(".local/share/ncui/applications")
}

/// Where this user's application state lives.
///
/// Beside the applications rather than inside one: a bundled application may sit in a directory
/// its user cannot write, and what a window remembers about itself belongs to the user who moved
/// it, not to the directory it was installed from.
#[must_use]
pub fn state_root() -> PathBuf {
    let mut root = default_root();
    root.pop();
    root.push("State");
    root
}

/// Installs bundles under one root, and removes them again.
#[derive(Debug, Clone)]
pub struct Installer {
    root: PathBuf,
}

impl Installer {
    /// An installer whose applications live under `root`.
    ///
    /// # Errors
    ///
    /// [`InstallError::Io`] when the root cannot be created.
    pub fn new(root: &Path) -> Result<Self, InstallError> {
        std::fs::create_dir_all(root).map_err(|error| InstallError::Io {
            path: root.display().to_string(),
            reason: error.to_string(),
        })?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// The root applications are installed under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Installs the bundle at `bundle`.
    ///
    /// # Errors
    ///
    /// [`InstallError::NotABundle`] when the directory holds no readable bundle manifest,
    /// [`InstallError::NameUnusable`] for a name that cannot be a directory name,
    /// [`InstallError::AlreadyInstalled`] rather than installing over something,
    /// [`InstallError::Io`] when a copy fails, and [`InstallError::Registration`] when a
    /// registration is refused — in which case the files stay installed and the reason says what
    /// could not be registered.
    pub fn install(&self, bundle: &Path) -> Result<Installed, InstallError> {
        let manifest = read_bundle(bundle)?;
        if !is_directory_name(&manifest.name) {
            return Err(InstallError::NameUnusable {
                name: manifest.name,
            });
        }
        let directory = self.root.join(&manifest.name);
        if directory.exists() {
            return Err(InstallError::AlreadyInstalled {
                name: manifest.name,
            });
        }
        let program = directory.join(&manifest.runtime);
        if manifest.runtime.is_empty() || manifest.runtime.contains(['/', '\\']) {
            return Err(InstallError::NotABundle {
                reason: format!("`{}` is not a file name for the runtime", manifest.runtime),
            });
        }

        copy_tree(bundle, &directory)?;
        // What was copied is checked against what the bundle said it was, and a copy that does not
        // match is removed rather than left half-installed: an application that is not the one the
        // bundle describes is not an application this should have installed.
        if let Err(error) = verify_tree(&directory, &manifest) {
            let _ = std::fs::remove_dir_all(&directory);
            return Err(error);
        }

        // The registrations come after the files, and each names the *installed* program: a
        // registration written first would point at a directory that is not there yet.
        let Registrations {
            identity: identity_registered,
            scheme: scheme_registered,
            done: registrations,
        } = register(&manifest, &program, &directory)?;

        let installed = Installed {
            name: manifest.name,
            version: manifest.version,
            directory: directory.display().to_string(),
            program: program.display().to_string(),
            identity: identity_registered,
            scheme: scheme_registered,
            registrations,
        };
        write_record(&directory, &installed)?;
        Ok(installed)
    }

    /// Replaces an installed application with another version of itself.
    ///
    /// An update is not an uninstall followed by an install: that would leave a window in which no
    /// application is installed at all, and a failure in the middle would leave the user with
    /// neither the old version nor the new one. It is a staged copy and two renames instead — the
    /// new files are written *beside* the installed ones, the installed directory is moved aside,
    /// the staged one is moved into its place, and the old copy is removed only then. A rename that
    /// fails puts the old copy back, so an update that cannot complete leaves what was there.
    ///
    /// The registrations follow the swap: what the new bundle names is registered, and what the old
    /// install registered and the new bundle does not is removed. That order matters — removing
    /// first would leave a moment in which the application has no registration at all, and a
    /// failure there would leave it that way.
    ///
    /// # Errors
    ///
    /// [`InstallError::NotInstalled`] when the name is not installed here, and otherwise what
    /// [`Installer::install`] reports: [`InstallError::NameUnusable`], [`InstallError::NotABundle`],
    /// [`InstallError::Io`] and [`InstallError::Registration`].
    pub fn update(&self, bundle: &Path) -> Result<Updated, InstallError> {
        let manifest = read_bundle(bundle)?;
        if !is_directory_name(&manifest.name) {
            return Err(InstallError::NameUnusable {
                name: manifest.name,
            });
        }
        if manifest.runtime.is_empty() || manifest.runtime.contains(['/', '\\']) {
            return Err(InstallError::NotABundle {
                reason: format!("`{}` is not a file name for the runtime", manifest.runtime),
            });
        }
        let directory = self.root.join(&manifest.name);
        if !directory.is_dir() {
            return Err(InstallError::NotInstalled {
                name: manifest.name,
            });
        }
        let previous = read_record(&directory)?;

        // The staging names carry a stamp so two updates cannot collide, and a staging directory
        // left by a run that died before it could finish is removed rather than copied over.
        let stamp = stamp();
        let staged = self.root.join(format!("{}.staged-{stamp}", manifest.name));
        let replaced = self
            .root
            .join(format!("{}.replaced-{stamp}", manifest.name));
        let _ = std::fs::remove_dir_all(&staged);
        copy_tree(bundle, &staged)?;
        // The staged copy is checked before anything is moved: a bundle that is not what it says it
        // is must not be able to replace a version that is.
        if let Err(error) = verify_tree(&staged, &manifest) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(error);
        }

        if let Err(error) = std::fs::rename(&directory, &replaced) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(InstallError::Io {
                path: directory.display().to_string(),
                reason: error.to_string(),
            });
        }
        if let Err(error) = std::fs::rename(&staged, &directory) {
            // The new copy could not take the old one's place, so the old one goes back. An update
            // that cannot finish leaves the application it found.
            let _ = std::fs::rename(&replaced, &directory);
            let _ = std::fs::remove_dir_all(&staged);
            return Err(InstallError::Io {
                path: directory.display().to_string(),
                reason: error.to_string(),
            });
        }
        let _ = std::fs::remove_dir_all(&replaced);

        let program = directory.join(&manifest.runtime);
        let Registrations {
            identity: identity_registered,
            scheme: scheme_registered,
            done: mut registrations,
        } = register(&manifest, &program, &directory)?;

        // What the previous install registered and this bundle does not name is removed, because a
        // registration the application no longer wants is a scheme or an identity it keeps claiming.
        if let Some(scheme) = previous.scheme.as_deref()
            && scheme_registered.as_deref() != Some(scheme)
        {
            let _ = protocol::unregister(scheme);
            registrations.push(format!("scheme {scheme} (released)"));
        }
        if let Some(identity) = previous.identity.as_deref()
            && identity_registered.as_deref() != Some(identity)
            && identity::is_supported()
        {
            let _ = identity::unregister(identity);
            registrations.push(format!("identity {identity} (released)"));
        }

        let installed = Installed {
            name: manifest.name,
            version: manifest.version,
            directory: directory.display().to_string(),
            program: program.display().to_string(),
            identity: identity_registered,
            scheme: scheme_registered,
            registrations,
        };
        write_record(&directory, &installed)?;
        Ok(Updated {
            previous,
            installed,
        })
    }

    /// Removes the application installed as `name`, with the registrations it made.
    ///
    /// # Errors
    ///
    /// [`InstallError::NameUnusable`] for a name that could not have been installed,
    /// [`InstallError::NotInstalled`] when nothing is there, and [`InstallError::Io`] when the
    /// directory cannot be removed. A registration that cannot be removed is reported as
    /// [`InstallError::Registration`], and the directory is left alone so the record survives to
    /// try again.
    pub fn uninstall(&self, name: &str) -> Result<Installed, InstallError> {
        if !is_directory_name(name) {
            return Err(InstallError::NameUnusable {
                name: name.to_owned(),
            });
        }
        let directory = self.root.join(name);
        let installed = read_record(&directory)?;

        // The record is what was installed, so removal is exact: what the bundle says *now* may
        // differ from what was registered then.
        if let Some(scheme) = installed.scheme.as_deref() {
            protocol::unregister(scheme).map_err(|error| InstallError::Registration {
                what: "URL scheme",
                reason: error.to_string(),
            })?;
        }
        if let Some(identity) = installed.identity.as_deref()
            && identity::is_supported()
        {
            identity::unregister(identity).map_err(|error| InstallError::Registration {
                what: "application identity",
                reason: error.to_string(),
            })?;
        }
        std::fs::remove_dir_all(&directory).map_err(|error| InstallError::Io {
            path: directory.display().to_string(),
            reason: error.to_string(),
        })?;
        Ok(installed)
    }

    /// What is installed here, in the order the names sort.
    ///
    /// # Errors
    ///
    /// [`InstallError::Io`] when the root cannot be read, and [`InstallError::NotABundle`] when a
    /// directory under it holds a record that cannot be read — a corrupt record is reported
    /// rather than skipped, because an application nobody can list is one nobody can uninstall.
    pub fn installed(&self) -> Result<Vec<Installed>, InstallError> {
        let entries = std::fs::read_dir(&self.root).map_err(|error| InstallError::Io {
            path: self.root.display().to_string(),
            reason: error.to_string(),
        })?;
        let mut installed = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| InstallError::Io {
                path: self.root.display().to_string(),
                reason: error.to_string(),
            })?;
            let path = entry.path();
            // A run that died between the two renames of an update leaves a staging directory
            // behind, and it holds a record because it holds the files that were installed. It is
            // not an application: it is the remains of one operation, and listing it would offer
            // the user an application whose name is a stamp.
            if is_staging_name(&entry.file_name().to_string_lossy()) {
                continue;
            }
            if !path.is_dir() || !path.join(INSTALL_RECORD_NAME).is_file() {
                continue;
            }
            installed.push(read_record(&path)?);
        }
        installed.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(installed)
    }
}

/// Whether `name` is one of the directory names an update uses while it works.
///
/// The two names an update creates are reserved: a directory called `<name>.staged-…` is a copy
/// being prepared and one called `<name>.replaced-…` is the version being replaced. Neither is an
/// application, whether the update that made it is still running or ended badly.
#[must_use]
pub fn is_staging_name(name: &str) -> bool {
    name.contains(".staged-") || name.contains(".replaced-")
}

/// A stamp that distinguishes one update's staging directories from another's.
///
/// The clock rather than a counter, because the two updates it must separate are usually in
/// different processes: two runs started in the same instant are separated by the process id.
fn stamp() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

/// Whether `name` can be a directory name for an application.
///
/// Deliberately narrow: letters, digits, dashes and underscores. A name is used as a path
/// component and inside a registry key, so anything that means something to either is refused
/// rather than translated.
#[must_use]
pub fn is_directory_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

/// Reads a bundle's manifest.
fn read_bundle(bundle: &Path) -> Result<BundleManifest, InstallError> {
    let path = bundle.join(BUNDLE_MANIFEST_NAME);
    let encoded = std::fs::read(&path).map_err(|error| InstallError::NotABundle {
        reason: format!("{}: {error}", path.display()),
    })?;
    serde_json::from_slice(&encoded).map_err(|error| InstallError::NotABundle {
        reason: format!("{} is not readable as one: {error}", path.display()),
    })
}

/// Copies a bundle's tree into `destination`.
fn copy_tree(source: &Path, destination: &Path) -> Result<(), InstallError> {
    let mut stack = vec![(source.to_path_buf(), destination.to_path_buf())];
    let mut copied = 0usize;
    while let Some((from, to)) = stack.pop() {
        std::fs::create_dir_all(&to).map_err(|error| InstallError::Io {
            path: to.display().to_string(),
            reason: error.to_string(),
        })?;
        let entries = std::fs::read_dir(&from).map_err(|error| InstallError::Io {
            path: from.display().to_string(),
            reason: error.to_string(),
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| InstallError::Io {
                path: from.display().to_string(),
                reason: error.to_string(),
            })?;
            let path = entry.path();
            let target = to.join(entry.file_name());
            if path.is_dir() {
                stack.push((path, target));
                continue;
            }
            copied += 1;
            if copied > MAX_BUNDLE_FILES {
                return Err(InstallError::NotABundle {
                    reason: format!("it holds more than {MAX_BUNDLE_FILES} files"),
                });
            }
            std::fs::copy(&path, &target).map_err(|error| InstallError::Io {
                path: target.display().to_string(),
                reason: error.to_string(),
            })?;
        }
    }
    Ok(())
}

/// What a registration pass recorded.
#[derive(Debug)]
struct Registrations {
    /// The identity that was registered, when the bundle names one this platform keeps.
    identity: Option<String>,
    /// The URL scheme that was registered, when the bundle names one.
    scheme: Option<String>,
    /// What the pass did, in the order it did it, for the record a person reads.
    done: Vec<String>,
}

/// Registers what a bundle names: its application identity and its URL scheme.
///
/// Run after the files are in place, because both registrations name the *installed* program: one
/// written first would point at a directory that is not there yet.
///
/// # Errors
///
/// Returns [`InstallError::Registration`] when the platform refuses either registration. The files
/// stay installed and the reason says what could not be registered.
fn register(
    manifest: &BundleManifest,
    program: &Path,
    directory: &Path,
) -> Result<Registrations, InstallError> {
    let mut done = Vec::new();
    let mut identity_registered = None;
    if let Some(value) = manifest.identity.as_deref() {
        if identity::is_supported() {
            identity::register(value, &manifest.name).map_err(|error| {
                InstallError::Registration {
                    what: "application identity",
                    reason: error.to_string(),
                }
            })?;
            identity_registered = Some(value.to_owned());
            done.push(format!("identity {value}"));
        } else {
            done.push(format!("identity {value} (this platform keeps none)"));
        }
    }
    let mut scheme_registered = None;
    if let Some(value) = manifest.scheme.as_deref() {
        let arguments = vec!["run".to_owned(), directory.display().to_string()];
        protocol::register(value, program, &arguments).map_err(|error| {
            InstallError::Registration {
                what: "URL scheme",
                reason: error.to_string(),
            }
        })?;
        scheme_registered = Some(value.to_owned());
        done.push(format!("scheme {value}"));
    }
    Ok(Registrations {
        identity: identity_registered,
        scheme: scheme_registered,
        done,
    })
}

/// Checks a copied tree against the files the bundle said it holds.
///
/// Both halves matter. A file whose size or digest differs is not the file the bundle described, and
/// a file the bundle does not name is one nobody checked — either way the tree is not the bundle, and
/// installing it would be installing something nobody has a claim about.
///
/// # Errors
///
/// Returns [`InstallError::Damaged`] for a file that is not what the bundle says, and
/// [`InstallError::Io`] when the tree cannot be walked.
fn verify_tree(directory: &Path, manifest: &BundleManifest) -> Result<(), InstallError> {
    let mut named: Vec<&str> = manifest
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    named.sort_unstable();
    for file in &manifest.files {
        check_file(directory, file)?;
    }
    // A file the bundle does not name was never checked, so it is not installed either. The
    // bundle's own manifest is the exception: it cannot name itself, and it is the file that
    // carries the digests — a caller who trusts it has already trusted it. What makes a bundle
    // from elsewhere trustworthy is its transport, which is where its own digest belongs.
    for path in walk(directory, directory)? {
        if path == BUNDLE_MANIFEST_NAME {
            continue;
        }
        if !named.contains(&path.as_str()) {
            return Err(InstallError::Damaged {
                path,
                reason: "the bundle holds it and does not name it, so nothing has checked it"
                    .to_owned(),
            });
        }
    }
    Ok(())
}

/// Checks one file against the record the bundle carries for it.
///
/// # Errors
///
/// Returns [`InstallError::Damaged`] when the file is missing, is a different size, or hashes to
/// something else, and [`InstallError::Io`] when it cannot be read.
fn check_file(directory: &Path, file: &BundleFile) -> Result<(), InstallError> {
    let full = directory.join(file.path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let metadata = std::fs::metadata(&full).map_err(|error| InstallError::Damaged {
        path: file.path.clone(),
        reason: format!("the bundle names it and it is not there: {error}"),
    })?;
    if metadata.len() != file.bytes {
        return Err(InstallError::Damaged {
            path: file.path.clone(),
            reason: format!(
                "the bundle says {} bytes and it is {}",
                file.bytes,
                metadata.len()
            ),
        });
    }
    let digest = digest_of(&full)?;
    if !digest.eq_ignore_ascii_case(&file.sha256) {
        return Err(InstallError::Damaged {
            path: file.path.clone(),
            reason: format!(
                "the bundle says its digest is {} and it is {digest}",
                file.sha256
            ),
        });
    }
    Ok(())
}

/// Every file under `directory`, as paths relative to `root` with forward slashes.
fn walk(root: &Path, directory: &Path) -> Result<Vec<String>, InstallError> {
    let mut found = Vec::new();
    let mut stack = vec![directory.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current).map_err(|error| InstallError::Io {
            path: current.display().to_string(),
            reason: error.to_string(),
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| InstallError::Io {
                path: current.display().to_string(),
                reason: error.to_string(),
            })?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let relative = path.strip_prefix(root).map_err(|_| InstallError::Io {
                path: path.display().to_string(),
                reason: "it is not under the directory being checked".to_owned(),
            })?;
            let name = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<String>>()
                .join("/");
            found.push(name);
        }
    }
    Ok(found)
}

/// The SHA-256 of a file, as lowercase hex.
fn digest_of(path: &Path) -> Result<String, InstallError> {
    use sha2::{Digest as _, Sha256};
    let bytes = std::fs::read(path).map_err(|error| InstallError::Io {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = core::fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}"));
    }
    Ok(hex)
}

/// Writes the record of an install into the installed directory.
fn write_record(directory: &Path, installed: &Installed) -> Result<(), InstallError> {
    let path = directory.join(INSTALL_RECORD_NAME);
    let encoded = serde_json::to_vec_pretty(installed).map_err(|error| InstallError::Io {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    std::fs::write(&path, encoded).map_err(|error| InstallError::Io {
        path: path.display().to_string(),
        reason: error.to_string(),
    })
}

/// Reads the record of an install.
fn read_record(directory: &Path) -> Result<Installed, InstallError> {
    let path = directory.join(INSTALL_RECORD_NAME);
    let encoded = std::fs::read(&path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => InstallError::NotInstalled {
            name: directory
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        },
        _ => InstallError::Io {
            path: path.display().to_string(),
            reason: error.to_string(),
        },
    })?;
    serde_json::from_slice(&encoded).map_err(|error| InstallError::NotABundle {
        reason: format!("{} is not readable as one: {error}", path.display()),
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
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The SHA-256 of a file, as lowercase hex.
    ///
    /// Written here rather than shared with the installer because a test that computed a digest with
    /// the code under test would be checking that code against itself.
    fn test_digest(path: &Path) -> String {
        use sha2::{Digest as _, Sha256};
        let bytes = std::fs::read(path).expect("the file is readable");
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let mut hex = String::new();
        for byte in hasher.finalize() {
            let _ = core::fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}"));
        }
        hex
    }

    /// The file records for a bundle's payload, in the shape the bundler writes them.
    fn test_records(root: &Path, paths: &[&str]) -> serde_json::Value {
        let mut files = Vec::new();
        for path in paths {
            let full = root.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
            let bytes = std::fs::metadata(&full).expect("the file is there").len();
            files.push(serde_json::json!({
                "path": path,
                "bytes": bytes,
                "sha256": test_digest(&full),
            }));
        }
        serde_json::Value::Array(files)
    }

    /// A directory this test owns and removes.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "ncui-install-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("the scratch directory is creatable");
            Self { path }
        }

        /// A bundle, with the shape the bundler writes.
        ///
        /// Written here rather than produced by the bundler because the bundler is a build-time
        /// Python tool: what this test needs is the *shape* it writes, which the bundler's own
        /// tests assert against its output.
        fn bundle(&self, name: &str, identity: Option<&str>, scheme: Option<&str>) -> PathBuf {
            self.bundle_version(name, "1.0.0", identity, scheme)
        }

        /// A bundle of one version, so an update has something to replace it with.
        fn bundle_version(
            &self,
            name: &str,
            version: &str,
            identity: Option<&str>,
            scheme: Option<&str>,
        ) -> PathBuf {
            let bundle = self.path.join(format!("{name}-{version}-bundle"));
            std::fs::create_dir_all(bundle.join("public")).expect("the bundle is creatable");
            std::fs::write(
                bundle.join("ncui.app.json"),
                br#"{"name":"counter","version":"1.0.0","entry":"index.html","assets":"public"}"#,
            )
            .expect("the application's manifest is writable");
            std::fs::write(
                bundle.join("public").join("index.html"),
                format!("<!doctype html><!-- {version} -->"),
            )
            .expect("the page is writable");
            // A stand-in for the runtime binary: this test is about where files land and what is
            // registered, and which file the runtime is belongs to the bundler.
            std::fs::write(bundle.join("ncui.exe"), b"not really a program")
                .expect("the runtime slot is writable");
            // The records a bundle carries, written after the files they describe because that is
            // the order the bundler uses: it digests what it wrote.
            let manifest = serde_json::json!({
                "name": name,
                "version": version,
                "runtime": "ncui.exe",
                "identity": identity,
                "scheme": scheme,
                "files": test_records(&bundle, &["ncui.app.json", "public/index.html", "ncui.exe"]),
            });
            std::fs::write(
                bundle.join(BUNDLE_MANIFEST_NAME),
                serde_json::to_vec_pretty(&manifest).expect("a manifest encodes"),
            )
            .expect("the manifest is writable");
            bundle
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn an_update_replaces_the_files_and_leaves_nothing_behind() {
        let scratch = Scratch::new("update");
        let root = scratch.path.join("applications");
        let installer = Installer::new(&root).expect("the root is creatable");
        let first = scratch.bundle_version("counter", "1.0.0", None, None);
        installer
            .install(&first)
            .expect("the first version installs");

        let second = scratch.bundle_version("counter", "2.0.0", None, None);
        let updated = installer.update(&second).expect("the update applies");
        assert_eq!(updated.previous.version, "1.0.0");
        assert_eq!(updated.installed.version, "2.0.0");

        // The files are the new version's.
        let page = std::fs::read(Path::new(&updated.installed.directory).join("public/index.html"))
            .expect("the page is installed");
        assert!(
            String::from_utf8_lossy(&page).contains("2.0.0"),
            "the installed page must be the new one: {}",
            String::from_utf8_lossy(&page)
        );

        // The staging directories are gone: a copy left in the applications directory would be a
        // second copy of the application, under a name nobody chose.
        let leftovers: Vec<String> = std::fs::read_dir(&root)
            .expect("the root is readable")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| is_staging_name(name))
            .collect();
        assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");

        // And the application is listed once, as itself.
        let listed = installer.installed().expect("the root is readable");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "counter");
        assert_eq!(listed[0].version, "2.0.0");
    }

    #[test]
    fn an_update_of_something_that_is_not_installed_is_refused() {
        let scratch = Scratch::new("update-missing");
        let root = scratch.path.join("applications");
        let installer = Installer::new(&root).expect("the root is creatable");
        let bundle = scratch.bundle_version("counter", "1.0.0", None, None);
        let error = installer
            .update(&bundle)
            .expect_err("an update of nothing is refused");
        assert_eq!(
            error,
            InstallError::NotInstalled {
                name: "counter".to_owned()
            }
        );
    }

    #[test]
    fn an_update_that_is_not_a_bundle_leaves_the_application_alone() {
        // The staging copy is written before anything is moved, so a bundle that cannot be read is
        // refused with the installed application exactly as it was.
        let scratch = Scratch::new("update-bad");
        let root = scratch.path.join("applications");
        let installer = Installer::new(&root).expect("the root is creatable");
        let bundle = scratch.bundle_version("counter", "1.0.0", None, None);
        installer
            .install(&bundle)
            .expect("the first version installs");

        let broken = scratch.path.join("counter-2.0.0-bundle");
        std::fs::create_dir_all(&broken).expect("the directory is creatable");
        let error = installer
            .update(&broken)
            .expect_err("a directory that is not a bundle is refused");
        assert!(matches!(error, InstallError::NotABundle { .. }), "{error}");

        let listed = installer.installed().expect("the root is readable");
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].version, "1.0.0",
            "the old version is still installed"
        );
        assert!(
            Path::new(&listed[0].directory)
                .join("public/index.html")
                .is_file(),
            "and its files are still there"
        );
        let leftovers: Vec<String> = std::fs::read_dir(&root)
            .expect("the root is readable")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| is_staging_name(name))
            .collect();
        assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
    }

    #[test]
    fn an_update_does_not_touch_a_different_application() {
        let scratch = Scratch::new("update-other");
        let root = scratch.path.join("applications");
        let installer = Installer::new(&root).expect("the root is creatable");
        installer
            .install(&scratch.bundle_version("counter", "1.0.0", None, None))
            .expect("counter installs");

        let other = scratch.bundle_version("other", "1.0.0", None, None);
        let error = installer
            .update(&other)
            .expect_err("a bundle of another application is refused");
        assert_eq!(
            error,
            InstallError::NotInstalled {
                name: "other".to_owned()
            }
        );
        let listed = installer.installed().expect("the root is readable");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "counter");
    }

    #[test]
    fn a_staging_directory_left_by_a_failed_update_is_not_an_application() {
        // A run that died between the two renames leaves a directory holding a record. Listing it
        // would offer the user an application whose name is a stamp.
        let scratch = Scratch::new("update-leftover");
        let root = scratch.path.join("applications");
        let installer = Installer::new(&root).expect("the root is creatable");
        installer
            .install(&scratch.bundle_version("counter", "1.0.0", None, None))
            .expect("counter installs");

        let leftover = root.join("counter.replaced-999-1");
        std::fs::create_dir_all(&leftover).expect("the leftover is creatable");
        std::fs::write(
            leftover.join(INSTALL_RECORD_NAME),
            br#"{"name":"counter","version":"0.9.0","directory":"x","program":"y"}"#,
        )
        .expect("the record is writable");

        let listed = installer.installed().expect("the root is readable");
        assert_eq!(
            listed.len(),
            1,
            "the leftover must not be listed: {listed:?}"
        );
        assert_eq!(listed[0].name, "counter");
        assert_eq!(listed[0].version, "1.0.0");
    }

    #[test]
    fn an_install_puts_the_files_where_it_says_and_records_what_it_did() {
        let scratch = Scratch::new("install");
        let bundle = scratch.bundle("counter", Some("com.ncui.test-install"), None);
        let root = scratch.path.join("applications");
        let installer = Installer::new(&root).expect("the root is creatable");

        let installed = installer.install(&bundle).expect("the bundle installs");
        assert_eq!(installed.name, "counter");
        assert!(
            Path::new(&installed.directory)
                .join("ncui.app.json")
                .is_file()
        );
        assert!(
            Path::new(&installed.directory)
                .join("public/index.html")
                .is_file()
        );
        assert!(
            Path::new(&installed.directory)
                .join(INSTALL_RECORD_NAME)
                .is_file()
        );
        assert!(
            installed.program.starts_with(&installed.directory),
            "the registered program must be the installed one: {}",
            installed.program
        );

        // The listing reads back what was installed, from the record rather than from the bundle.
        let listed = installer.installed().expect("the root is readable");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], installed);

        let removed = installer.uninstall("counter").expect("it uninstalls");
        assert_eq!(removed, installed);
        assert!(!Path::new(&installed.directory).exists());
        assert!(installer.installed().expect("readable").is_empty());
    }

    #[test]
    fn a_bundle_whose_file_is_not_what_it_says_is_refused() {
        // The defect this closes: an install installed whatever bytes were on disk. The bundle says
        // what its files are, and a file that is not that is a file nobody has a claim about.
        let scratch = Scratch::new("damaged");
        let root = scratch.path.join("applications");
        let installer = Installer::new(&root).expect("the root is creatable");
        let bundle = scratch.bundle_version("counter", "1.0.0", None, None);
        std::fs::write(
            bundle.join("public").join("index.html"),
            "<h1>tampered</h1>",
        )
        .expect("the page is writable");

        let error = installer
            .install(&bundle)
            .expect_err("a file that is not what the bundle says is refused");
        assert!(matches!(error, InstallError::Damaged { .. }), "got {error}");
        assert!(error.to_string().contains("public/index.html"), "{error}");
        // Nothing was installed: a half-installed application is worse than none.
        assert!(installer.installed().expect("readable").is_empty());
        assert!(!root.join("counter").exists());
    }

    #[test]
    fn a_bundle_holding_a_file_it_does_not_name_is_refused() {
        // A file the bundle does not name was never checked, so it is not installed either.
        let scratch = Scratch::new("unnamed");
        let root = scratch.path.join("applications");
        let installer = Installer::new(&root).expect("the root is creatable");
        let bundle = scratch.bundle_version("counter", "1.0.0", None, None);
        std::fs::write(bundle.join("extra.txt"), b"nobody checked this").expect("writable");

        let error = installer
            .install(&bundle)
            .expect_err("a file the bundle does not name is refused");
        assert!(matches!(error, InstallError::Damaged { .. }), "got {error}");
        assert!(error.to_string().contains("extra.txt"), "{error}");
        assert!(installer.installed().expect("readable").is_empty());
    }

    #[test]
    fn an_update_from_a_damaged_bundle_leaves_the_installed_version_alone() {
        // The staged copy is checked before anything is moved, so a bundle that is not what it says
        // it is cannot replace a version that is.
        let scratch = Scratch::new("damaged-update");
        let root = scratch.path.join("applications");
        let installer = Installer::new(&root).expect("the root is creatable");
        installer
            .install(&scratch.bundle_version("counter", "1.0.0", None, None))
            .expect("the first version installs");

        let second = scratch.bundle_version("counter", "2.0.0", None, None);
        std::fs::write(second.join("ncui.exe"), b"a different runtime").expect("writable");
        let error = installer
            .update(&second)
            .expect_err("a damaged bundle is refused");
        assert!(matches!(error, InstallError::Damaged { .. }), "got {error}");

        let listed = installer.installed().expect("the root is readable");
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].version, "1.0.0",
            "the version that was installed is still installed"
        );
        let leftovers: Vec<String> = std::fs::read_dir(&root)
            .expect("the root is readable")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| is_staging_name(name))
            .collect();
        assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
    }

    #[test]
    fn an_install_refuses_what_it_cannot_install() {
        let scratch = Scratch::new("refusals");
        let root = scratch.path.join("applications");
        let installer = Installer::new(&root).expect("the root is creatable");

        // A directory that is not a bundle.
        let not_a_bundle = scratch.path.join("empty");
        std::fs::create_dir_all(&not_a_bundle).expect("creatable");
        assert_eq!(
            installer
                .install(&not_a_bundle)
                .expect_err("not a bundle")
                .code(),
            "install_not_a_bundle"
        );

        // A name that could not be a directory name: refused before anything is copied.
        let hostile = scratch.path.join("hostile-bundle");
        std::fs::create_dir_all(&hostile).expect("creatable");
        std::fs::write(
            hostile.join(BUNDLE_MANIFEST_NAME),
            br#"{"name":"../escape","version":"1.0.0","runtime":"ncui.exe","files":[]}"#,
        )
        .expect("writable");
        assert_eq!(
            installer
                .install(&hostile)
                .expect_err("unusable name")
                .code(),
            "install_name_unusable"
        );
        assert!(
            !scratch.path.join("escape").exists(),
            "a name that is a path must not have been followed"
        );

        // Nothing installed under a name is reported as such.
        assert_eq!(
            installer
                .uninstall("counter")
                .expect_err("not installed")
                .code(),
            "install_not_installed"
        );
        assert_eq!(
            installer
                .uninstall("../escape")
                .expect_err("unusable")
                .code(),
            "install_name_unusable"
        );
    }

    #[test]
    fn installing_over_an_existing_install_is_refused() {
        let scratch = Scratch::new("twice");
        let bundle = scratch.bundle("counter", None, None);
        let installer =
            Installer::new(&scratch.path.join("applications")).expect("the root is creatable");
        installer.install(&bundle).expect("the first install works");
        let error = installer
            .install(&bundle)
            .expect_err("installing over it is refused");
        assert_eq!(error.code(), "install_already_installed");
        assert!(error.to_string().contains("uninstall it first"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn an_install_registers_the_identity_and_the_scheme_and_removes_both() {
        // The real path on this platform: the user's own hive for the identity, and the
        // platform's own tool for the scheme, both naming the installed program.
        let scratch = Scratch::new("registers");
        let identity = "com.ncui.test-installed";
        let scheme = "ncui-test-installed";
        identity::unregister(identity).expect("nothing to remove");
        protocol::unregister(scheme).expect("nothing to remove");

        let bundle = scratch.bundle("installedapp", Some(identity), Some(scheme));
        let installer =
            Installer::new(&scratch.path.join("applications")).expect("the root is creatable");
        let installed = installer.install(&bundle).expect("the bundle installs");

        assert_eq!(
            identity::registered(identity).expect("readable").as_deref(),
            Some("installedapp"),
            "the identity must be registered under the application's name"
        );
        let registered = protocol::registered(scheme)
            .expect("readable")
            .expect("the scheme must be registered");
        assert!(
            registered.contains(&installed.program),
            "the scheme must run the installed program: {registered}"
        );
        assert!(
            registered.contains(&installed.directory),
            "and tell the runtime which application the link belongs to: {registered}"
        );

        installer.uninstall("installedapp").expect("it uninstalls");
        assert_eq!(
            identity::registered(identity).expect("readable"),
            None,
            "the identity is removed with the application"
        );
        assert_eq!(
            protocol::registered(scheme).expect("readable"),
            None,
            "and so is the scheme"
        );
    }

    #[test]
    fn the_default_root_is_this_user_s_own_directory() {
        // Per user, and not a scratch directory: an installed application has to survive a
        // reboot, or installing it was pointless.
        let root = default_root();
        assert!(root.is_absolute(), "{}", root.display());
        assert!(
            !root.starts_with(std::env::temp_dir()) || std::env::var("LOCALAPPDATA").is_err(),
            "the default root must not be the system's scratch directory: {}",
            root.display()
        );
        assert!(
            root.ends_with("Applications") || root.ends_with("applications"),
            "{}",
            root.display()
        );
    }

    #[test]
    fn a_directory_name_is_narrow_on_purpose() {
        for name in ["counter", "my-app", "app_2"] {
            assert!(is_directory_name(name), "{name}");
        }
        for name in [
            "",
            "../escape",
            "a/b",
            "a\\b",
            "a b",
            "a.b",
            &"a".repeat(65),
        ] {
            assert!(!is_directory_name(name), "{name}");
        }
    }
}
