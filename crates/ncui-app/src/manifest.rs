//! The application manifest: what an application says about itself.
//!
//! An application is a directory with a manifest in it. The manifest names the application,
//! the page it opens, the assets it serves, the window it opens it in, and the capabilities
//! its pages are granted. It is the one file that turns a directory of files into something
//! a runtime can run, and it is the file an author edits — so every field in it has to mean
//! something the runtime actually does.
//!
//! # Why JSON and not TOML
//!
//! ADR-012 keeps the dependency list deliberately small, and TOML would add one. JSON is
//! already a dependency of every crate in the workspace, and the manifest is read once at
//! startup, where a parse cost is irrelevant.
//!
//! # Why the manifest validates rather than trusting
//!
//! Every field is checked before the runtime acts on it, and the checks are the ones that
//! would otherwise be scattered through the loader: a name that is empty, an entry that
//! points outside the application, a capability that does not exist, a window that is too
//! small to use. A manifest that is wrong is refused with a message that says which field
//! is wrong, because the person who can fix it is reading the error, not the code.

use core::fmt::Write as _;
use std::path::{Path, PathBuf};

use ncui_core::CapabilitySet;
use serde::{Deserialize, Serialize};

/// The file an application's manifest lives in.
pub const MANIFEST_FILE_NAME: &str = "ncui.app.json";

/// The directory an application keeps its assets in, when the manifest does not say.
pub const DEFAULT_ASSETS_DIRECTORY: &str = "public";

/// An application's own description of itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppManifest {
    /// The application's name.
    pub name: String,
    /// The application's version.
    pub version: String,
    /// The page the application opens, relative to its assets directory.
    #[serde(default = "default_entry")]
    pub entry: String,
    /// The directory the application's assets are served from, relative to the application.
    #[serde(default = "default_assets")]
    pub assets: String,
    /// The window the application opens.
    #[serde(default)]
    pub window: WindowConfig,
    /// The tray icon the application shows, when it shows one.
    ///
    /// Optional, and absent for most applications: a tray icon is for an application a user needs to
    /// reach when none of its windows are visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tray: Option<TrayConfig>,
    /// The global shortcuts the application declares.
    ///
    /// Declared here rather than asked for by a page: a shortcut is a key taken from every other
    /// application on the machine, and a page that could register one could take the key another
    /// program is using. What a page can do is *hear* about a shortcut this application declared.
    #[serde(default)]
    pub shortcuts: Vec<ShortcutConfig>,
    /// The capabilities the application's pages are granted.
    #[serde(default)]
    pub capabilities: CapabilitySet,
    /// The URL scheme this application handles, when it handles one.
    ///
    /// A link of the form `scheme://…` is handed to the application by the platform, which is
    /// what makes a deep link work. The scheme is validated here for the same reason the identity
    /// is: it is written into a registry key or a desktop entry, and a value that reaches one
    /// unvalidated is a value that can carry something else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// The identity the application is registered under, when it names one.
    ///
    /// It is what a notification is delivered under on Windows and what a bundle registers with
    /// the platform, so it is validated here rather than at the point it is used: an identity
    /// that reaches a script or a registry key unvalidated is an identity that can carry
    /// something else. An application that names none gets one derived from its name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

/// The tray icon an application shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrayConfig {
    /// The image the tray shows, as a path relative to the application.
    pub icon: String,
    /// The words the platform shows when the pointer rests on the tray icon.
    pub tooltip: String,
}

/// One global shortcut an application declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShortcutConfig {
    /// The accelerator, such as `Ctrl+Shift+K`.
    pub accelerator: String,
    /// The command the page is told about when the shortcut is pressed.
    ///
    /// A name rather than an action: what a shortcut *does* is the application's decision, and the
    /// page is the part of the application that knows. The runtime's job is to deliver the press.
    pub command: String,
}

