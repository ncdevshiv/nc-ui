//! Native file, save and folder dialogs: the platform's own picker, behind a capability.
//!
//! A page that needs a file the user holds — a document to open, a place to save — cannot reach
//! outside its own directory, and should not have to. A native dialog puts the platform's own
//! picker in front of the person using the application and hands back the path *they* chose. The
//! person is the authority: they browsed to it, so the application learns a path a human selected
//! rather than one it guessed at.
//!
//! # What the platform does and what this does
//!
//! The platform owns the picker: it is the operating system that draws the dialog, walks the
//! filesystem, and returns what the person picked. This module owns the vocabulary — which kinds
//! of dialog an application may ask for, which filters it may name — and every rule about what a
//! request and a returned path must look like. Those rules are plain Rust and are tested without a
//! display, which is the part that can be.
//!
//! # Why the returned path is validated
//!
//! The path a dialog returns is *input*, exactly like a payload a page posted: it came from outside
//! this process and is about to be handed to one. [`validate_chosen_path`] therefore bounds its
//! length and refuses control characters, so a path can be neither a line break in something a
//! person reads nor a carrier of a control code. The path is not a command line and never becomes
//! one, so it needs no quoting check — but it is bounded and character-checked for the same reason
//! any untrusted string is.
//!
//! # What a dialog does not grant
//!
//! Learning a path is not reading the file at it. Opening the file still needs
//! [`Capability::FilesystemRead`](ncui_core::Capability::FilesystemRead), and that read is still
//! contained to the application's own directory — a dialog reaches *outside* that directory by
//! design, because the person chose the file, so the two capabilities answer different questions
//! and neither implies the other. This module returns the path; it does not open, read or write it.
//!
//! # The modal, and what a test can and cannot reach
//!
//! A dialog is *modal*: [`DialogBackend::show`] and the platform adapter behind it block until a
//! person answers.
//! That is the honest shape of the thing, and it means the single real open — a person clicking a
//! picker and choosing a file — is the one step a test cannot drive, because a test has no person.
//! Everything around it is tested here: the request vocabulary, the option and filter rules, the
//! validation of what comes back, and the refusals. The one real modal open is recorded in
//! `docs/known-gaps.md` as a one-time human verification rather than claimed as automated, which is
//! the same standard the rest of this capability surface holds.
//!
//! # The dependency
//!
//! `rfd` is ADR-012's eighth exception: the platform's own dialog is an `IFileOpenDialog`, an
//! `NSOpenPanel` or a GTK file chooser, all foreign functions, and `unsafe_code = "forbid"`
//! rejects calling them. `rfd` performs that FFI internally, so this project's own code stays free
//! of `unsafe`. It is behind the `dialogs` feature because a modal picker needs a person, which a
//! headless machine does not have.

/// Which kind of picker an application may ask for.
///
/// The set is closed and small on purpose: each kind is one platform dialog this module can
/// describe and refuse by name, rather than an open-ended string a caller could spell wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    /// Choose one existing file.
    OpenFile,
    /// Choose one or more existing files.
    OpenFiles,
    /// Choose where to save a file, which may name a file that does not exist yet.
    SaveFile,
    /// Choose one existing directory.
    PickFolder,
}

impl DialogKind {
    /// The stable string form, e.g. `"open.file"`, used on the wire and in refusals.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenFile => "open.file",
            Self::OpenFiles => "open.files",
            Self::SaveFile => "save.file",
            Self::PickFolder => "pick.folder",
        }
    }
}

/// One file-type filter a dialog may offer: a label and the extensions it matches.
///
/// A filter is what the picker's "files of type" list shows. The label is for the person; the
/// extensions are what the platform matches against, and they are validated here so an
/// application cannot name an extension that is not one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFilter {
    /// What the picker shows for this filter, e.g. `"Text files"`.
    pub name: String,
    /// The extensions this filter matches, without the leading dot, e.g. `["txt", "md"]`.
    pub extensions: Vec<String>,
}

/// The longest dialog title this module will pass to the platform. Named distinctly from
/// [`crate::notify::MAX_TITLE_BYTES`], which bounds a notification's title: the two are different
/// surfaces with different limits, and a re-export of both into `lib.rs` would collide.
pub const MAX_DIALOG_TITLE_BYTES: usize = 256;

