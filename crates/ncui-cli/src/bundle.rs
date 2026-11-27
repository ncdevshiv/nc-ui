//! Building a shippable application directory.
//!
//! An application that cannot be handed to somebody else is not an application, and this is the
//! step between "this runs on my machine" and "this is a program". `ncui build <directory>` writes
//! a portable directory: the runtime binary, the application's manifest and assets, a manifest of
//! what was produced with a digest per file, and — where the platform needs one — the script that
//! registers the application's identity and its URL scheme.
//!
//! Three decisions are worth stating, because they are the ones a reader would otherwise have to
//! infer.
//!
//! * **Nothing is written outside the output directory.** Registering an application identity or a
//!   URL scheme touches the user's registry or desktop entries, and that is an *install* step
//!   rather than a *build* step, so the bundle carries the scripts and the user runs them. A build
//!   that quietly rewrote `HKCU` would be a build whose output depended on the machine that ran it.
//! * **The output is deterministic.** No timestamp, no machine name, no ordering that depends on the
//!   filesystem. Two bundles built from the same tree are byte-identical, which is what makes the
//!   digest manifest worth reading: a difference means the tree differed.
//! * **The manifest is read by the runtime's own reader.** A bundler with its own idea of what a
//!   manifest is would be a second answer to a question the runtime already answers, and the two
//!   would drift.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use ncui_app::AppManifest;
use sha2::{Digest, Sha256};

/// Which runtime binary a build ships, chosen by `ncui build --profile`.
///
/// The GPU-island renderer is the single heaviest subsystem in the runtime — measured at
/// 6.4 MiB of the 11.3 MiB release build (`comparison/README.md`) — and an application that
/// draws no islands pays for it in every byte it ships. Both profiles ship the *same* window
/// host; `Minimal` links out only the graphics stack, which is why the difference is a cargo
/// feature and not a code path. This is A3's constraint honoured rather than worked around:
/// the capability is **optional at link time**, never removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// The window host **and** the GPU-island renderer. The full runtime, for applications
    /// that draw islands.
    #[default]
    Standard,
    /// The window host **without** the GPU-island renderer. For applications that open a
    /// window and serve a page but draw no GPU islands: it links out `wgpu` and the text
    /// shaper, landing far under the full build's size. A page that asked for an island on
    /// this profile gets a clear "no island in this build" rather than a silent failure,
    /// because the island route is `#[cfg(feature = "gpu-islands")]`-gated.
    Minimal,
}

impl Profile {
    /// Parses a `--profile` value.
    ///
    /// # Errors
    ///
    /// Returns the accepted names when the value is not one of them, so a typo reports what
    /// would have worked rather than a bare "unknown".
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "standard" => Ok(Self::Standard),
            "minimal" => Ok(Self::Minimal),
            other => Err(format!(
                "unknown profile {other:?}: expected \"standard\" or \"minimal\""
            )),
        }
    }

    /// The `cargo` feature this profile compiles `ncui-cli` with.
    ///
    /// `Standard` is `wry-host-gpu` — the window host plus the GPU-island renderer, the full
    /// build. `Minimal` is `wry-host` — the window host alone, which omits the renderer
    /// because `wry-host` no longer implies `gpu-islands`.
    #[must_use]
    pub const fn cargo_feature(self) -> &'static str {
        match self {
            Self::Standard => "wry-host-gpu",
            Self::Minimal => "wry-host",
        }
    }

    /// Whether this profile compiles the GPU-island renderer in.
    #[must_use]
    pub const fn includes_gpu_islands(self) -> bool {
        matches!(self, Self::Standard)
    }

    /// Builds the runtime binary for this profile with `cargo build --release`, returning the
    /// path to the compiled binary.
    ///
    /// A `--profile minimal` that shipped whatever binary happened to be in `target/release`
    /// would be a lie: the size is the whole point, and a stale full build in that directory
    /// would bundle the renderer the profile promised to omit. So the profile compiles its own
    /// feature set rather than reusing an artifact it does not control.
    ///
    /// # Errors
    ///
    /// Returns a message when `cargo` is unavailable or the build fails, so a caller sees why
    /// rather than a bare non-zero exit.
    pub fn build_runtime(self, root: &Path) -> Result<PathBuf, String> {
        // The workspace default carries no window host (so the crate builds headless), so
        // both profiles name their feature explicitly rather than relying on defaults.
        let status = Command::new("cargo")
            .current_dir(root)
            .args([
                "build",
                "--release",
                "-p",
                "ncui-cli",
                "--no-default-features",
            ])
            .arg("--features")
            .arg(self.cargo_feature())
            .status()
            .map_err(|error| format!("could not run cargo: {error}"))?;
        if !status.success() {
            return Err(format!(
                "cargo build --release failed for the {self} profile"
            ));
        }
        Ok(default_binary(root))
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Standard => "standard",
            Self::Minimal => "minimal",
        };
        f.write_str(name)
    }
}