/// Whether `scheme` has the shape of a URL scheme.
///
/// The shape only: whether a scheme is one the machine already uses is a question for the code
/// that registers it, which is where the answer is acted on.
#[must_use]
pub fn is_scheme(scheme: &str) -> bool {
    let mut characters = scheme.chars();
    match characters.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    scheme.len() <= 64
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Whether `identity` is an application identity rather than something else.
///
/// Deliberately an allow-list: the value is written into a Windows registry key and into a
/// generated script, so a character that is not part of an identity is refused rather than
/// escaped, because there is no legitimate identity that needs it.
#[must_use]
pub fn is_identity(identity: &str) -> bool {
    !identity.is_empty()
        && identity.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    '{' | '}' | '\\' | '.' | '-' | '_' | ' ' | '!' | '~'
                )
        })
}

/// The page an application opens, when the manifest does not say.
fn default_entry() -> String {
    "index.html".to_owned()
}

/// The assets directory an application serves, when the manifest does not say.
fn default_assets() -> String {
    DEFAULT_ASSETS_DIRECTORY.to_owned()
}

/// The window an application opens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowConfig {
    /// The title the window's frame carries.
    #[serde(default)]
    pub title: Option<String>,
    /// The window's width in logical pixels.
    #[serde(default)]
    pub width: Option<u32>,
    /// The window's height in logical pixels.
    #[serde(default)]
    pub height: Option<u32>,
    /// Whether the window floats above other windows.
    #[serde(default)]
    pub always_on_top: bool,
    /// Whether the window carries its own frame.
    #[serde(default = "default_true")]
    pub decorations: bool,
    /// Whether the window's background may be see-through.
    #[serde(default)]
    pub transparent: bool,
    /// Whether the user may resize the window.
    #[serde(default = "default_true")]
    pub resizable: bool,
    /// The smallest width the user may drag the window to.
    #[serde(default)]
    pub min_width: Option<u32>,
    /// The smallest height the user may drag the window to.
    #[serde(default)]
    pub min_height: Option<u32>,
    /// Whether the window opens filling its display.
    #[serde(default)]
    pub fullscreen: bool,
    /// Whether the window opens maximised.
    #[serde(default)]
    pub maximized: bool,
    /// The image the window carries, as a path relative to the application.
    ///
    /// Optional because most applications do not need one: a window with no icon is a window the
    /// platform draws with the program's own. The path is relative to the application directory
    /// rather than to the assets directory, because an icon belongs to the application and is not
    /// something the page fetches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// The default for a window option that is on unless an application says otherwise.
const fn default_true() -> bool {
    true
}