/// The longest extension this module accepts, and the longest a returned path may be.
pub const MAX_EXTENSION_BYTES: usize = 32;
pub const MAX_PATH_BYTES: usize = 4096;

/// Why a dialog could not be asked for, shown, or answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogError {
    /// The request is malformed: an empty or over-long title, a filter with no extensions, an
    /// extension that is not one, or a default path carrying a control character.
    RefusedRequest {
        /// Which part of the request was refused.
        field: &'static str,
        /// Why.
        reason: String,
    },
    /// The person cancelled the dialog, or closed it without choosing.
    Cancelled,
    /// This host cannot show a dialog at all — no display, or the `dialogs` feature is off.
    Unavailable {
        /// What the platform said, when it said anything.
        reason: String,
    },
    /// The platform showed the dialog and could not complete it.
    Failed {
        /// What the platform said.
        reason: String,
    },
    /// The path the dialog returned is not one this module will hand back.
    RefusedPath {
        /// The path, truncated.
        path: String,
        /// Why.
        reason: String,
    },
}

impl DialogError {
    /// The stable code a caller sees on the wire.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::RefusedRequest { .. } => "dialog_request_refused",
            Self::Cancelled => "dialog_cancelled",
            Self::Unavailable { .. } => "dialog_unavailable",
            Self::Failed { .. } => "dialog_failed",
            Self::RefusedPath { .. } => "dialog_path_refused",
        }
    }

    /// Whether this is the ordinary "the person cancelled" outcome rather than a failure.
    ///
    /// A cancel is not an error a caller should log as one: it is the person deciding against the
    /// action, which is a normal answer to a modal question.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

impl core::fmt::Display for DialogError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RefusedRequest { field, reason } => {
                write!(f, "the dialog request's {field} is refused: {reason}")
            }
            Self::Cancelled => f.write_str("the dialog was cancelled"),
            Self::Unavailable { reason } => {
                write!(f, "this host cannot show a dialog: {reason}")
            }
            Self::Failed { reason } => write!(f, "the dialog could not be completed: {reason}"),
            Self::RefusedPath { path, reason } => {
                write!(f, "the chosen path `{path}` is refused: {reason}")
            }
        }
    }
}

impl std::error::Error for DialogError {}

/// What a dialog returned: the path or paths the person chose.
///
/// A [`DialogKind::OpenFile`], [`DialogKind::SaveFile`] and [`DialogKind::PickFolder`] yield one
/// path; [`DialogKind::OpenFiles`] yields one or more. The distinction is carried by the kind the
/// caller asked for, so this is a plain list and the caller reads it according to what it asked.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DialogOutcome {
    /// The chosen paths, in the order the platform returned them.
    pub paths: Vec<String>,
}

impl DialogOutcome {
    /// The single path this outcome holds, or an error if it holds none or more than one.
    ///
    /// A caller that asked for one file wants one path and should say so rather than silently
    /// taking the first of several.
    ///
    /// # Errors
    ///
    /// [`DialogError::RefusedPath`] when the outcome holds no path or more than one, which would
    /// mean the platform returned something the caller did not ask for.
    pub fn single(&self) -> Result<&str, DialogError> {
        if self.paths.len() == 1 {
            return Ok(&self.paths[0]);
        }
        Err(DialogError::RefusedPath {
            path: format!("{} paths", self.paths.len()),
            reason: "this caller asked for exactly one path".to_owned(),
        })
    }
}

/// Shows a native dialog and returns what the person chose.
///
/// This is the seam the rest of the capability is written against: everything that decides
/// *whether* a dialog may be shown, and *what* may be done with its answer, is plain Rust over
/// this trait and is tested without a display. The `dialogs` feature supplies the one
/// implementation that puts a real picker on screen.
pub trait DialogBackend: core::fmt::Debug + Send + Sync {
    /// Shows `kind` with `options` and returns the chosen paths.
    ///
    /// # Errors
    ///
    /// [`DialogError::RefusedRequest`] when the options do not describe a dialog this module will
    /// show, [`DialogError::Cancelled`] when the person cancels, [`DialogError::Unavailable`] when
    /// this host cannot show a dialog, and [`DialogError::Failed`] when the platform shows one and
    /// cannot complete it.
    fn show(&self, kind: DialogKind, options: &DialogOptions)
    -> Result<DialogOutcome, DialogError>;
}

