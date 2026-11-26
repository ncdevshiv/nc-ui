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
    /// The native menu bar the application declares.
    ///
    /// A menu is authored here rather than registered by a page for the same reason a global
    /// shortcut is: it is application chrome, and the commands it emits are choices the author
    /// exposes to the user. Selecting one delivers its command name to the page as a `menu` event.
    #[serde(default)]
    pub menus: Vec<MenuConfig>,
    /// The sidecar commands the application declares.
    ///
    /// A sidecar is a command a page can call that is answered by *any* executable the
    /// application ships or names — a Go binary, a Python script, a Zig tool, anything that
    /// reads a JSON request on standard input and writes a JSON answer on standard output.
    /// It is how an application's backend is not Rust-only: the page calls `mycommand`, the
    /// runtime runs the program, and the program's answer comes back over the same
    /// capability-gated seam a native command uses.
    ///
    /// Declared in the manifest rather than registered by a page for the same reason a shortcut
    /// is: which programs an application runs is the application author's decision, and the
    /// `process.spawn` capability is what authorises running them at all. A sidecar is only
    /// registered when the application holds that capability, and a page still needs the
    /// capability each sidecar's `requires` names to call it.
    #[serde(default)]
    pub sidecars: Vec<SidecarConfig>,
    /// The commands the application declares the page answers in JavaScript.
    ///
    /// A page command is the other half of "a command is not Rust-only": where a sidecar hands
    /// the call to another program, a page command hands it back to the page's own handler. It
    /// is declared here — not registered by the page at runtime — so the command is in the action
    /// table pages are checked against and gated by `requires` exactly like a native command,
    /// and so the page cannot invent a route the author did not declare. The page registers the
    /// function that answers it with `window.ncui.registerCommand`; a command declared but never
    /// handed a handler answers with an error rather than running anything.
    #[serde(default)]
    pub page_commands: Vec<PageCommandConfig>,
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
    /// The file extensions this application associates with itself, e.g. `["notes", "md"]`.
    ///
    /// A file of one of these types opens with this application — double-clicked, or opened
    /// through the platform's "open with" — exactly as a link of the manifest's scheme does.
    /// Each is a lowercase extension without its dot; the platform registration happens when
    /// the application is installed, and the extensions the operating system loads and runs
    /// code with (`exe`, `dll`, …) are refused there.
    #[serde(default)]
    pub associations: Vec<String>,
    /// The identity the application is registered under, when it names one.
    ///
    /// It is what a notification is delivered under on Windows and what a bundle registers with
    /// the platform, so it is validated here rather than at the point it is used: an identity
    /// that reaches a script or a registry key unvalidated is an identity that can carry
    /// something else. An application that names none gets one derived from its name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// The language the application's interface is written in, as a BCP 47 tag.
    ///
    /// Optional. An application that declares one is telling the runtime — and so a screen
    /// reader, and the page's own formatting — what language its text is in, which decides the
    /// text direction the interface lays out in: an application declaring `he` or `ar` lays out
    /// right-to-left. Absent means the runtime falls back to the platform's locale, and an
    /// application that knows neither is told `"und"` (undetermined) rather than guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_locale: Option<String>,
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

/// One top-level native menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuConfig {
    /// The label shown in the menu bar.
    pub label: String,
    /// Whether the menu is enabled. Defaults to `true`.
    #[serde(default = "default_menu_enabled")]
    pub enabled: bool,
    /// The menu's entries, in display order.
    pub items: Vec<MenuEntryConfig>,
}

/// One nested native submenu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuSubmenuConfig {
    /// The label shown to the user.
    pub label: String,
    /// Whether the submenu is enabled. Defaults to `true`.
    #[serde(default = "default_menu_enabled")]
    pub enabled: bool,
    /// The submenu's entries, in display order.
    pub items: Vec<MenuEntryConfig>,
}

