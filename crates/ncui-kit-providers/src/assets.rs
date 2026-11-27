//! Resolution and loading of the kit's filesystem assets.
//!
//! The kit's icons are SVG files rather than strings embedded in the binary, and its
//! light/dark theme files live beside them in the source repository. That layout is right
//! for a playground and wrong for an installed application unless it is translated: an
//! installed binary cannot assume a sibling source checkout exists. Resolution therefore
//! has three ordered answers — an explicit environment override, the installed layout
//! beside the executable, and the development checkout as a fallback.
//!
//! The override is authoritative even when incomplete. That is deliberate: when an
//! operator explicitly points at a root, silently shopping for a different one hides the
//! mistake. The implicit executable candidate is used only when it is complete, because
//! a partial installation was never asked for explicitly. The development fallback is
//! always returned as a last resort and carries its completeness flag honestly.
//!
//! The integration boundary is respected rather than crossed: gpui 0.2.2 accepts an
//! [`gpui::AssetSource`] only through `Application::with_assets` before the application
//! runs, so [`KitAssets`] cannot install itself. It is instead handed to the engine by the
//! composition root — `ncui-cli` builds the factory with
//! `GpuiHostFactory::with_asset_source` when the kit-providers feature is on — because the
//! composition root is the one place that owns both sides of that handoff. Were the source
//! ever absent, the consequence would be missing SVG glyphs, not inert widgets: hit
//! regions, text, keyboard handling and
//! event wiring do not depend on those files.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

use crate::KitProviderError;

/// The environment variable naming the kit root explicitly.
///
/// The name matches the kit playground's override, so one operator-facing spelling works
/// in both places. The directory it names is expected to contain `assets/icons` and
/// `themes`.
pub const KIT_ROOT_ENV: &str = "NC_UI_KIT_ROOT";

/// Which rule selected an asset layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetResolutionSource {
    /// [`KIT_ROOT_ENV`] was set to a non-empty value. The override wins even when the
    /// layout is incomplete, because an explicit mistake should be visible.
    Environment,
    /// The directory containing the running executable has the complete installed
    /// layout: `assets/icons` and `themes` beside the executable.
    ExecutableDirectory,
    /// The sibling source checkout was used. This is the development fallback and may
    /// be incomplete on a machine without the checkout.
    DevFallback,
}

/// The asset directories selected for this process.
///
/// The value is data rather than a promise: [`AssetResolution::is_complete`] says whether
/// both required directories were present when resolution ran. Consumers decide whether
/// missing themes are a fatal packaging error or a reason to use built-in palettes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetResolution {
    root: PathBuf,
    source: AssetResolutionSource,
    complete: bool,
}

impl AssetResolution {
    /// The root directory containing `assets` and `themes`.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The base directory for gpui asset paths such as `icons/check.svg`.
    #[must_use]
    pub fn assets_dir(&self) -> PathBuf {
        self.root.join("assets")
    }

    /// The directory containing the kit's JSON themes.
    #[must_use]
    pub fn themes_dir(&self) -> PathBuf {
        self.root.join("themes")
    }

    /// Which resolution rule selected this layout.
    #[must_use]
    pub const fn source(&self) -> AssetResolutionSource {
        self.source
    }

    /// Whether `assets/icons` and `themes` both existed when resolution ran.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }
}

/// Resolves the kit asset layout for the current process.
///
/// The order is [`KIT_ROOT_ENV`], then the executable's own directory, then the sibling
/// development checkout. No canonicalisation is performed: a caller installing the app is
/// entitled to use junctions or symlinks, and resolution is about which path was chosen,
/// not about second-guessing the filesystem's answer to where it leads.
#[must_use]
pub fn resolve() -> AssetResolution {
    let environment = std::env::var_os(KIT_ROOT_ENV)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty());
    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let dev_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../nc-ui-kit");
    resolve_with(environment, executable_dir, dev_root, Path::is_dir)
}

/// Builds the filesystem asset source for the resolved layout.
///
/// The returned source is ready for `gpui::Application::with_assets`. It is exposed even
/// though the current `ncui-gpui` engine cannot accept it after construction, because the
/// correct integration is to hand this source to the application at birth rather than to
/// teach every widget a second loading rule.
#[must_use]
pub fn asset_source() -> KitAssets {
    KitAssets::new(resolve().assets_dir())
}

/// The pure half of resolution, dependency-injected so the priority rules are testable
/// without creating directories or depending on the machine the test runs on.
fn resolve_with(
    environment: Option<PathBuf>,
    executable_dir: Option<PathBuf>,
    dev_root: PathBuf,
    is_dir: impl Fn(&Path) -> bool,
) -> AssetResolution {
    if let Some(root) = environment {
        return resolution(root, AssetResolutionSource::Environment, &is_dir);
    }
    if let Some(root) = executable_dir {
        let candidate = resolution(root, AssetResolutionSource::ExecutableDirectory, &is_dir);
        if candidate.is_complete() {
            return candidate;
        }
    }
    resolution(dev_root, AssetResolutionSource::DevFallback, &is_dir)
}