/// The options an application may set on a dialog.
///
/// Every field is optional and every one is validated: a title is bounded, a filter's extensions
/// must be extensions, and a default path may not carry a control character. The defaults are the
/// platform's own, which is the right answer when an application has no opinion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DialogOptions {
    /// The dialog's title, or `None` for the platform's default.
    pub title: Option<String>,
    /// The file-type filters to offer, or `None` for the platform's default of showing every file.
    /// Ignored by [`DialogKind::PickFolder`], which picks a directory and so has no file type.
    pub filters: Option<Vec<FileFilter>>,
    /// A default path — a starting directory or a suggested file name — or `None` to let the
    /// platform choose.
    pub default_path: Option<String>,
}

/// Validates a dialog request before the platform is asked for anything.
///
/// This is the whole of what can be checked without a person: that the title is bounded, that each
/// filter names at least one real extension, that a folder dialog is not given file filters it
/// cannot use, and that a default path carries no control character.
///
/// # Errors
///
/// [`DialogError::RefusedRequest`] naming the offending field for anything this module will not
/// pass to the platform.
pub fn validate_options(kind: DialogKind, options: &DialogOptions) -> Result<(), DialogError> {
    if let Some(title) = &options.title {
        if title.is_empty() {
            return Err(DialogError::RefusedRequest {
                field: "title",
                reason: "it is empty".to_owned(),
            });
        }
        if title.len() > MAX_DIALOG_TITLE_BYTES {
            return Err(DialogError::RefusedRequest {
                field: "title",
                reason: format!("it is longer than {MAX_DIALOG_TITLE_BYTES} bytes"),
            });
        }
    }
    if let Some(filters) = &options.filters {
        if kind == DialogKind::PickFolder {
            return Err(DialogError::RefusedRequest {
                field: "filters",
                reason: "a folder dialog picks a directory and has no file type to filter"
                    .to_owned(),
            });
        }
        if filters.is_empty() {
            return Err(DialogError::RefusedRequest {
                field: "filters",
                reason: "the list is empty; omit it to show every file".to_owned(),
            });
        }
        for filter in filters {
            validate_filter(filter)?;
        }
    }
    if let Some(default_path) = &options.default_path {
        validate_default_path(default_path)?;
    }
    Ok(())
}

/// Validates one file-type filter.
fn validate_filter(filter: &FileFilter) -> Result<(), DialogError> {
    if filter.name.is_empty() {
        return Err(DialogError::RefusedRequest {
            field: "filter name",
            reason: "it is empty".to_owned(),
        });
    }
    if filter.extensions.is_empty() {
        return Err(DialogError::RefusedRequest {
            field: "filter extensions",
            reason: format!("the filter \"{}\" names no extension", filter.name),
        });
    }
    for extension in &filter.extensions {
        validate_extension(extension)?;
    }
    Ok(())
}

/// Validates one extension: no leading dot, no separator, no control character, bounded.
fn validate_extension(extension: &str) -> Result<(), DialogError> {
    if extension.is_empty() {
        return Err(DialogError::RefusedRequest {
            field: "extension",
            reason: "it is empty".to_owned(),
        });
    }
    if extension.len() > MAX_EXTENSION_BYTES {
        return Err(DialogError::RefusedRequest {
            field: "extension",
            reason: format!("it is longer than {MAX_EXTENSION_BYTES} bytes"),
        });
    }
    // A leading dot is a common spelling mistake; the extension is matched without one, so
    // accepting `"txt"` and refusing `".txt"` keeps the vocabulary single-spelled.
    if let Some(character) = extension
        .chars()
        .find(|character| matches!(*character, '.' | '/' | '\\') || character.is_control())
    {
        return Err(DialogError::RefusedRequest {
            field: "extension",
            reason: format!(
                "`{extension}` contains {character:?}; name the extension without a dot or a \
                 separator"
            ),
        });
    }
    Ok(())
}