/// One entry inside a native menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MenuEntryConfig {
    /// A command delivered to the page when selected.
    Item {
        /// The label shown to the user.
        label: String,
        /// The command name delivered in the `menu` event.
        command: String,
        /// Whether the item is enabled. Defaults to `true`.
        #[serde(default = "default_menu_enabled")]
        enabled: bool,
    },
    /// A nested menu.
    Submenu(MenuSubmenuConfig),
    /// A visual separator.
    Separator,
}

const fn default_menu_enabled() -> bool {
    true
}

#[derive(Default)]
struct MenuValidator<'a> {
    entries: usize,
    claimed: Vec<&'a str>,
}

impl<'a> MenuValidator<'a> {
    const MAX_ENTRIES: usize = 256;
    const MAX_DEPTH: usize = 8;
    const MAX_LABEL_BYTES: usize = 128;
    const MAX_COMMAND_BYTES: usize = 64;

    fn check_submenu(
        &mut self,
        label: &str,
        items: &'a [MenuEntryConfig],
        field: &str,
        depth: usize,
    ) -> Result<(), ManifestError> {
        check_menu_label(label, &format!("{field}.label"), Self::MAX_LABEL_BYTES)?;
        if items.is_empty() {
            return Err(ManifestError::Invalid {
                field: format!("{field}.items"),
                reason: "must contain at least one entry".to_owned(),
            });
        }
        self.check_items(items, field, depth)
    }

    fn check_items(
        &mut self,
        items: &'a [MenuEntryConfig],
        field: &str,
        depth: usize,
    ) -> Result<(), ManifestError> {
        for (index, entry) in items.iter().enumerate() {
            self.entries += 1;
            if self.entries > Self::MAX_ENTRIES {
                return Err(ManifestError::Invalid {
                    field: field.to_owned(),
                    reason: format!(
                        "menu trees may contain at most {} entries",
                        Self::MAX_ENTRIES
                    ),
                });
            }
            let field = format!("{field}.items[{index}]");
            match entry {
                MenuEntryConfig::Item {
                    label,
                    command,
                    enabled: _,
                } => {
                    check_menu_label(label, &format!("{field}.label"), Self::MAX_LABEL_BYTES)?;
                    if !is_menu_command(command) {
                        return Err(ManifestError::Invalid {
                            field: format!("{field}.command"),
                            reason: format!(
                                "must start with a lowercase letter and contain only lowercase \
                                 letters, digits, `.`, `_` or `-`, without the reserved `ncui.` \
                                 prefix (max {} bytes); got `{command}`",
                                Self::MAX_COMMAND_BYTES
                            ),
                        });
                    }
                    if let Some(duplicate) = self.claimed.iter().find(|name| **name == command) {
                        return Err(ManifestError::Invalid {
                            field: format!("{field}.command"),
                            reason: format!(
                                "duplicate command `{duplicate}`: a menu selection must have one \
                                 meaning regardless of which submenu contains it"
                            ),
                        });
                    }
                    self.claimed.push(command);
                }
                MenuEntryConfig::Submenu(submenu) => {
                    if depth >= Self::MAX_DEPTH {
                        return Err(ManifestError::Invalid {
                            field,
                            reason: format!("menus may be at most {} levels deep", Self::MAX_DEPTH),
                        });
                    }
                    self.check_submenu(&submenu.label, &submenu.items, &field, depth + 1)?;
                }
                MenuEntryConfig::Separator => {}
            }
        }
        Ok(())
    }
}

fn check_menu_label(label: &str, field: &str, max_bytes: usize) -> Result<(), ManifestError> {
    if label.trim().is_empty() || label.len() > max_bytes || label.chars().any(char::is_control) {
        return Err(ManifestError::Invalid {
            field: field.to_owned(),
            reason: format!(
                "must be visible text without control characters, at most {max_bytes} bytes; got \
                 `{label}`"
            ),
        });
    }
    Ok(())
}

