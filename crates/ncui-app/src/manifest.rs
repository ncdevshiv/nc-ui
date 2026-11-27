//! The application manifest: what an application says about itself.
//!
//! An application is a directory with a manifest in it. The manifest names the application,
//! what its root window opens, the assets it serves, the window it opens in, and the
//! capabilities its pages are granted. It is the one file that turns a directory of files
//! into something a runtime can run, and it is the file an author edits — so every field in
//! it has to mean something the runtime actually does.
//!
//! What the root window opens is one of two kinds, because ADR-020 made content two kinds:
//! a page the assets server serves (`"entry": "index.html"`, the only kind there was), or a
//! native view a registered provider builds (`"entry": {"kind": "native", "provider": …}`).
//! The string form means exactly what it always meant, so a manifest written before native
//! entries existed is still read the way its author wrote it.
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

/// The longest a native entry's provider identifier may be, in bytes.
///
/// A coarse copy of the bound the window contract enforces exactly: ncui-webview's
/// `MAX_PROVIDER_ID_BYTES` is the single source of truth, and it is checked again when the
/// window opens — along with the character set, which is deliberately not checked here.
/// The number is restated rather than imported because this crate does not depend on the
/// window contract crate, and the restatement is safe precisely because the authoritative
/// check is downstream: a disagreement would cost an earlier error message, never a wrong
/// acceptance.
const MAX_ENTRY_PROVIDER_BYTES: usize = 128;

/// What an application's root window opens.
///
/// A window's content is one of two kinds (ADR-020), and the kind travels inside the entry
/// rather than beside it — the same decision the window contract makes with `WindowContent`
/// — so everything that reads the entry sees the whole of what was asked for in one place.
/// Untagged because a page entry was always a bare string: `"entry": "index.html"` parses
/// as [`EntryPoint::Page`] byte-for-byte the way it did before native entries existed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EntryPoint {
    /// A page the assets server serves, relative to the assets directory.
    Page(String),
    /// A native view a registered provider builds.
    Native(NativeEntryPoint),
}

impl core::fmt::Display for EntryPoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The entry as one line of text, for metadata that carries a string — the bundle
        // manifest is the consumer. A page entry is its path, byte-identical to what a
        // string entry always produced; a native entry is its compact JSON object, which
        // loses nothing and cannot be mistaken for a path. Written piecewise rather than
        // through `serde_json::to_string` so the conversion cannot fail: the keys are
        // static and the strings encode through `Value`'s own escaping.
        match self {
            Self::Page(page) => f.write_str(page),
            Self::Native(native) => write!(
                f,
                "{{\"kind\":{},\"provider\":{},\"props\":{}}}",
                serde_json::Value::String(native.kind.clone()),
                serde_json::Value::String(native.provider.clone()),
                native.props
            ),
        }
    }
}

/// A native entry: the root window's content is host-linked Rust built by a provider,
/// not a page served to a webview.
///
/// The shape is the window-create contract's content block, so the two ways a native
/// window comes to exist — declared here, or opened by a page through the bridge — are
/// described in one vocabulary. Unknown fields are refused because the object is authored
/// by hand: `props` misspelt would be props silently dropped to the default, and a
/// manifest that forgives a typo is a manifest whose author never learns of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEntryPoint {
    /// The content kind: exactly the string `"native"`.
    ///
    /// Explicit rather than inferred from the object's shape, so a mistagged object is a
    /// mistake the validation refuses by name instead of a page nobody meant to write.
    pub kind: String,
    /// The provider the view is built by, namespaced — e.g. `nc-ui-kit::settings-tabs`.
    pub provider: String,
    /// The props the view is built with. Absent means the provider's own defaults.
    #[serde(default)]
    pub props: serde_json::Value,
}

/// An application's own description of itself.
///
/// `PartialEq` but not `Eq`: a native entry carries `serde_json::Value` props, and a JSON
/// number has no total equality.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppManifest {
    /// The application's name.
    pub name: String,
    /// The application's version.
    pub version: String,
    /// What the application's root window opens: a page the assets server serves, or a
    /// native view a registered provider builds (ADR-020).
    ///
    /// A string is a page path relative to the assets directory, exactly as it has always
    /// been. An object is a native entry — `"entry": {"kind": "native", "provider":
    /// "nc-ui-kit::settings-tabs", "props": {…}}` — in the same shape the window-create
    /// contract carries its content block, so a window the manifest declares and a window
    /// a page opens are described in one vocabulary.
    #[serde(default = "default_entry")]
    pub entry: EntryPoint,
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