/// The manifest an application directory carries.
const MANIFEST_NAME: &str = "ncui.app.json";

/// The manifest a bundle carries, describing what was produced.
const BUNDLE_MANIFEST_NAME: &str = "bundle.json";

/// The script that registers the application's identity on Windows.
const IDENTITY_SCRIPT_NAME: &str = "register-identity.ps1";

/// The script that registers the application as the handler for its links on Windows.
const PROTOCOL_SCRIPT_NAME: &str = "register-protocol.ps1";

/// The prefix an identity is derived under when the manifest names none.
///
/// Stable across builds, because an identity that changed between releases would leave the previous
/// one registered and the notifications attributed to a name the user cannot find.
const IDENTITY_PREFIX: &str = "com.ncui";

/// The characters a generated script cannot carry: each would end the quoting it sits inside.
///
/// Written as a raw string rather than a tuple of escapes, because a list of escape sequences is
/// exactly the kind of value a reader cannot check by looking at it.
const SCRIPT_SPECIAL: &str = "\"'`$;";

/// One file the bundle holds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BundledFile {
    /// The path within the bundle, with forward slashes.
    pub path: String,
    /// How many bytes it is.
    pub bytes: u64,
    /// Its SHA-256, as hex.
    pub sha256: String,
}

/// What a bundle is, as its own manifest describes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Bundle {
    /// The application's name.
    pub name: String,
    /// Its version.
    pub version: String,
    /// The identity it is registered under.
    pub identity: String,
    /// The URL scheme it handles, when it handles one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// The file extensions this application is associated with, normalised.
    #[serde(default)]
    pub associations: Vec<String>,
    /// The page the application opens.
    pub entry: String,
    /// The runtime binary's name within the bundle.
    pub runtime: String,
    /// Every file the bundle holds, sorted by path.
    pub files: Vec<BundledFile>,
}

/// Why a bundle could not be built, or is not what it says it is.
#[derive(Debug)]
pub enum BundleError {
    /// The application directory is not one.
    Application {
        /// What was wrong.
        reason: String,
    },
    /// The runtime binary is not there.
    MissingBinary {
        /// The path that was looked for.
        path: PathBuf,
    },
    /// A file could not be read or written.
    Io {
        /// The path.
        path: PathBuf,
        /// What the system said.
        reason: String,
    },
    /// A value that reaches a generated script carries a character that would end its quoting.
    Unscriptable {
        /// What the value is, for the message.
        what: &'static str,
    },
    /// The bundle in the output directory is not what its own manifest describes.
    NotIntact {
        /// Why.
        reason: String,
    },
    /// The bundle in the output directory differs from a fresh build of the application.
    Stale {
        /// The output directory.
        out: PathBuf,
    },
}

impl fmt::Display for BundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Application { reason } => {
                write!(f, "the application cannot be bundled: {reason}")
            }
            Self::MissingBinary { path } => write!(
                f,
                "{} does not exist: build the runtime first, or name one with --binary",
                path.display()
            ),
            Self::Io { path, reason } => write!(f, "{}: {reason}", path.display()),
            Self::Unscriptable { what } => write!(
                f,
                "the application's {what} contains a character a generated script cannot carry"
            ),
            Self::NotIntact { reason } => write!(f, "the bundle is not intact: {reason}"),
            Self::Stale { out } => write!(
                f,
                "the bundle in {} differs from a fresh build of the application",
                out.display()
            ),
        }
    }
}