fn is_menu_command(command: &str) -> bool {
    let mut bytes = command.bytes();
    bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && !command.starts_with("ncui.")
        && command.len() <= MenuValidator::MAX_COMMAND_BYTES
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

/// One sidecar command an application declares.
///
/// The contract between the runtime and the program is a line of JSON each way: the runtime
/// writes the call's payload to the program's standard input and reads one JSON answer from
/// its standard output. Anything that can read stdin and write stdout can be a backend,
/// in whatever language the author likes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarConfig {
    /// The command name the page calls, such as `search.index`.
    ///
    /// Validated like any other command name, so it cannot collide with a built-in or carry
    /// something that is not a name.
    pub command: String,
    /// The program to run: a path inside the application, or a bare name the operating system
    /// resolves.
    ///
    /// The same containment rule the `process` capability uses applies — a relative path is
    /// resolved inside the application, an absolute one is refused — so a sidecar cannot name
    /// an arbitrary file elsewhere on the disk.
    pub program: String,
    /// The arguments passed to the program, as an argument vector and never through a shell.
    #[serde(default)]
    pub args: Vec<String>,
    /// The capability a page must hold to call this command.
    ///
    /// Empty means any page of the application may call it; otherwise only a page granted this
    /// capability can. The call is gated at the seam either way, so declaring a capability here
    /// is how an author exposes a powerful backend only to a page that was granted it.
    #[serde(default)]
    pub requires: CapabilitySet,
    /// How long the program may run, in milliseconds, before it is stopped.
    ///
    /// Absent uses the `process` runner's own default. A sidecar is a program the runtime
    /// starts on a page's behalf, so it is timed like one: a backend that never answers must
    /// not hang the page that called it. For a `stream` sidecar this is the bound on one
    /// call's answer, not on the program's lifetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Whether the program runs once per call or stays running across calls.
    ///
    /// Absent is `oneshot`: the program starts, answers one JSON document on standard
    /// output, and exits — the shape for a stateless tool. `stream` starts the program
    /// once, on the first call, and keeps it running: each call writes one newline-
    /// delimited JSON line to its standard input and reads the answer line that names the
    /// call's `id`, so a backend with start-up cost — a model, a connection, a warm index —
    /// pays it once. The program is stopped when the runtime stops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SidecarMode>,
    /// One line describing what the command does, shown by `describe`.
    #[serde(default)]
    pub summary: String,
}

/// Whether a sidecar runs once per call or stays running across calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidecarMode {
    /// Start the program per call; one JSON document in, one out (the default).
    #[default]
    Oneshot,
    /// Start the program once and answer one NDJSON line per call.
    Stream,
}

/// One command an application declares the page answers in JavaScript.
///
/// The command is gated exactly like a native one: it goes in the action table pages are
/// checked against, and a page must hold `requires` to call it. What the runtime does with a
/// call is hand it to the page's own handler rather than run a native function, so a
/// JavaScript-only application gets the same seam, the same capability check and the same
/// audit trail as a command the runtime answers itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageCommandConfig {
    /// The command name the page calls, such as `cart.total`.
    ///
    /// Validated like any other command name, so it cannot collide with a built-in or carry
    /// something that is not a name.
    pub command: String,
    /// The capability a page must hold to call this command.
    ///
    /// Empty means any page of the application may call it; otherwise only a page granted this
    /// capability can. The call is gated at the seam either way, so declaring a capability here
    /// is how an author exposes a command only to a page that was granted it.
    #[serde(default)]
    pub requires: CapabilitySet,
    /// One line describing what the command does, shown by `describe`.
    #[serde(default)]
    pub summary: String,
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