fn resolution(
    root: PathBuf,
    source: AssetResolutionSource,
    is_dir: &impl Fn(&Path) -> bool,
) -> AssetResolution {
    let complete = is_dir(&root.join("assets").join("icons")) && is_dir(&root.join("themes"));
    AssetResolution {
        root,
        source,
        complete,
    }
}

/// A filesystem-backed gpui asset source rooted at the kit's `assets` directory.
///
/// Paths are confined lexically: absolute paths, parent traversal and Windows path
/// prefixes are refused before the filesystem is asked. This is not a sandbox — the
/// process already trusts host-linked providers — but it keeps a malformed asset name
/// from turning a relative lookup into an accidental read elsewhere.
#[derive(Debug, Clone)]
pub struct KitAssets {
    base: PathBuf,
}

impl KitAssets {
    /// A source loading gpui asset paths from `base`.
    #[must_use]
    pub fn new(base: PathBuf) -> Self {
        Self { base }
    }

    /// The directory asset paths are resolved against.
    #[must_use]
    pub fn base(&self) -> &Path {
        &self.base
    }

    fn resolve_path(&self, path: &str) -> Result<PathBuf, KitProviderError> {
        let relative = Path::new(path);
        let mut resolved = self.base.clone();
        for component in relative.components() {
            match component {
                Component::Normal(part) => resolved.push(part),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(KitProviderError::AssetPathOutsideRoot {
                        path: path.to_owned(),
                    });
                }
            }
        }
        Ok(resolved)
    }
}

impl gpui::AssetSource for KitAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let resolved = match self.resolve_path(path) {
            Ok(path) => path,
            Err(error) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    error.to_string(),
                )
                .into());
            }
        };
        match std::fs::read(resolved) {
            Ok(bytes) => Ok(Some(Cow::Owned(bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        let resolved = match self.resolve_path(path) {
            Ok(path) => path,
            Err(error) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    error.to_string(),
                )
                .into());
            }
        };
        let entries = match std::fs::read_dir(resolved) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut names = Vec::new();
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                names.push(gpui::SharedString::from(name.to_owned()));
            }
        }
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_executable(path: &Path) -> bool {
        path == Path::new("C:/installed/assets/icons")
            || path == Path::new("C:/installed/themes")
            || path == Path::new("C:/devkit/assets/icons")
            || path == Path::new("C:/devkit/themes")
    }

    #[test]
    fn an_explicit_environment_override_wins_even_when_incomplete() {
        let resolved = resolve_with(
            Some(PathBuf::from("C:/override")),
            Some(PathBuf::from("C:/installed")),
            PathBuf::from("C:/devkit"),
            complete_executable,
        );
        assert_eq!(resolved.source(), AssetResolutionSource::Environment);
        assert_eq!(resolved.root(), Path::new("C:/override"));
        assert!(
            !resolved.is_complete(),
            "the override is authoritative rather than silently corrected"
        );
    }

    #[test]
    fn a_complete_installed_layout_wins_when_no_override_exists() {
        let resolved = resolve_with(
            None,
            Some(PathBuf::from("C:/installed")),
            PathBuf::from("C:/devkit"),
            complete_executable,
        );
        assert_eq!(
            resolved.source(),
            AssetResolutionSource::ExecutableDirectory
        );
        assert_eq!(resolved.assets_dir(), PathBuf::from("C:/installed/assets"));
        assert!(resolved.is_complete());
    }

    #[test]
    fn an_incomplete_executable_layout_falls_through_to_the_development_checkout() {
        let resolved = resolve_with(
            None,
            Some(PathBuf::from("C:/partial")),
            PathBuf::from("C:/devkit"),
            complete_executable,
        );
        assert_eq!(resolved.source(), AssetResolutionSource::DevFallback);
        assert_eq!(resolved.root(), Path::new("C:/devkit"));
        assert!(resolved.is_complete());
    }

    #[test]
    fn the_development_fallback_reports_incompleteness_honestly() {
        let resolved = resolve_with(None, None, PathBuf::from("C:/absent"), |_| false);
        assert_eq!(resolved.source(), AssetResolutionSource::DevFallback);
        assert!(!resolved.is_complete());
    }

    #[test]
    fn asset_paths_cannot_escape_their_root() {
        let assets = KitAssets::new(PathBuf::from("assets"));
        assert!(assets.resolve_path("icons/check.svg").is_ok());
        assert!(assets.resolve_path("../secret").is_err());
        assert!(assets.resolve_path("/etc/passwd").is_err());
        assert!(assets.resolve_path("C:/Windows/win.ini").is_err());
    }
}