impl std::error::Error for BundleError {}

/// Builds the bundle for `app` from `binary` into `out`, returning its manifest.
///
/// # Errors
///
/// Returns [`BundleError::Application`] when the directory is not an application,
/// [`BundleError::MissingBinary`] when the runtime is not there, [`BundleError::Unscriptable`] when
/// a value the bundle writes into a script carries a character that would end its quoting, and
/// [`BundleError::Io`] when a file cannot be read or written.
pub fn build(app: &Path, binary: &Path, out: &Path) -> Result<Bundle, BundleError> {
    let manifest = AppManifest::load(app).map_err(|error| BundleError::Application {
        reason: error.to_string(),
    })?;
    check_bundle_inputs(&manifest, binary)?;
    let identity = identity_for(&manifest);
    ncui_native::identity::check(&identity)
        .map_err(|_| BundleError::Unscriptable { what: "identity" })?;
    let display_name = manifest.name.clone();
    if carries_a_script_character(&display_name) {
        return Err(BundleError::Unscriptable { what: "name" });
    }

    // The output directory is replaced rather than merged: a bundle that kept a file from a
    // previous build would be a bundle describing a tree that no longer exists.
    if out.exists() {
        std::fs::remove_dir_all(out).map_err(|error| BundleError::Io {
            path: out.to_path_buf(),
            reason: error.to_string(),
        })?;
    }
    std::fs::create_dir_all(out).map_err(|error| BundleError::Io {
        path: out.to_path_buf(),
        reason: error.to_string(),
    })?;

    let binary_name = binary_name(binary);
    copy_file(binary, &out.join(&binary_name))?;
    copy_file(&app.join(MANIFEST_NAME), &out.join(MANIFEST_NAME))?;
    let mut written: BTreeSet<String> = BTreeSet::new();
    written.insert(binary_name.clone());
    written.insert(MANIFEST_NAME.to_owned());
    copy_assets(app, &manifest.assets, out, &mut written)?;
    copy_loose_files(app, out, &mut written)?;

    write_registrations(
        out,
        &manifest,
        &identity,
        &display_name,
        &binary_name,
        &mut written,
    )?;

    let files = describe_files(out, &written)?;
    let bundle = Bundle {
        name: display_name,
        version: manifest.version.clone(),
        identity,
        scheme: manifest.scheme.clone().filter(|scheme| !scheme.is_empty()),
        associations: normalized_associations(&manifest.associations)?,
        entry: manifest.entry.to_string(),
        runtime: binary_name,
        files,
    };
    let encoded = serde_json::to_vec_pretty(&bundle).map_err(|error| BundleError::Io {
        path: out.join(BUNDLE_MANIFEST_NAME),
        reason: error.to_string(),
    })?;
    write_bytes(&out.join(BUNDLE_MANIFEST_NAME), &encoded)?;
    Ok(bundle)
}

/// Checks that the application and the runtime are what a bundle can be built from.
fn check_bundle_inputs(manifest: &AppManifest, binary: &Path) -> Result<(), BundleError> {
    if !binary.is_file() {
        return Err(BundleError::MissingBinary {
            path: binary.to_path_buf(),
        });
    }
    if carries_a_script_character(&manifest.name) {
        return Err(BundleError::Unscriptable { what: "name" });
    }
    Ok(())
}