/// Whether `tag` has the shape of a BCP 47 language tag.
///
/// The shape only: whether a tag names a real language is a question for the renderer, and a
/// tag this accepts but the platform does not know degrades to left-to-right rather than
/// breaking. A tag is letters (the language, then optional script/region/variant subtags)
/// separated by `-` or `_`, each subtag one to eight alphanumeric characters. This refuses an
/// empty tag and one carrying a path separator or whitespace, which would be a value that is
/// not a locale pretending to be one.
pub fn is_locale_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 35
        && tag.split(['-', '_']).all(|subtag| {
            !subtag.is_empty()
                && subtag.len() <= 8
                && subtag
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
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

    /// Checks the file extensions the application claims.
    ///
    /// An association reaches the platform's extension registry, so an extension that
    /// cannot be one is refused here rather than at registration time.
    fn check_associations(&self) -> Result<(), ManifestError> {
        for association in &self.associations {
            let trimmed = association.trim().trim_start_matches('.');
            if trimmed.is_empty()
                || trimmed.len() > 32
                || !trimmed
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                return Err(ManifestError::Invalid {
                    field: "associations".to_owned(),
                    reason: format!(
                        "must be a file extension — lowercase letters, digits and hyphens, \
                         at most 32 bytes — got `{association}`"
                    ),
                });
            }
        }
        Ok(())
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
        if let Some(locale) = &self.default_locale
            && !is_locale_tag(locale)
        {
            return Err(ManifestError::Invalid {
                field: "default_locale".to_owned(),
                reason: format!(
                    "must be a BCP 47 language tag — letters separated by `-` or `_`, \
                     e.g. `en`, `pt-BR`, `he-IL` — got `{locale}`"
                ),
            });
        }
        self.check_associations()?;
        self.check_tray()?;
        self.check_shortcuts()?;
        self.check_menus()?;
        self.check_sidecars()?;
        self.check_page_commands()?;
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

    /// Checks the native menus the application declares.
    ///
    /// Labels, depth and total entry count are bounded before a platform sees them, and command
    /// names use the bridge's action-name shape. Names are unique across the whole menu tree so a
    /// selection has exactly one meaning regardless of which submenu contains it.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] for an unusable label, command, or menu tree.
    fn check_menus(&self) -> Result<(), ManifestError> {
        const MAX_MENUS: usize = 32;

        if self.menus.len() > MAX_MENUS {
            return Err(ManifestError::Invalid {
                field: "menus".to_owned(),
                reason: format!("must contain at most {MAX_MENUS} top-level menus"),
            });
        }
        let mut validation = MenuValidator::default();
        for (index, menu) in self.menus.iter().enumerate() {
            validation.check_submenu(&menu.label, &menu.items, &format!("menus[{index}]"), 1)?;
        }
        Ok(())
    }

    /// Checks the sidecar commands the application declares.
    ///
    /// A sidecar's *behaviour* is checked where it runs — the `process` runner contains the
    /// program path, scrubs the environment and times the run — because that lives in the crate
    /// that starts programs. What is checked here is the declaration's shape: a sidecar must name
    /// the command a page would call and the program that answers it, and no two sidecars may
    /// claim the same command name, or one would silently shadow the other.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] for a sidecar with no command, no program, or a command
    /// name another sidecar already claimed.
    fn check_sidecars(&self) -> Result<(), ManifestError> {
        let mut claimed: Vec<&str> = Vec::new();
        for (index, sidecar) in self.sidecars.iter().enumerate() {
            if sidecar.command.trim().is_empty() {
                return Err(ManifestError::Invalid {
                    field: format!("sidecars[{index}].command"),
                    reason: "must not be empty".to_owned(),
                });
            }
            if sidecar.program.trim().is_empty() {
                return Err(ManifestError::Invalid {
                    field: format!("sidecars[{index}].program"),
                    reason: "must not be empty; a sidecar names the program that answers it"
                        .to_owned(),
                });
            }
            if let Some(duplicate) = claimed.iter().find(|name| **name == sidecar.command) {
                return Err(ManifestError::Invalid {
                    field: format!("sidecars[{index}].command"),
                    reason: format!(
                        "duplicate command `{duplicate}`: two sidecars answer the same name, so \
                         one would shadow the other"
                    ),
                });
            }
            claimed.push(&sidecar.command);
        }
        Ok(())
    }

    /// Checks the commands the application declares the page answers in JavaScript.
    ///
    /// The declaration's shape is what is checked here: a page command must name the command a
    /// page would call, and no two commands — page or sidecar — may answer the same name. A
    /// collision with a *built-in* is caught where the command is registered, against the same
    /// action table the page bridge enforces; this check catches the ones this file can see.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] for a page command with no name, or a name another
    /// page command or sidecar already claimed.
    fn check_page_commands(&self) -> Result<(), ManifestError> {
        let mut claimed: Vec<&str> = self.sidecars.iter().map(|s| s.command.as_str()).collect();
        for (index, command) in self.page_commands.iter().enumerate() {
            if command.command.trim().is_empty() {
                return Err(ManifestError::Invalid {
                    field: format!("page_commands[{index}].command"),
                    reason: "must not be empty".to_owned(),
                });
            }
            if let Some(duplicate) = claimed.iter().find(|name| **name == command.command) {
                return Err(ManifestError::Invalid {
                    field: format!("page_commands[{index}].command"),
                    reason: format!(
                        "duplicate command `{duplicate}`: two commands answer the same name, so \
                         one would shadow the other"
                    ),
                });
            }
            claimed.push(&command.command);
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
    fn a_default_locale_is_validated_as_a_language_tag() {
        // A locale decides the text direction an interface lays out in, so a manifest that
        // declares one must declare a language tag rather than a value that is not one.
        let base = AppManifest::parse(
            r#"{"name":"counter","version":"1.0.0"}"#,
            Path::new("ncui.app.json"),
        )
        .expect("a minimal manifest");
        assert!(is_locale_tag("en"));
        assert!(is_locale_tag("pt-BR"));
        assert!(is_locale_tag("he-IL"));
        assert!(is_locale_tag("zh_Hans"));
        assert!(!is_locale_tag(""));
        assert!(!is_locale_tag("en US"), "a space is not a subtag separator");
        assert!(!is_locale_tag("en.US"), "a dot is a codeset, not a tag");
        assert!(!is_locale_tag("../etc"), "a locale is not a path");
        assert!(!is_locale_tag("toolongsubtag"));

        let mut manifest = AppManifest {
            default_locale: Some("he-IL".to_owned()),
            ..base
        };
        assert!(manifest.validate().is_ok());
        manifest.default_locale = Some("not a locale".to_owned());
        let error = manifest.validate().expect_err("not a locale tag");
        assert!(error.to_string().contains("default_locale"), "{error}");
    }

    #[test]
    fn a_manifest_declaring_a_default_locale_parses_it() {
        // The declared locale round-trips through the manifest, so an application that names
        // its interface language has it honoured rather than dropped on the floor.
        let manifest = AppManifest::parse(
            r#"{"name":"counter","version":"1.0.0","default_locale":"he"}"#,
            Path::new("ncui.app.json"),
        )
        .expect("a manifest with a default locale");
        assert_eq!(manifest.default_locale.as_deref(), Some("he"));
        // Absent stays absent: an application that declares none falls back to the platform.
        let bare = AppManifest::parse(
            r#"{"name":"counter","version":"1.0.0"}"#,
            Path::new("ncui.app.json"),
        )
        .expect("a minimal manifest");
        assert_eq!(bare.default_locale, None);
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
    fn native_menus_are_parsed_with_nested_entries_and_enabled_defaults() {
        let parsed = manifest(
            r#"{
                "name":"a","version":"1.0.0","menus":[
                    {"label":"File","items":[
                        {"type":"item","label":"Increment","command":"increment"},
                        {"type":"separator"},
                        {"type":"submenu","label":"More","enabled":false,"items":[
                            {"type":"item","label":"Summary","command":"counter.summary","enabled":false}
                        ]}
                    ]}
                ]
            }"#,
        )
        .expect("a manifest with native menus");
        assert_eq!(parsed.menus.len(), 1);
        assert!(parsed.menus[0].enabled);
        let MenuEntryConfig::Submenu(submenu) = &parsed.menus[0].items[2] else {
            panic!("the third entry is a submenu");
        };
        assert!(!submenu.enabled);
        let MenuEntryConfig::Item { enabled, .. } = &submenu.items[0] else {
            panic!("the submenu contains an item");
        };
        assert!(!enabled);

        let plain = manifest(r#"{"name":"a","version":"1.0.0"}"#).expect("a minimal manifest");
        assert!(plain.menus.is_empty());
    }

    #[test]
    fn malformed_native_menus_are_refused() {
        for raw in [
            r#"{"name":"a","version":"1.0.0","menus":[{"label":"","items":[{"type":"separator"}]}]}"#,
            r#"{"name":"a","version":"1.0.0","menus":[{"label":"File","items":[]}]}"#,
            r#"{"name":"a","version":"1.0.0","menus":[{"label":"File","items":[{"type":"item","label":"Bad\nLabel","command":"go"}]}]}"#,
            r#"{"name":"a","version":"1.0.0","menus":[{"label":"File","items":[{"type":"item","label":"Bad","command":"ncui.method"}]}]}"#,
            r#"{"name":"a","version":"1.0.0","menus":[{"label":"File","items":[{"type":"item","label":"Bad","command":"Upper"}]}]}"#,
            r#"{"name":"a","version":"1.0.0","menus":[{"label":"File","items":[{"type":"unknown"}]}]}"#,
        ] {
            let error = manifest(raw).expect_err("a malformed menu is refused");
            assert!(
                matches!(
                    error,
                    ManifestError::Invalid { .. } | ManifestError::Malformed { .. }
                ),
                "{error}"
            );
        }
    }

    #[test]
    fn a_native_menu_command_has_one_meaning_across_the_whole_tree() {
        let error = manifest(
            r#"{"name":"a","version":"1.0.0","menus":[
                {"label":"File","items":[{"type":"item","label":"One","command":"dup"}]},
                {"label":"Edit","items":[{"type":"item","label":"Two","command":"dup"}]}
            ]}"#,
        )
        .expect_err("duplicate menu commands are refused");
        assert!(error.to_string().contains("dup"), "{error}");
    }

    #[test]
    fn native_menu_depth_and_size_are_bounded() {
        let mut entry = serde_json::json!({"type":"item","label":"Leaf","command":"leaf"});
        for _ in 0..8 {
            entry = serde_json::json!({"type":"submenu","label":"Nested","items":[entry]});
        }
        let raw = serde_json::json!({
            "name":"a","version":"1.0.0","menus":[{"label":"File","items":[entry]}]
        });
        let error = AppManifest::parse(&raw.to_string(), Path::new("ncui.app.json"))
            .expect_err("an over-deep menu is refused");
        assert!(error.to_string().contains("deep"), "{error}");

        let items: Vec<_> = (0..257)
            .map(|index| {
                serde_json::json!({
                    "type":"item","label":format!("Item {index}"),"command":format!("command_{index}")
                })
            })
            .collect();
        let raw = serde_json::json!({
            "name":"a","version":"1.0.0","menus":[{"label":"File","items":items}]
        });
        let error = AppManifest::parse(&raw.to_string(), Path::new("ncui.app.json"))
            .expect_err("an oversized menu is refused");
        assert!(error.to_string().contains("256"), "{error}");
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

    #[test]
    fn a_sidecar_declared_in_the_manifest_is_parsed_with_its_contract() {
        // The author-facing half of a sidecar is the manifest entry: the author writes the
        // command a page calls, the program that answers it, and the capability that gates it.
        // This drives a real `ncui.app.json` through the loader rather than building the
        // struct in Rust, so the declared shape is what the loader accepts.
        let parsed = manifest(
            r#"{
                "name": "searcher",
                "version": "1.0.0",
                "capabilities": ["process.spawn", "filesystem.read"],
                "sidecars": [{
                    "command": "search.index",
                    "program": "tools/index.py",
                    "args": ["--quiet"],
                    "requires": ["filesystem.read"],
                    "timeout_ms": 2500,
                    "summary": "build the search index"
                }]
            }"#,
        )
        .expect("a manifest declaring a sidecar");
        assert_eq!(parsed.sidecars.len(), 1, "the sidecar is declared");
        let sidecar = &parsed.sidecars[0];
        assert_eq!(sidecar.command, "search.index");
        assert_eq!(sidecar.program, "tools/index.py");
        assert_eq!(sidecar.args, vec!["--quiet".to_owned()]);
        assert!(
            sidecar.requires.contains(Capability::FilesystemRead),
            "the declared capability gates the command"
        );
        assert_eq!(sidecar.timeout_ms, Some(2500));
        assert_eq!(sidecar.summary, "build the search index");
    }

    #[test]
    fn a_manifest_with_no_sidecars_declares_none() {
        let parsed = manifest(r#"{"name":"a","version":"1.0.0"}"#).expect("a manifest");
        assert!(
            parsed.sidecars.is_empty(),
            "a sidecar-less application declares no sidecars"
        );
    }

    #[test]
    fn a_sidecar_with_no_command_or_no_program_is_refused() {
        for raw in [
            r#"{"name":"a","version":"1.0.0","sidecars":[{"command":"","program":"x.py"}]}"#,
            r#"{"name":"a","version":"1.0.0","sidecars":[{"command":"c","program":"  "}]}"#,
        ] {
            let error = manifest(raw).expect_err("a sidecar with a missing name");
            assert!(
                matches!(error, ManifestError::Invalid { .. }),
                "got: {error}"
            );
        }
    }

    #[test]
    fn two_sidecars_may_not_answer_the_same_command() {
        let error = manifest(
            r#"{"name":"a","version":"1.0.0","sidecars":[
                {"command":"dup","program":"one.py"},
                {"command":"dup","program":"two.py"}]}"#,
        )
        .expect_err("a duplicate sidecar command");
        assert!(
            matches!(error, ManifestError::Invalid { .. }),
            "got: {error}"
        );
        assert!(
            error.to_string().contains("dup"),
            "the refusal names the command two sidecars claimed; got: {error}"
        );
    }

    #[test]
    fn a_page_command_declared_in_the_manifest_is_parsed_with_its_gate() {
        // The JavaScript half of a command is declared here, not registered by the page: the
        // author names the command and the capability that gates it, so the command sits in the
        // action table pages are checked against exactly like a native one. This drives a real
        // `ncui.app.json` through the loader.
        let parsed = manifest(
            r#"{
                "name": "cart",
                "version": "1.0.0",
                "capabilities": ["inspect.runtime"],
                "page_commands": [{
                    "command": "cart.total",
                    "requires": ["inspect.runtime"],
                    "summary": "total the cart"
                }]
            }"#,
        )
        .expect("a manifest declaring a page command");
        assert_eq!(
            parsed.page_commands.len(),
            1,
            "the page command is declared"
        );
        let command = &parsed.page_commands[0];
        assert_eq!(command.command, "cart.total");
        assert_eq!(command.summary, "total the cart");
        assert!(
            command.requires.contains(Capability::InspectRuntime),
            "the declared capability gates the command"
        );
    }

    #[test]
    fn a_manifest_with_no_page_commands_declares_none() {
        let parsed = manifest(r#"{"name":"a","version":"1.0.0"}"#).expect("a manifest");
        assert!(
            parsed.page_commands.is_empty(),
            "an application that declares none has none"
        );
    }

    #[test]
    fn a_page_command_with_no_name_is_refused() {
        let error = manifest(
            r#"{"name":"a","version":"1.0.0","page_commands":[{"command":"","summary":"x"}]}"#,
        )
        .expect_err("a page command with no name");
        assert!(
            matches!(error, ManifestError::Invalid { .. }),
            "got: {error}"
        );
    }

    #[test]
    fn a_page_command_may_not_answer_the_name_a_sidecar_claims() {
        // A sidecar and a page command both become commands the page calls, registered in the
        // same action table. A name two of them claim would shadow, so it is refused here where
        // the author can see it rather than at registration.
        let error = manifest(
            r#"{"name":"a","version":"1.0.0",
                "sidecars":[{"command":"dup","program":"x.py"}],
                "page_commands":[{"command":"dup","summary":"x"}]}"#,
        )
        .expect_err("a page command shadowing a sidecar");
        assert!(
            matches!(error, ManifestError::Invalid { .. }),
            "got: {error}"
        );
        assert!(
            error.to_string().contains("dup"),
            "the refusal names the contested command; got: {error}"
        );
    }
}