/// The entry an application opens, when the manifest does not say: the index page, exactly
/// as it has always been.
fn default_entry() -> EntryPoint {
    EntryPoint::Page("index.html".to_owned())
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
        // What the entry must satisfy depends on its kind. A page entry is a path the
        // assets server must be able to serve, so it is non-empty here and checked against
        // the relative-path rules with the other paths below. A native entry names no file,
        // so its checks are the object's own, in `check_native_entry`.
        let page = match &self.entry {
            EntryPoint::Page(page) => {
                if page.trim().is_empty() {
                    return Err(ManifestError::Invalid {
                        field: "entry".to_owned(),
                        reason: "must not be empty".to_owned(),
                    });
                }
                page.as_str()
            }
            EntryPoint::Native(native) => {
                Self::check_native_entry(native)?;
                // No path to check below: the loop skips empty values, and a native entry
                // has no path because there is nothing to serve.
                ""
            }
        };
        // The entry, the assets directory and the icon are all relative paths that will be joined
        // onto the application directory. A path that is absolute, or that climbs out with
        // `..`, would name a file outside the application — so they are refused here rather
        // than at the point of use.
        let icon = self.window.icon.as_deref().unwrap_or_default();
        for (field, value) in [
            ("entry", page),
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

    /// Checks the object half of a native entry.
    ///
    /// What is checked here is only what the window contract cannot see: that the object
    /// is tagged `native` at all, because an object entry is a native view or a mistake,
    /// and that the provider is a non-empty bounded string. The provider's character set
    /// and the props' size are deliberately *not* checked here: `WindowRequest::validate`
    /// in ncui-webview enforces them when the window opens, it is the single source of
    /// truth for what a native window may carry, and a second copy of its rules here
    /// would be two answers drifting apart.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] when the kind is not exactly `native` or the
    /// provider is empty or over the byte bound.
    fn check_native_entry(native: &NativeEntryPoint) -> Result<(), ManifestError> {
        if native.kind != "native" {
            return Err(ManifestError::Invalid {
                field: "entry.kind".to_owned(),
                reason: format!(
                    "must be \"native\"; got `{}` — an object entry is a native view or a \
                     mistake, and a mistake should surface where it was made",
                    native.kind
                ),
            });
        }
        if native.provider.is_empty() || native.provider.len() > MAX_ENTRY_PROVIDER_BYTES {
            return Err(ManifestError::Invalid {
                field: "entry.provider".to_owned(),
                reason: format!(
                    "must be a non-empty provider identifier of at most \
                     {MAX_ENTRY_PROVIDER_BYTES} bytes; got `{}`",
                    native.provider
                ),
            });
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
    /// A page concept only: a native entry names no file — its content is built by a
    /// provider rather than served — so asking for its path is a programming error, and
    /// the answer is an honest refusal rather than an invented path.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] when the entry is native, or when a page entry
    /// names a path that cannot be resolved inside the assets directory.
    pub fn entry_path(&self, application_directory: &Path) -> Result<PathBuf, ManifestError> {
        let EntryPoint::Page(page) = &self.entry else {
            return Err(ManifestError::Invalid {
                field: "entry".to_owned(),
                reason: "a native entry has no asset path: its content is built by a \
                         provider, not served from a file"
                    .to_owned(),
            });
        };
        let assets = self.assets_directory(application_directory);
        ncui_security::path::SafePath::resolve(&assets, page).map_err(|error| {
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
    /// A page concept only, for the same reason as [`AppManifest::entry_path`]: a native
    /// entry has no URL because nothing is fetched. The path is percent-encoded per
    /// component, so a name with a space in it becomes a URL a browser can fetch.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Invalid`] when the entry is native.
    pub fn entry_url_path(&self) -> Result<String, ManifestError> {
        let EntryPoint::Page(page) = &self.entry else {
            return Err(ManifestError::Invalid {
                field: "entry".to_owned(),
                reason: "a native entry has no URL path: its content is built by a \
                         provider, not served from a file"
                    .to_owned(),
            });
        };
        let mut path = String::from("/");
        for (index, component) in page.split(['/', '\\']).enumerate() {
            if index > 0 {
                path.push('/');
            }
            path.push_str(&percent_encode(component));
        }
        Ok(path)
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