/// The name the bundled runtime keeps, so a user can see which runtime they run.
fn binary_name(binary: &Path) -> String {
    binary.file_name().map_or_else(
        || "ncui".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Copies the assets the application serves to a page.
fn copy_assets(
    app: &Path,
    assets: &str,
    out: &Path,
    written: &mut BTreeSet<String>,
) -> Result<(), BundleError> {
    let source = app.join(assets);
    if source.is_dir() {
        copy_tree(&source, &out.join(assets), assets, written)?;
    }
    Ok(())
}

/// Copies the application directory's other files: notes, fixtures, a licence. They are
/// part of the application whether or not they are served to a page.
fn copy_loose_files(
    app: &Path,
    out: &Path,
    written: &mut BTreeSet<String>,
) -> Result<(), BundleError> {
    for path in read_dir(app)? {
        if path.is_file() && path.file_name().is_some_and(|name| name != MANIFEST_NAME) {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            copy_file(&path, &out.join(&name))?;
            written.insert(name);
        }
    }
    Ok(())
}

/// Describes the files a bundle holds: their size and their digest.
fn describe_files(out: &Path, written: &BTreeSet<String>) -> Result<Vec<BundledFile>, BundleError> {
    let mut files = Vec::with_capacity(written.len());
    for path in written {
        let full = out.join(path);
        let bytes = std::fs::metadata(&full).map_err(|error| BundleError::Io {
            path: full.clone(),
            reason: error.to_string(),
        })?;
        files.push(BundledFile {
            path: path.clone(),
            bytes: bytes.len(),
            sha256: digest(&full)?,
        });
    }
    Ok(files)
}

/// The bundle's associations, normalised once, in the manifest's order.
fn normalized_associations(associations: &[String]) -> Result<Vec<String>, BundleError> {
    associations
        .iter()
        .map(|extension| {
            ncui_native::normalize_extension(extension).map_err(|error| BundleError::Application {
                reason: error.to_string(),
            })
        })
        .collect()
}

/// Writes the scripts that register what this platform needs registered.
///
/// Both are *install* steps rather than build steps, which is why they are scripts the user runs
/// rather than keys the build writes: a build that rewrote the user's registry would be a build
/// whose output depended on the machine that ran it.
fn write_registrations(
    out: &Path,
    manifest: &AppManifest,
    identity: &str,
    display_name: &str,
    binary_name: &str,
    written: &mut BTreeSet<String>,
) -> Result<(), BundleError> {
    if cfg!(windows) {
        write_script(
            out,
            IDENTITY_SCRIPT_NAME,
            &identity_script(identity, display_name)?,
            written,
        )?;
        write_script(
            out,
            "unregister-identity.ps1",
            &unregister_identity_script(identity)?,
            written,
        )?;
    }

    // An application that declares no scheme gets no registration: a bundle that claimed links its
    // application never asked for would be claiming them from whatever handles them now.
    let Some(scheme) = manifest
        .scheme
        .as_deref()
        .filter(|scheme| !scheme.is_empty())
    else {
        return Ok(());
    };
    if cfg!(windows) {
        write_script(
            out,
            PROTOCOL_SCRIPT_NAME,
            &protocol_script(scheme, binary_name)?,
            written,
        )?;
        write_script(
            out,
            "unregister-protocol.ps1",
            &unregister_protocol_script(scheme)?,
            written,
        )?;
    } else {
        write_script(
            out,
            "install-protocol.sh",
            &linux_protocol_entry(scheme, binary_name)?,
            written,
        )?;
    }
    Ok(())
}

/// Why the bundle in `out` is not what its own manifest describes, or `Ok(())` when it is.
///
/// A bundle's manifest describes the files that were written, so it is the thing to check the files
/// against. Comparing only the manifests of two builds misses the case this exists for: a file
/// inside the bundle changed after it was built — edited, truncated, replaced — while the manifest
/// that describes it still holds the digest of the file that was supposed to be there.
///
/// # Errors
///
/// Returns [`BundleError::NotIntact`] when the bundle is missing, unreadable, or holds a file that
/// is not the one its manifest describes.
pub fn verify(out: &Path) -> Result<(), BundleError> {
    let path = out.join(BUNDLE_MANIFEST_NAME);
    let encoded = std::fs::read(&path).map_err(|error| BundleError::NotIntact {
        reason: format!("{}: {error}", path.display()),
    })?;
    let bundle: Bundle =
        serde_json::from_slice(&encoded).map_err(|error| BundleError::NotIntact {
            reason: format!("{} is not a bundle manifest: {error}", path.display()),
        })?;
    for entry in &bundle.files {
        let full = out.join(&entry.path);
        let metadata = std::fs::metadata(&full).map_err(|_| BundleError::NotIntact {
            reason: format!("{} is described but missing", entry.path),
        })?;
        if metadata.len() != entry.bytes || digest(&full)? != entry.sha256 {
            return Err(BundleError::NotIntact {
                reason: format!("{} is not what the bundle's manifest describes", entry.path),
            });
        }
    }
    Ok(())
}

/// Builds into a private directory and compares the result with the bundle in `out`.
///
/// # Errors
///
/// Returns [`BundleError::NotIntact`] when the existing bundle is not what it says it is, and
/// [`BundleError::Stale`] when it differs from a fresh build of the application.
pub fn check(app: &Path, binary: &Path, out: &Path) -> Result<Bundle, BundleError> {
    let fresh = scratch_directory()?;
    let built = match build(app, binary, &fresh) {
        Ok(bundle) => bundle,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&fresh);
            return Err(error);
        }
    };
    let _ = std::fs::remove_dir_all(&fresh);

    if !out.join(BUNDLE_MANIFEST_NAME).is_file() {
        return Err(BundleError::NotIntact {
            reason: format!("{} has no bundle; run `ncui build` first", out.display()),
        });
    }
    verify(out)?;
    let encoded =
        std::fs::read(out.join(BUNDLE_MANIFEST_NAME)).map_err(|error| BundleError::NotIntact {
            reason: format!("{}: {error}", out.join(BUNDLE_MANIFEST_NAME).display()),
        })?;
    let existing: Bundle =
        serde_json::from_slice(&encoded).map_err(|error| BundleError::NotIntact {
            reason: format!("the bundle manifest is unreadable: {error}"),
        })?;
    if existing != built {
        return Err(BundleError::Stale {
            out: out.to_path_buf(),
        });
    }
    Ok(built)
}