/// Validates a default path: bounded, and free of control characters.
///
/// A default path is a suggestion, not a containment boundary — the person can browse anywhere the
/// platform allows — so this checks only that it is a well-formed suggestion. It is not required to
/// exist: a save dialog's default is often a file that does not exist yet.
fn validate_default_path(default_path: &str) -> Result<(), DialogError> {
    if default_path.is_empty() {
        return Err(DialogError::RefusedRequest {
            field: "default path",
            reason: "it is empty; omit it to let the platform choose".to_owned(),
        });
    }
    if default_path.len() > MAX_PATH_BYTES {
        return Err(DialogError::RefusedRequest {
            field: "default path",
            reason: format!("it is longer than {MAX_PATH_BYTES} bytes"),
        });
    }
    if let Some(character) = default_path
        .chars()
        .find(|character| character.is_control())
    {
        return Err(DialogError::RefusedRequest {
            field: "default path",
            reason: format!("it contains the control character {character:?}"),
        });
    }
    Ok(())
}

/// Validates a path a dialog returned before handing it to a caller.
///
/// The path is untrusted input from outside this process, so it is bounded and refused if it
/// carries a control character — the same treatment any string about to reach a caller gets. It is
/// *not* required to exist, because a save dialog may name a file that does not exist yet, and it
/// is *not* contained to the application's directory, because reaching outside that directory is
/// the whole point of asking a person.
///
/// # Errors
///
/// [`DialogError::RefusedPath`] for an empty, over-long or control-carrying path.
pub fn validate_chosen_path(path: &str) -> Result<(), DialogError> {
    if path.is_empty() {
        return Err(DialogError::RefusedPath {
            path: String::new(),
            reason: "the dialog returned an empty path".to_owned(),
        });
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(DialogError::RefusedPath {
            path: path.chars().take(64).collect(),
            reason: format!("it is longer than {MAX_PATH_BYTES} bytes"),
        });
    }
    if let Some(character) = path.chars().find(|character| character.is_control()) {
        return Err(DialogError::RefusedPath {
            path: path.chars().take(64).collect(),
            reason: format!("it contains the control character {character:?}"),
        });
    }
    Ok(())
}

#[cfg(feature = "dialogs")]
mod platform {
    //! The thin adapter over `rfd`: it turns this module's vocabulary into the platform's calls
    //! and validates what comes back. Everything it decides has already been decided by
    //! [`super::validate_options`]; this is the part that talks to the person.

    use super::{
        DialogBackend, DialogError, DialogKind, DialogOptions, DialogOutcome, validate_chosen_path,
    };

    /// The platform's dialog picker, shown through `rfd`.
    ///
    /// One per application. It holds no state: a dialog is a question asked of a person, and the
    /// answer comes back through the return value rather than through a channel this owns.
    #[derive(Debug, Default)]
    pub struct RfdDialogs;

    impl RfdDialogs {
        /// A picker this host can show.
        ///
        /// Constructing one does not touch the display; the display is touched when a dialog is
        /// shown, so an application that holds the capability but never asks pays nothing.
        #[must_use]
        pub const fn new() -> Self {
            Self
        }
    }

    impl DialogBackend for RfdDialogs {
        fn show(
            &self,
            kind: DialogKind,
            options: &DialogOptions,
        ) -> Result<DialogOutcome, DialogError> {
            // The caller has already validated the options; validating again here would be
            // defensive duplication, but the platform adapter is the boundary to a person and a
            // caller that skipped validation would put a malformed dialog on screen. This module
            // validates once, at its own entry, and the command layer calls that entry.
            super::validate_options(kind, options)?;

            let mut dialog = rfd::FileDialog::new();
            if let Some(title) = &options.title {
                dialog = dialog.set_title(title);
            }
            if let Some(filters) = &options.filters {
                for filter in filters {
                    dialog = dialog.add_filter(&filter.name, &filter.extensions);
                }
            }
            match kind {
                DialogKind::OpenFile => {
                    let chosen = dialog.pick_file();
                    chosen_path(chosen)
                }
                DialogKind::OpenFiles => {
                    let chosen = dialog.pick_files();
                    chosen_paths(chosen)
                }
                DialogKind::SaveFile => {
                    let chosen = dialog.save_file();
                    chosen_path(chosen)
                }
                DialogKind::PickFolder => {
                    let chosen = dialog.pick_folder();
                    chosen_path(chosen)
                }
            }
        }
    }

    /// Wraps one chosen path, treating the platform's `None` as the person cancelling.
    fn chosen_path(chosen: Option<std::path::PathBuf>) -> Result<DialogOutcome, DialogError> {
        let Some(path) = chosen else {
            return Err(DialogError::Cancelled);
        };
        let path = path_to_string(&path)?;
        validate_chosen_path(&path)?;
        Ok(DialogOutcome { paths: vec![path] })
    }

