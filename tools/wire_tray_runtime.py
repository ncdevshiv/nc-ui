"""Wires the tray icon through the runtime's window surface, the manifest and the application."""

from pathlib import Path

# 1. The runtime's feature.
p = Path('crates/ncui-runtime/Cargo.toml')
s = p.read_text(encoding='utf-8')
old = 'wry-host = ["dep:ncui-webview-wry", "ncui-webview-wry/wry-engine"]'
new = ('wry-host = ["dep:ncui-webview-wry", "ncui-webview-wry/wry-engine"]\n'
       '# Enables the application tray icon. Implies the window host, because a tray icon belongs\n'
       '# to the engine\'s event loop and there is no tray without one.\n'
       'tray-icon = ["wry-host", "ncui-webview-wry/tray-icon"]')
assert old in s, 'wry-host feature not found'
s = s.replace(old, new, 1)
p.write_text(s, encoding='utf-8')

# 2. The window surface.
p = Path('crates/ncui-runtime/src/window.rs')
s = p.read_text(encoding='utf-8')
old = """    /// Mounts a component into a window."""
new = """    /// Shows the application's tray icon, or removes it, reporting whether the platform agreed.
    ///
    /// Asked through a window because the window is the handle to the engine that owns the
    /// platform's event loop, not because the tray belongs to it: the tray belongs to the
    /// application, and an application whose window is closed is still one a user may need to reach.
    ///
    /// # Errors
    ///
    /// Returns [`WindowRejection::EngineUnavailable`] when the window is not open, when this build
    /// cannot show a tray at all, or when the platform refused it.
    pub fn set_tray(
        &self,
        id: WindowId,
        tray: Option<ncui_webview::TrayRequest>,
    ) -> Result<(), WindowRejection> {
        self.with_registry_mut(|registry| {
            let window =
                registry
                    .get_mut(id)
                    .ok_or_else(|| WindowRejection::EngineUnavailable {
                        reason: Some(format!("window {id} is not open")),
                    })?;
            if !window.set_tray(tray) {
                return Err(WindowRejection::EngineUnavailable {
                    reason: Some(format!(
                        "the engine for window {id} cannot show a tray icon"
                    )),
                });
            }
            Ok(())
        })
    }

    /// Mounts a component into a window."""
assert old in s, 'mount anchor not found'
s = s.replace(old, new, 1)
p.write_text(s, encoding='utf-8')

# 3. The manifest's tray.
p = Path('crates/ncui-app/src/manifest.rs')
s = p.read_text(encoding='utf-8')
old = """    /// The global shortcuts the application declares."""
new = """    /// The tray icon the application shows, when it shows one.
    ///
    /// Optional, and absent for most applications: a tray icon is for an application a user needs to
    /// reach when none of its windows are visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tray: Option<TrayConfig>,
    /// The global shortcuts the application declares."""
assert old in s, 'shortcuts field anchor not found'
s = s.replace(old, new, 1)

old = """/// One global shortcut an application declares."""
new = """/// The tray icon an application shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrayConfig {
    /// The image the tray shows, as a path relative to the application.
    pub icon: String,
    /// The words the platform shows when the pointer rests on the tray icon.
    pub tooltip: String,
}

/// One global shortcut an application declares."""
assert old in s, 'shortcut config anchor not found'
s = s.replace(old, new, 1)

# Validation: the same rules the icon and the request are held to.
old = """        // The accelerator's *shape* is checked here and its meaning where it is used: this crate"""
new = """        if let Some(tray) = &self.tray {
            if tray.icon.trim().is_empty() {
                return Err(ManifestError::Invalid {
                    field: "tray.icon".to_owned(),
                    reason: "must not be empty".to_owned(),
                });
            }
            let path = Path::new(&tray.icon);
            if path.is_absolute() || tray.icon.starts_with('/') || tray.icon.starts_with('\\\\') {
                return Err(ManifestError::Invalid {
                    field: "tray.icon".to_owned(),
                    reason: format!(
                        "must be a path relative to the application; got `{}`",
                        tray.icon
                    ),
                });
            }
            if tray.icon.split(['/', '\\\\']).any(|part| part == "..") {
                return Err(ManifestError::Invalid {
                    field: "tray.icon".to_owned(),
                    reason: format!(
                        "must not traverse outside the application; got `{}`",
                        tray.icon
                    ),
                });
            }
            if tray.tooltip.trim().is_empty() {
                return Err(ManifestError::Invalid {
                    field: "tray.tooltip".to_owned(),
                    reason: "must not be empty: a tray icon with no words says nothing about what \\
                             it belongs to"
                        .to_owned(),
                });
            }
        }
        // The accelerator's *shape* is checked here and its meaning where it is used: this crate"""
assert old in s, 'shortcut validation anchor not found'
s = s.replace(old, new, 1)

# The tray's icon resolves the way the window's does.
old = """    /// The entry's path relative to the assets directory, as a URL path."""
new = """    /// The tray icon's path inside the application directory, when the application shows one.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] when the tray names a path that cannot be resolved inside
    /// the application directory.
    pub fn tray_icon_path(
        &self,
        application_directory: &Path,
    ) -> Result<Option<PathBuf>, ManifestError> {
        let Some(icon) = self
            .tray
            .as_ref()
            .map(|tray| tray.icon.as_str())
            .filter(|icon| !icon.is_empty())
        else {
            return Ok(None);
        };
        let path = ncui_security::path::SafePath::resolve(application_directory, icon).map_err(
            |error| ManifestError::Invalid {
                field: "tray.icon".to_owned(),
                reason: format!("does not name a file inside the application: {error}"),
            },
        )?;
        Ok(Some(path))
    }

    /// The entry's path relative to the assets directory, as a URL path."""
assert old in s, 'entry url anchor not found'
s = s.replace(old, new, 1)

p.write_text(s, encoding='utf-8')

# 4. The re-export.
p = Path('crates/ncui-app/src/lib.rs')
s = p.read_text(encoding='utf-8')
old = "pub use manifest::{\n    AppManifest, MANIFEST_FILE_NAME, ManifestError, ShortcutConfig, WindowConfig,\n};"
new = "pub use manifest::{\n    AppManifest, MANIFEST_FILE_NAME, ManifestError, ShortcutConfig, TrayConfig, WindowConfig,\n};"
assert old in s, 'app re-export not found'
s = s.replace(old, new, 1)
p.write_text(s, encoding='utf-8')

print('runtime surface and manifest wired')