/// Where `cargo build --release` puts the runtime, for a caller that names no binary.
#[must_use]
pub fn default_binary(root: &Path) -> PathBuf {
    let name = if cfg!(windows) { "ncui.exe" } else { "ncui" };
    root.join("target").join("release").join(name)
}

/// The identity an application is registered under.
///
/// A manifest may name one; otherwise it is derived from the application's name. The derived form
/// keeps only what an identity may contain, so an application called `My App!` becomes
/// `com.ncui.my-app` rather than an identity Windows would refuse.
#[must_use]
pub fn identity_for(manifest: &AppManifest) -> String {
    if let Some(named) = manifest
        .identity
        .as_deref()
        .filter(|named| !named.is_empty())
    {
        return named.to_owned();
    }
    let mut slug = String::new();
    for character in manifest.name.to_lowercase().chars() {
        if character.is_alphanumeric() {
            slug.push(character);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let trimmed = slug.trim_matches('-');
    format!(
        "{IDENTITY_PREFIX}.{}",
        if trimmed.is_empty() { "app" } else { trimmed }
    )
}

/// The SHA-256 of a file, as hex.
fn digest(path: &Path) -> Result<String, BundleError> {
    let bytes = std::fs::read(path).map_err(|error| BundleError::Io {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Whether `value` holds a character that would end the quoting it sits inside.
fn carries_a_script_character(value: &str) -> bool {
    value
        .chars()
        .any(|character| SCRIPT_SPECIAL.contains(character) || character.is_control())
}

/// Copies `source` to `destination`, creating the parent directory.
fn copy_file(source: &Path, destination: &Path) -> Result<(), BundleError> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| BundleError::Io {
            path: parent.to_path_buf(),
            reason: error.to_string(),
        })?;
    }
    std::fs::copy(source, destination).map_err(|error| BundleError::Io {
        path: source.to_path_buf(),
        reason: error.to_string(),
    })?;
    Ok(())
}

/// Writes `bytes` to `path`.
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), BundleError> {
    std::fs::write(path, bytes).map_err(|error| BundleError::Io {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })
}

/// Writes a generated script, remembering it was written.
fn write_script(
    out: &Path,
    name: &str,
    body: &str,
    written: &mut BTreeSet<String>,
) -> Result<(), BundleError> {
    write_bytes(&out.join(name), body.as_bytes())?;
    written.insert(name.to_owned());
    Ok(())
}

/// The entries of a directory, sorted.
///
/// Sorted because the bundle's contents must not depend on the order the filesystem hands them
/// over: two builds of one tree are byte-identical only if the walk is.
fn read_dir(directory: &Path) -> Result<Vec<PathBuf>, BundleError> {
    let entries = std::fs::read_dir(directory).map_err(|error| BundleError::Io {
        path: directory.to_path_buf(),
        reason: error.to_string(),
    })?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| BundleError::Io {
            path: directory.to_path_buf(),
            reason: error.to_string(),
        })?;
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

/// Copies a directory into the bundle, remembering the relative paths written.
fn copy_tree(
    source: &Path,
    destination: &Path,
    prefix: &str,
    written: &mut BTreeSet<String>,
) -> Result<(), BundleError> {
    for path in read_dir(source)? {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            copy_tree(&path, &destination.join(&name), &relative, written)?;
        } else {
            copy_file(&path, &destination.join(&name))?;
            written.insert(relative);
        }
    }
    Ok(())
}

