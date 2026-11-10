//! The native capability surface: what an application's page may do to the machine.
//!
//! A `WebView` gives a page the web platform. A *framework* gives it the machine, and the
//! only question that matters is how much of the machine, on whose authority, with what
//! containment. This crate is the answer for the filesystem: a page that holds
//! `filesystem.read` or `filesystem.write` may reach the files under one root, and nothing
//! else.
//!
//! # The shape every capability here has
//!
//! A capability is not a boolean checked at the call site. It has three parts, and a
//! capability missing any of them is a capability that does not exist:
//!
//! * **a definition** — [`fs::FileSystem`] says what the operation is and what it refuses;
//! * **a provider** — an instance rooted at one directory, created by whoever runs the
//!   application;
//! * **a consumer** — the command registered on the page's action table, which declares
//!   the capability it requires (see `ncui_runtime::native`).
//!
//! The runtime's own pipeline supplies the fourth thing: the page grant decides whether
//! the command exists for that page at all, and the authorization step refuses a call the
//! grant does not cover even if a page learns the name some other way.
//!
//! # What is here, and what is not
//!
//! Three capabilities are here. Two are implemented completely against `std`, with no
//! platform binding and no `unsafe`: the filesystem ([`fs`]) and running a program
//! ([`process`]). The third, the clipboard ([`clipboard`]), goes through the tool each
//! platform already ships, because the clipboard has no `std` interface and its bindings
//! are not safe Rust. The rest of the native surface — dialogs, notifications, the tray,
//! menus and global shortcuts — needs a platform binding this workspace does not have yet,
//! and `docs/known-gaps.md` records exactly which ones and why. They are not offered here
//! at all: a command that reports success and does nothing would be worse than one that
//! does not exist.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

pub mod clipboard;
pub mod fs;
#[cfg(feature = "global-hotkeys")]
pub mod hotkey;
pub mod identity;
pub mod install;
pub mod notify;
pub mod open;
pub mod process;
pub mod protocol;
pub mod single_instance;

pub use clipboard::{
    CLIPBOARD_TIMEOUT, Clipboard, ClipboardError, ClipboardText, ClipboardWritten,
    MAX_CLIPBOARD_BYTES,
};
pub use fs::{
    DirectoryListing, Entry, FileContent, FileSystem, FileWritten, FsError, MAX_DIRECTORY_ENTRIES,
    MAX_READ_BYTES, MAX_WRITE_BYTES, Stat,
};
pub use identity::{
    IDENTITY_TIMEOUT, IdentityError, MAX_IDENTITY_BYTES, check as check_identity,
    register as register_identity, registered as identity_registered,
    unregister as unregister_identity,
};
pub use install::{
    BUNDLE_MANIFEST_NAME, BundleManifest, INSTALL_RECORD_NAME, InstallError, Installed, Installer,
    default_root, is_directory_name, state_root,
};
pub use notify::{MAX_BODY_BYTES, MAX_TITLE_BYTES, NotificationError, Notifier};
pub use open::{
    MAX_TARGET_BYTES, OPEN_TIMEOUT, OpenError, REFUSED_SCHEMES, check_target, open_with_default,
    scheme_of,
};
pub use process::{
    DEFAULT_TIMEOUT, MAX_OUTPUT_BYTES, MAX_TIMEOUT, ProcessError, ProcessRunner, RunOutput,
    RunRequest, is_secret_shaped, scrubbed_environment, scrubbed_from,
};
pub use protocol::{
    PROTOCOL_TIMEOUT, ProtocolError, RESERVED_SCHEMES, command_line, register as register_protocol,
    registered as protocol_registered, unregister as unregister_protocol,
};
pub use single_instance::{
    Acquired, HANDOFF_TIMEOUT, InstanceError, InstanceGuard, MAX_HANDOFF_BYTES, hand_over,
};