    /// Wraps a list of chosen paths; an empty list is the person cancelling.
    fn chosen_paths(chosen: Option<Vec<std::path::PathBuf>>) -> Result<DialogOutcome, DialogError> {
        let Some(paths) = chosen else {
            return Err(DialogError::Cancelled);
        };
        if paths.is_empty() {
            return Err(DialogError::Cancelled);
        }
        let mut validated = Vec::with_capacity(paths.len());
        for path in paths {
            let path = path_to_string(&path)?;
            validate_chosen_path(&path)?;
            validated.push(path);
        }
        Ok(DialogOutcome { paths: validated })
    }

    /// Renders a chosen path, refusing one this module cannot represent as UTF-8.
    fn path_to_string(path: &std::path::Path) -> Result<String, DialogError> {
        // A path the platform returned that is not valid UTF-8 cannot be handed to a page as a
        // JSON string. Refusing it by name is the honest answer rather than lossily transcoding a
        // path and handing back one that does not point at the file the person chose.
        let rendered = path.to_string_lossy();
        if rendered.contains('\u{FFFD}') {
            return Err(DialogError::RefusedPath {
                path: rendered.chars().take(64).collect(),
                reason: "it is not valid UTF-8, so it cannot be handed to the page as a string"
                    .to_owned(),
            });
        }
        Ok(rendered.into_owned())
    }

    /// Whether the `dialogs` feature is present, for a caller that wants to report availability.
    #[must_use]
    pub const fn dialogs_available() -> bool {
        true
    }
}

#[cfg(feature = "dialogs")]
pub use platform::{RfdDialogs, dialogs_available};

