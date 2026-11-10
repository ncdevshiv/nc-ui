//! The application layer: what turns a directory of files into a running application.
//!
//! NC-UI's runtime knows how to open a window, run a page, and answer the page's calls.
//! What it did not have was the layer above that: the thing an author writes. An
//! application in NC-UI is a directory containing a manifest and some assets, and this
//! crate is what reads the manifest, serves the assets, and hands the runtime the URL and
//! the window the application asked for.
//!
//! * [`manifest`] reads `ncui.app.json` — the application's name, entry page, assets
//!   directory, window shape, and the capabilities its pages are granted.
//! * [`assets`] serves the assets over loopback HTTP, so the page has a real origin, can
//!   load modules, and can fetch its siblings.
//! * [`watch`] notices that an asset changed, so a development run can reload the page.
//!
//! Neither half talks to an engine. The runtime opens the window and the engine renders
//! it; this crate decides *what* is opened. That separation is the same one ADR-001 draws
//! between the host policy and the engine, applied one level up.
//!
//! # The shape of an application
//!
//! ```text
//! my-app/
//!   ncui.app.json      the manifest
//!   public/
//!     index.html       the entry page
//!     app.js           the page's own code
//! ```
//!
//! `ncui run my-app` reads the manifest, starts the asset server on a free loopback port,
//! opens a window at the entry page, and serves the page's calls to the runtime until the
//! window closes.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

pub mod assets;
pub mod manifest;
pub mod watch;

pub use assets::{AssetServer, Responder, Route, Served, content_type_for};
pub use manifest::{
    AppManifest, MANIFEST_FILE_NAME, ManifestError, ShortcutConfig, TrayConfig, WindowConfig,
};
pub use watch::{DirectoryWatcher, Fingerprint};