/// A private directory under the system's own scratch area, for a build nobody asked to keep.
fn scratch_directory() -> Result<PathBuf, BundleError> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "ncui-build-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).map_err(|error| BundleError::Io {
        path: path.clone(),
        reason: error.to_string(),
    })?;
    Ok(path)
}

/// The PowerShell script that registers an application's identity.
///
/// It writes one key under `HKCU`, which needs no administrator, and prints what it did so the user
/// can see the effect rather than trust it.
fn identity_script(identity: &str, display_name: &str) -> Result<String, BundleError> {
    for value in [identity, display_name] {
        if carries_a_script_character(value) {
            return Err(BundleError::Unscriptable { what: "identity" });
        }
    }
    Ok(format!(
        "# Registers this application's identity with Windows, so its notifications arrive under\n\
         # its own name instead of the shell's. Run it once per user; it needs no administrator,\n\
         # and `unregister-identity.ps1` removes it.\n\
         $ErrorActionPreference = 'Stop'\n\
         $identity = '{identity}'\n\
         $name = '{display_name}'\n\
         $key = \"HKCU:\\Software\\Classes\\AppUserModelId\\$identity\"\n\
         New-Item -Path $key -Force | Out-Null\n\
         New-ItemProperty -Path $key -Name 'DisplayName' -Value $name -PropertyType String -Force | Out-Null\n\
         Write-Output \"registered $identity as $name\"\n"
    ))
}

/// The script that removes what [`identity_script`] wrote.
fn unregister_identity_script(identity: &str) -> Result<String, BundleError> {
    if carries_a_script_character(identity) {
        return Err(BundleError::Unscriptable { what: "identity" });
    }
    Ok(format!(
        "# Removes the identity this application registered. Nothing else is touched.\n\
         $ErrorActionPreference = 'Stop'\n\
         $identity = '{identity}'\n\
         $key = \"HKCU:\\Software\\Classes\\AppUserModelId\\$identity\"\n\
         if (Test-Path $key) {{ Remove-Item -Path $key -Recurse -Force }}\n\
         Write-Output \"unregistered $identity\"\n"
    ))
}

/// The script that registers this application as the handler for its links.
///
/// The paths are resolved when the script runs rather than when the bundle is built: a bundle is a
/// directory somebody can move, and a registration that named the path it was built at would point
/// at a directory that is no longer there. `$here` is where the script itself sits, which is the one
/// location that is true wherever the bundle ends up.
fn protocol_script(scheme: &str, binary_name: &str) -> Result<String, BundleError> {
    for value in [scheme, binary_name] {
        if carries_a_script_character(value) {
            return Err(BundleError::Unscriptable { what: "scheme" });
        }
    }
    Ok(format!(
        "# Registers this application as the handler for its links, so a URL of the form\n\
         # {scheme}://... is handed to it. Run it once per user; it needs no administrator.\n\
         $ErrorActionPreference = 'Stop'\n\
         $here = Split-Path -Parent $MyInvocation.MyCommand.Path\n\
         $scheme = '{scheme}'\n\
         $program = Join-Path $here '{binary_name}'\n\
         $command = \"\\\"$program\\\" run \\\"$here\\\" \\\"%1\\\"\"\n\
         $key = \"HKCU:\\Software\\Classes\\$scheme\"\n\
         New-Item -Path $key -Force | Out-Null\n\
         New-ItemProperty -Path $key -Name 'URL Protocol' -Value '' -PropertyType String -Force | Out-Null\n\
         $open = Join-Path $key 'shell\\open\\command'\n\
         New-Item -Path $open -Force | Out-Null\n\
         (Get-Item -Path $open).SetValue('', $command)\n\
         Write-Output \"registered $scheme to run $program\"\n"
    ))
}