/// A manifest that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// The manifest file could not be read.
    Io {
        /// The path that could not be read.
        path: PathBuf,
        /// What the operating system said.
        reason: String,
    },
    /// The manifest is not valid JSON, or does not have the shape a manifest has.
    Malformed {
        /// The path that was read.
        path: PathBuf,
        /// What was wrong with it.
        reason: String,
    },
    /// A field of the manifest names something that cannot be used.
    Invalid {
        /// The field that is wrong.
        field: String,
        /// Why it is wrong.
        reason: String,
    },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, reason } => {
                write!(f, "cannot read manifest {}: {reason}", path.display())
            }
            Self::Malformed { path, reason } => {
                write!(f, "manifest {} is malformed: {reason}", path.display())
            }
            Self::Invalid { field, reason } => write!(f, "manifest field `{field}` {reason}"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl AppManifest {
    /// Reads the manifest in `directory`.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Io`] when the file cannot be read,
    /// [`ManifestError::Malformed`] when it is not a manifest, and
    /// [`ManifestError::Invalid`] when a field cannot be used.
    pub fn load(directory: &Path) -> Result<Self, ManifestError> {
        let path = directory.join(MANIFEST_FILE_NAME);
        let raw = std::fs::read_to_string(&path).map_err(|error| ManifestError::Io {
            path: path.clone(),
            reason: error.to_string(),
        })?;
        Self::parse(&raw, &path)
    }

    /// Reads a manifest from text.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Malformed`] when the text is not a manifest, and
    /// [`ManifestError::Invalid`] when a field cannot be used.
    pub fn parse(raw: &str, path: &Path) -> Result<Self, ManifestError> {
        let manifest: Self =
            serde_json::from_str(raw).map_err(|error| ManifestError::Malformed {
                path: path.to_path_buf(),
                reason: error.to_string(),
            })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Checks every field the runtime will act on.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] for a field that cannot be used.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.name.trim().is_empty() {
            return Err(ManifestError::Invalid {
                field: "name".to_owned(),
                reason: "must not be empty".to_owned(),
            });
        }
        if let Some(scheme) = &self.scheme
            && !is_scheme(scheme)
        {
            return Err(ManifestError::Invalid {
                field: "scheme".to_owned(),
                reason: format!(
                    "must be a URL scheme — a letter followed by letters, digits, `+`, `-` or \
                     `.` — got `{scheme}`"
                ),
            });
        }
        if let Some(identity) = &self.identity
            && !is_identity(identity)
        {
            return Err(ManifestError::Invalid {
                field: "identity".to_owned(),
                reason: format!(
                    "must be an application identity — letters, digits, dots, dashes, \
                     underscores, braces, spaces, `!` or `~` — got `{identity}`"
                ),
            });
        }
        self.check_tray()?;
        self.check_shortcuts()?;
        if self.version.trim().is_empty() {
            return Err(ManifestError::Invalid {
                field: "version".to_owned(),
                reason: "must not be empty".to_owned(),
            });
        }
        if !is_version(&self.version) {
            return Err(ManifestError::Invalid {
                field: "version".to_owned(),
                reason: format!(
                    "must be three numbers separated by dots, such as 1.0.0; got `{}`",
                    self.version
                ),
            });
        }
        if self.entry.trim().is_empty() {
            return Err(ManifestError::Invalid {
                field: "entry".to_owned(),
                reason: "must not be empty".to_owned(),
            });
        }
        // The entry, the assets directory and the icon are all relative paths that will be joined
        // onto the application directory. A path that is absolute, or that climbs out with
        // `..`, would name a file outside the application — so they are refused here rather
        // than at the point of use.
        let icon = self.window.icon.as_deref().unwrap_or_default();
        for (field, value) in [
            ("entry", self.entry.as_str()),
            ("assets", self.assets.as_str()),
            ("icon", icon),
        ] {
            if value.is_empty() {
                continue;
            }
            if Path::new(value).is_absolute() || value.starts_with('/') || value.starts_with('\\') {
                return Err(ManifestError::Invalid {
                    field: field.to_owned(),
                    reason: format!("must be a path relative to the application; got `{value}`"),
                });
            }
            if value.split(['/', '\\']).any(|part| part == "..") {
                return Err(ManifestError::Invalid {
                    field: field.to_owned(),
                    reason: format!("must not traverse outside the application; got `{value}`"),
                });
            }
        }
        Ok(())
    }

    /// Checks the tray icon the application declares, when it declares one.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] for a tray whose image is not a path inside the
    /// application, or whose tooltip says nothing.
    fn check_tray(&self) -> Result<(), ManifestError> {
        let Some(tray) = &self.tray else {
            return Ok(());
        };
        if tray.icon.trim().is_empty() {
            return Err(ManifestError::Invalid {
                field: "tray.icon".to_owned(),
                reason: "must not be empty".to_owned(),
            });
        }
        let path = Path::new(&tray.icon);
        if path.is_absolute() || tray.icon.starts_with('/') || tray.icon.starts_with('\\') {
            return Err(ManifestError::Invalid {
                field: "tray.icon".to_owned(),
                reason: format!(
                    "must be a path relative to the application; got `{}`",
                    tray.icon
                ),
            });
        }
        if tray.icon.split(['/', '\\']).any(|part| part == "..") {
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
                reason: "must not be empty: a tray icon with no words says nothing about what it \
                         belongs to"
                    .to_owned(),
            });
        }
        Ok(())
    }

    /// Checks the shortcuts the application declares.
    ///
    /// The accelerator's *shape* is checked here and its meaning where it is used: this crate cannot
    /// see the vocabulary that reads it, because that lives in the crate that talks to the platform,
    /// and a manifest that names a key this runtime does not know is refused at launch rather than
    /// here.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] for an accelerator that is empty, too long, not ASCII or
    /// carries a space, and for a shortcut with no command.
    fn check_shortcuts(&self) -> Result<(), ManifestError> {
        for (index, shortcut) in self.shortcuts.iter().enumerate() {
            if shortcut.accelerator.trim().is_empty() {
                return Err(ManifestError::Invalid {
                    field: format!("shortcuts[{index}].accelerator"),
                    reason: "must not be empty".to_owned(),
                });
            }
            if shortcut.accelerator.len() > 64
                || !shortcut.accelerator.is_ascii()
                || shortcut.accelerator.chars().any(char::is_whitespace)
            {
                return Err(ManifestError::Invalid {
                    field: format!("shortcuts[{index}].accelerator"),
                    reason: format!(
                        "must be a short ASCII accelerator with no spaces, such as \
                         `Ctrl+Shift+K`; got `{}`",
                        shortcut.accelerator
                    ),
                });
            }
            if shortcut.command.trim().is_empty() {
                return Err(ManifestError::Invalid {
                    field: format!("shortcuts[{index}].command"),
                    reason: "must not be empty".to_owned(),
                });
            }
        }
        Ok(())
    }

    /// The directory the application's assets are served from.
    ///
    /// The path is joined onto the application directory and is not canonicalized: the
    /// directory may not exist yet when the manifest is read, and the server canonicalizes
    /// it when it binds.
    #[must_use]
    pub fn assets_directory(&self, application_directory: &Path) -> PathBuf {
        application_directory.join(&self.assets)
    }

    /// The page the application opens, as a path under the assets directory.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] when the entry names a path that cannot be
    /// resolved inside the assets directory.
    pub fn entry_path(&self, application_directory: &Path) -> Result<PathBuf, ManifestError> {
        let assets = self.assets_directory(application_directory);
        ncui_security::path::SafePath::resolve(&assets, &self.entry).map_err(|error| {
            ManifestError::Invalid {
                field: "entry".to_owned(),
                reason: format!("does not name a file inside the application: {error}"),
            }
        })
    }

    /// The icon's path inside the application directory, when the application names one.
    ///
    /// `None` when the manifest names no icon, which is the common case: a window with no icon is
    /// a window the platform draws with the program's own.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] when the icon names a path that cannot be resolved
    /// inside the application directory.
    pub fn icon_path(
        &self,
        application_directory: &Path,
    ) -> Result<Option<PathBuf>, ManifestError> {
        let Some(icon) = self.window.icon.as_deref().filter(|icon| !icon.is_empty()) else {
            return Ok(None);
        };
        let path = ncui_security::path::SafePath::resolve(application_directory, icon).map_err(
            |error| ManifestError::Invalid {
                field: "icon".to_owned(),
                reason: format!("does not name a file inside the application: {error}"),
            },
        )?;
        Ok(Some(path))
    }

    /// The tray icon's path inside the application directory, when the application shows one.
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

    /// The entry's path relative to the assets directory, as a URL path.
    ///
    /// The path is percent-encoded per component, so a name with a space in it becomes a
    /// URL a browser can fetch.
    #[must_use]
    pub fn entry_url_path(&self) -> String {
        let mut path = String::from("/");
        for (index, component) in self.entry.split(['/', '\\']).enumerate() {
            if index > 0 {
                path.push('/');
            }
            path.push_str(&percent_encode(component));
        }
        path
    }
}