#[cfg(not(feature = "dialogs"))]
/// Whether the `dialogs` feature is present.
///
/// A build without the feature cannot show a dialog, and a caller that asks reports
/// [`DialogError::Unavailable`] rather than silently doing nothing.
#[must_use]
pub const fn dialogs_available() -> bool {
    false
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

    #[test]
    fn a_well_formed_request_is_accepted() {
        let options = DialogOptions {
            title: Some("Open a note".to_owned()),
            filters: Some(vec![FileFilter {
                name: "Text".to_owned(),
                extensions: vec!["txt".to_owned(), "md".to_owned()],
            }]),
            default_path: Some("/home/someone/notes".to_owned()),
        };
        assert!(validate_options(DialogKind::OpenFile, &options).is_ok());
    }

    #[test]
    fn a_request_with_no_options_is_accepted() {
        // Every field is optional; the platform's own defaults are the right answer when the
        // application has no opinion.
        assert!(validate_options(DialogKind::OpenFile, &DialogOptions::default()).is_ok());
    }

    #[test]
    fn an_empty_or_over_long_title_is_refused() {
        let empty = DialogOptions {
            title: Some(String::new()),
            ..DialogOptions::default()
        };
        let error = validate_options(DialogKind::OpenFile, &empty).expect_err("refused");
        assert_eq!(error.code(), "dialog_request_refused");
        assert!(error.to_string().contains("title"), "{error}");

        let long = DialogOptions {
            title: Some("x".repeat(MAX_DIALOG_TITLE_BYTES + 1)),
            ..DialogOptions::default()
        };
        assert_eq!(
            validate_options(DialogKind::OpenFile, &long)
                .expect_err("refused")
                .code(),
            "dialog_request_refused"
        );
    }

    #[test]
    fn a_filter_with_no_extension_is_refused() {
        let no_extensions = DialogOptions {
            filters: Some(vec![FileFilter {
                name: "Text".to_owned(),
                extensions: Vec::new(),
            }]),
            ..DialogOptions::default()
        };
        let error = validate_options(DialogKind::OpenFile, &no_extensions).expect_err("refused");
        assert!(error.to_string().contains("Text"), "{error}");
    }

    #[test]
    fn an_extension_is_refused_for_a_leading_dot_a_separator_or_a_control() {
        for hostile in [".txt", "a/b", "a\\b", "tx\nt"] {
            let options = DialogOptions {
                filters: Some(vec![FileFilter {
                    name: "Text".to_owned(),
                    extensions: vec![hostile.to_owned()],
                }]),
                ..DialogOptions::default()
            };
            let error = validate_options(DialogKind::OpenFile, &options).expect_err("refused");
            assert_eq!(error.code(), "dialog_request_refused", "{hostile}");
            // The leading dot is the common spelling mistake and is named as such.
            if hostile == ".txt" {
                assert!(error.to_string().contains("without a dot"), "{error}");
            }
        }
    }

    #[test]
    fn an_empty_filter_list_is_refused_rather_than_shown_as_no_files() {
        let options = DialogOptions {
            filters: Some(Vec::new()),
            ..DialogOptions::default()
        };
        let error = validate_options(DialogKind::OpenFile, &options).expect_err("refused");
        assert!(error.to_string().contains("omit it"), "{error}");
    }

    #[test]
    fn a_folder_dialog_is_not_given_file_filters() {
        // A folder picker has no file type: it picks a directory. Filters there would be silently
        // ignored by the platform, which is how an application believes it filtered when it did not.
        let options = DialogOptions {
            filters: Some(vec![FileFilter {
                name: "Text".to_owned(),
                extensions: vec!["txt".to_owned()],
            }]),
            ..DialogOptions::default()
        };
        let error = validate_options(DialogKind::PickFolder, &options).expect_err("refused");
        assert!(error.to_string().contains("no file type"), "{error}");
        // The same options on a file dialog are fine.
        assert!(validate_options(DialogKind::OpenFile, &options).is_ok());
    }

    #[test]
    fn a_default_path_carrying_a_control_character_is_refused() {
        let options = DialogOptions {
            default_path: Some("/home/someone/\nnotes".to_owned()),
            ..DialogOptions::default()
        };
        let error = validate_options(DialogKind::OpenFile, &options).expect_err("refused");
        assert!(error.to_string().contains("control character"), "{error}");
    }

    #[test]
    fn a_chosen_path_is_validated_as_untrusted_input() {
        assert!(validate_chosen_path("/home/someone/notes.txt").is_ok());
        assert!(validate_chosen_path("C:\\Users\\someone\\notes.txt").is_ok()); // ncui-allow-hygiene: a Windows path fixture, not a path on any machine

        let empty = validate_chosen_path("").expect_err("refused");
        assert_eq!(empty.code(), "dialog_path_refused");

        let control = validate_chosen_path("/home/someone/no\ntes").expect_err("refused");
        assert!(
            control.to_string().contains("control character"),
            "{control}"
        );

        let long = validate_chosen_path(&"a".repeat(MAX_PATH_BYTES + 1)).expect_err("refused");
        assert!(long.to_string().contains("longer than"), "{long}");
    }

    #[test]
    fn a_single_outcome_yields_one_path_and_rejects_none_or_many() {
        let one = DialogOutcome {
            paths: vec!["/home/someone/notes.txt".to_owned()],
        };
        assert_eq!(one.single().expect("one path"), "/home/someone/notes.txt");

        let none = DialogOutcome { paths: Vec::new() };
        assert!(none.single().is_err());

        let many = DialogOutcome {
            paths: vec!["/a".to_owned(), "/b".to_owned()],
        };
        assert!(
            many.single().is_err(),
            "a caller that asked for one must not silently take the first"
        );
    }

    #[test]
    fn a_cancel_is_not_a_failure() {
        assert!(DialogError::Cancelled.is_cancelled());
        assert!(
            !DialogError::Failed {
                reason: "x".to_owned()
            }
            .is_cancelled()
        );
    }

    #[test]
    fn a_build_without_the_feature_reports_unavailable() {
        // On a build with the feature off, the availability probe says so. The platform adapter
        // behind the feature is the only thing that can show a real picker, and it is not compiled
        // here, so this is the honest answer rather than a silent no-op.
        #[cfg(not(feature = "dialogs"))]
        assert!(!dialogs_available());
    }

    #[test]
    fn every_dialog_kind_has_a_distinct_stable_name() {
        let names: Vec<&str> = [
            DialogKind::OpenFile,
            DialogKind::OpenFiles,
            DialogKind::SaveFile,
            DialogKind::PickFolder,
        ]
        .iter()
        .map(|kind| kind.as_str())
        .collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            names.len(),
            "a kind's name is duplicated: {names:?}"
        );
    }
}