/// The script that removes what [`protocol_script`] wrote.
fn unregister_protocol_script(scheme: &str) -> Result<String, BundleError> {
    if carries_a_script_character(scheme) {
        return Err(BundleError::Unscriptable { what: "scheme" });
    }
    Ok(format!(
        "# Removes the link registration this application made. Nothing else is touched.\n\
         $ErrorActionPreference = 'Stop'\n\
         $scheme = '{scheme}'\n\
         $key = \"HKCU:\\Software\\Classes\\$scheme\"\n\
         if (Test-Path $key) {{ Remove-Item -Path $key -Recurse -Force }}\n\
         Write-Output \"unregistered $scheme\"\n"
    ))
}

/// The installer that makes this bundle a link handler on Linux.
///
/// A desktop entry cannot compute its own location, so the paths are written by this script from
/// wherever it is run: the same shape as the Windows registration, and for the same reason.
fn linux_protocol_entry(scheme: &str, binary_name: &str) -> Result<String, BundleError> {
    for value in [scheme, binary_name] {
        if carries_a_script_character(value) {
            return Err(BundleError::Unscriptable { what: "scheme" });
        }
    }
    Ok(format!(
        "#!/bin/sh\n\
         # Installs this bundle as the handler for its links, resolving the paths of wherever the\n\
         # bundle currently is. Run it once per user.\n\
         set -eu\n\
         here=$(cd \"$(dirname \"$0\")\" && pwd)\n\
         scheme='{scheme}'\n\
         applications=\"$HOME/.local/share/applications\"\n\
         mkdir -p \"$applications\"\n\
         entry=\"$applications/ncui-$scheme.desktop\"\n\
         {{\n\
         \x20 echo '[Desktop Entry]'\n\
         \x20 echo 'Type=Application'\n\
         \x20 echo 'Name=NC-UI link handler'\n\
         \x20 echo \"Exec=\\\"$here/{binary_name}\\\" run \\\"$here\\\" %1\"\n\
         \x20 echo 'NoDisplay=true'\n\
         \x20 echo 'Terminal=false'\n\
         \x20 echo \"MimeType=x-scheme-handler/$scheme;\"\n\
         }} > \"$entry\"\n\
         xdg-mime default \"ncui-$scheme.desktop\" \"x-scheme-handler/$scheme\"\n\
         echo \"registered $scheme to run $here/{binary_name}\"\n"
    ))
}

#[cfg(test)]
mod profile_tests {
    use super::Profile;

    #[test]
    fn profile_parses_the_two_names_it_advertises() {
        assert_eq!(Profile::parse("standard"), Ok(Profile::Standard));
        assert_eq!(Profile::parse("minimal"), Ok(Profile::Minimal));
    }

    #[test]
    fn an_unknown_profile_names_the_accepted_values() {
        let error = Profile::parse("tiny").unwrap_err();
        assert!(
            error.contains("standard") && error.contains("minimal"),
            "the error should name both accepted profiles, got: {error}"
        );
    }

    #[test]
    fn the_default_profile_is_the_standard_full_build() {
        // A plain `ncui build` must keep shipping the renderer, so the default cannot be the
        // size-optimised profile: that would silently change what every existing build carries.
        assert_eq!(Profile::default(), Profile::Standard);
        assert!(Profile::default().includes_gpu_islands());
    }

    #[test]
    fn the_profiles_differ_exactly_in_the_gpu_island_renderer() {
        // The whole point of the profile is the renderer: standard carries it, minimal does
        // not, and both ship the same window host. If these ever agree the flag is a no-op.
        assert!(Profile::Standard.includes_gpu_islands());
        assert!(!Profile::Minimal.includes_gpu_islands());
        assert_ne!(
            Profile::Standard.cargo_feature(),
            Profile::Minimal.cargo_feature(),
            "the two profiles must compile different feature sets, or minimal ships the renderer"
        );
        assert_eq!(Profile::Standard.cargo_feature(), "wry-host-gpu");
        assert_eq!(Profile::Minimal.cargo_feature(), "wry-host");
    }
}