/// Whether a version string is three numbers separated by dots.
///
/// The rule is deliberately narrow: a version that cannot be compared is worse than no
/// version, because a consumer that has to parse it again will parse it differently.
fn is_version(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Percent-encodes one path component.
///
/// Everything outside the unreserved set of RFC 3986 is encoded, so the result can be put
/// in a URL without changing its meaning.
#[must_use]
pub fn percent_encode(component: &str) -> String {
    let mut encoded = String::with_capacity(component.len());
    for byte in component.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            other => {
                let _ = write!(encoded, "%{other:02X}");
            }
        }
    }
    encoded
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
    use ncui_core::Capability;

    fn manifest(raw: &str) -> Result<AppManifest, ManifestError> {
        AppManifest::parse(raw, Path::new(MANIFEST_FILE_NAME))
    }

    #[test]
    fn a_link_scheme_is_validated_because_it_reaches_a_registry_key() {
        let base = AppManifest::parse(
            r#"{"name":"counter","version":"1.0.0"}"#,
            Path::new("ncui.app.json"),
        )
        .expect("a minimal manifest");
        assert!(is_scheme("ncui"));
        assert!(is_scheme("my-app.v2"));
        assert!(!is_scheme(""));
        assert!(!is_scheme("1app"));
        assert!(!is_scheme("app://"));
        assert!(!is_scheme("app/x"));
        assert!(!is_scheme(&"a".repeat(65)));

        let mut manifest = AppManifest {
            scheme: Some("ncui".to_owned()),
            ..base
        };
        assert!(manifest.validate().is_ok());
        manifest.scheme = Some("ncui://open".to_owned());
        let error = manifest.validate().expect_err("not a scheme");
        assert!(error.to_string().contains("scheme"), "{error}");
    }

    #[test]
    fn an_identity_is_validated_because_it_reaches_a_registry_key_and_a_script() {
        // The value is written into the platform's registry and into a generated script, so a
        // character that is not part of an identity is refused rather than escaped.
        let base = AppManifest::parse(
            r#"{"name":"counter","version":"1.0.0"}"#,
            Path::new("ncui.app.json"),
        )
        .expect("a minimal manifest");
        let mut manifest = AppManifest {
            identity: Some("com.ncui.counter".to_owned()),
            ..base
        };
        assert!(manifest.validate().is_ok());
        assert!(is_identity("com.ncui.counter"));
        assert!(is_identity("a b"));
        assert!(!is_identity(""));
        assert!(!is_identity("a'; Remove-Item C:\\; 'b"));
        assert!(!is_identity("a\nb"));

        manifest.identity = Some("com.ncui.'; rm -rf /".to_owned());
        let error = manifest
            .validate()
            .expect_err("an identity that is not one is refused");
        assert!(error.to_string().contains("identity"), "{error}");

        // An application that names none is valid: its identity is derived when a bundle is
        // built, so the field stays optional.
        manifest.identity = None;
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn a_declared_shortcut_is_read_and_a_malformed_one_is_refused() {
        let parsed = manifest(
            r#"{"name":"a","version":"1.0.0","shortcuts":[{"accelerator":"Ctrl+Shift+K","command":"go"}]}"#,
        )
        .expect("a manifest with a shortcut");
        assert_eq!(parsed.shortcuts.len(), 1);
        assert_eq!(parsed.shortcuts[0].accelerator, "Ctrl+Shift+K");
        assert_eq!(parsed.shortcuts[0].command, "go");

        for raw in [
            // No accelerator.
            r#"{"name":"a","version":"1.0.0","shortcuts":[{"accelerator":"","command":"go"}]}"#,
            // Spaces: an accelerator is one word per part, and a space is not part of one.
            r#"{"name":"a","version":"1.0.0","shortcuts":[{"accelerator":"Ctrl Shift K","command":"go"}]}"#,
            // No command: a shortcut that does nothing is not a shortcut.
            r#"{"name":"a","version":"1.0.0","shortcuts":[{"accelerator":"Ctrl+K","command":"  "}]}"#,
            // No command at all, which is not a shortcut this can read.
            r#"{"name":"a","version":"1.0.0","shortcuts":[{"accelerator":"Ctrl+K"}]}"#,
        ] {
            let error = manifest(raw).expect_err("a malformed shortcut is refused");
            assert!(
                matches!(
                    error,
                    ManifestError::Invalid { .. } | ManifestError::Malformed { .. }
                ),
                "{raw} produced {error}"
            );
        }
    }

    #[test]
    fn the_example_application_s_manifest_is_one_this_accepts() {
        // The example is the documentation a person copies, so a manifest that no longer loads
        // would be documentation that no longer works.
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/counter")
            .join(MANIFEST_FILE_NAME);
        let raw = std::fs::read_to_string(&path).expect("the example's manifest is readable");
        let parsed = AppManifest::parse(&raw, &path).expect("the example's manifest is valid");
        assert_eq!(parsed.name, "counter");
        assert!(
            !parsed.shortcuts.is_empty(),
            "the example declares a global shortcut, and this is what keeps it honest"
        );
        assert!(
            parsed
                .shortcuts
                .iter()
                .all(|shortcut| !shortcut.command.is_empty()),
            "every declared shortcut names a command"
        );
    }

    #[test]
    fn a_tray_icon_is_read_and_a_malformed_one_is_refused() {
        let parsed = manifest(
            r#"{"name":"a","version":"1.0.0","tray":{"icon":"tray.bmp","tooltip":"Counter"}}"#,
        )
        .expect("a manifest with a tray icon");
        let tray = parsed.tray.as_ref().expect("the tray is read");
        assert_eq!(tray.icon, "tray.bmp");
        assert_eq!(tray.tooltip, "Counter");

        // An application that shows no tray says nothing about one.
        let plain = manifest(r#"{"name":"a","version":"1.0.0"}"#).expect("a manifest");
        assert!(plain.tray.is_none());
        assert_eq!(
            plain
                .tray_icon_path(Path::new("/srv/app"))
                .expect("no tray"),
            None
        );

        // The tray's image resolves inside the application directory, like the window's.
        let root = Path::new("/srv/app");
        assert_eq!(
            parsed.tray_icon_path(root).expect("the tray resolves"),
            Some(root.join("tray.bmp"))
        );

        for raw in [
            // An image outside the application.
            r#"{"name":"a","version":"1.0.0","tray":{"icon":"../outside.bmp","tooltip":"x"}}"#,
            // An image at an absolute path.
            r#"{"name":"a","version":"1.0.0","tray":{"icon":"/etc/tray.bmp","tooltip":"x"}}"#,
            // No image at all.
            r#"{"name":"a","version":"1.0.0","tray":{"icon":"","tooltip":"x"}}"#,
            // A tooltip with no words says nothing about what the icon belongs to.
            r#"{"name":"a","version":"1.0.0","tray":{"icon":"tray.bmp","tooltip":"  "}}"#,
            // No tooltip field at all, which is not a tray this can read.
            r#"{"name":"a","version":"1.0.0","tray":{"icon":"tray.bmp"}}"#,
        ] {
            let error = manifest(raw).expect_err("a malformed tray is refused");
            assert!(
                matches!(
                    error,
                    ManifestError::Invalid { .. } | ManifestError::Malformed { .. }
                ),
                "{raw} produced {error}"
            );
        }
    }

    #[test]
    fn a_complete_manifest_is_read() {
        let parsed = manifest(
            r#"{
                "name": "counter",
                "version": "1.0.0",
                "entry": "app.html",
                "assets": "public",
                "window": { "title": "Counter", "width": 640, "height": 480 },
                "capabilities": ["read.metrics"]
            }"#,
        )
        .expect("a complete manifest is valid");
        assert_eq!(parsed.name, "counter");
        assert_eq!(parsed.version, "1.0.0");
        assert_eq!(parsed.entry, "app.html");
        assert_eq!(parsed.assets, "public");
        assert_eq!(parsed.window.title.as_deref(), Some("Counter"));
        assert_eq!(parsed.window.width, Some(640));
        assert_eq!(parsed.window.height, Some(480));
        assert_eq!(
            parsed.capabilities,
            CapabilitySet::empty().with(Capability::ReadMetrics)
        );
    }

    #[test]
    fn a_manifest_with_only_a_name_and_version_takes_the_defaults() {
        let parsed =
            manifest(r#"{"name":"counter","version":"1.0.0"}"#).expect("a minimal manifest");
        assert_eq!(parsed.entry, "index.html");
        assert_eq!(parsed.assets, DEFAULT_ASSETS_DIRECTORY);
        assert_eq!(parsed.window, WindowConfig::default());
        assert!(parsed.capabilities.is_empty());
    }

    #[test]
    fn a_manifest_that_is_not_json_is_refused() {
        let error = manifest("this is not json").expect_err("not json");
        assert!(
            matches!(error, ManifestError::Malformed { .. }),
            "got: {error}"
        );
    }

    #[test]
    fn a_manifest_with_an_unknown_capability_is_refused() {
        let error =
            manifest(r#"{"name":"a","version":"1.0.0","capabilities":["not.a.capability"]}"#)
                .expect_err("unknown capability");
        assert!(
            matches!(error, ManifestError::Malformed { .. }),
            "got: {error}"
        );
        assert!(
            error.to_string().contains("not.a.capability"),
            "the refusal must name the capability it did not recognise; got: {error}"
        );
    }

    #[test]
    fn a_manifest_with_an_empty_name_or_version_is_refused() {
        for raw in [
            r#"{"name":"","version":"1.0.0"}"#,
            r#"{"name":"a","version":""}"#,
            r#"{"name":"a","version":"one"}"#,
            r#"{"name":"a","version":"1.0"}"#,
        ] {
            let error = manifest(raw).expect_err("an invalid manifest");
            assert!(
                matches!(error, ManifestError::Invalid { .. }),
                "{raw} produced {error}"
            );
        }
    }

    #[test]
    fn an_entry_that_leaves_the_application_is_refused() {
        for entry in ["../outside.html", "/etc/passwd", "a/../../b.html"] {
            let raw = format!(r#"{{"name":"a","version":"1.0.0","entry":"{entry}"}}"#);
            let error = manifest(&raw).expect_err("an entry that escapes");
            assert!(
                matches!(error, ManifestError::Invalid { .. }),
                "{entry} produced {error}"
            );
        }
    }

    #[test]
    fn the_entry_resolves_inside_the_assets_directory() {
        let parsed = manifest(r#"{"name":"a","version":"1.0.0","entry":"pages/app.html"}"#)
            .expect("manifest");
        let root = Path::new("/srv/app");
        let resolved = parsed.entry_path(root).expect("the entry resolves");
        assert_eq!(resolved, root.join("public").join("pages").join("app.html"));
    }

    #[test]
    fn an_icon_is_named_by_a_path_inside_the_application() {
        let parsed =
            manifest(r#"{"name":"counter","version":"1.0.0","window":{"icon":"images/icon.bmp"}}"#)
                .expect("a manifest with an icon");
        assert_eq!(parsed.window.icon.as_deref(), Some("images/icon.bmp"));

        // The icon is resolved inside the application directory, not the assets directory: it
        // belongs to the application and is not something the page fetches.
        let root = Path::new("/srv/app");
        let resolved = parsed
            .icon_path(root)
            .expect("the icon resolves")
            .expect("the manifest names one");
        assert_eq!(resolved, root.join("images").join("icon.bmp"));

        // An application that names no icon has none to resolve.
        let plain = manifest(r#"{"name":"counter","version":"1.0.0"}"#).expect("a manifest");
        assert_eq!(plain.window.icon, None);
        assert_eq!(plain.icon_path(root).expect("no icon"), None);

        // An icon outside the application is refused, as any path outside it is.
        for icon in ["../outside.bmp", "/etc/icon.bmp", "a/../../icon.bmp"] {
            let raw = format!(r#"{{"name":"a","version":"1.0.0","window":{{"icon":"{icon}"}}}}"#);
            let error = manifest(&raw).expect_err("an icon that escapes");
            assert!(
                matches!(error, ManifestError::Invalid { .. }),
                "{icon} produced {error}"
            );
            assert!(error.to_string().contains("icon"), "{error}");
        }
    }

    #[test]
    fn the_entry_url_path_is_encoded_per_component() {
        let parsed = manifest(r#"{"name":"a","version":"1.0.0","entry":"my pages/a b.html"}"#)
            .expect("manifest");
        assert_eq!(parsed.entry_url_path(), "/my%20pages/a%20b.html");
    }
}
