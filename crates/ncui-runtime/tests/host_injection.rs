//! Host injection, end to end.
//!
//! The runtime never names a window engine: `Runtime::with_host_factory` installs whatever
//! [`HostFactory`] the embedding chooses, and `Application::launch_with_factory` carries
//! the same choice through the whole launch composition. These tests run in the default
//! feature set precisely because no display is involved — the factory is a test double,
//! which is the point: the wiring is the runtime's, the host is the caller's.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ncui_core::{Limits, WindowId};
use ncui_runtime::Runtime;
use ncui_runtime::application::Application;
use ncui_webview::HostFactory;

/// A test factory that counts what it spawns, so the test can prove the caller's host —
/// and nothing else — was asked for the window.
#[derive(Debug, Default)]
struct Counting {
    spawned: Arc<AtomicUsize>,
}

impl HostFactory for Counting {
    fn spawn(
        &self,
        _window: WindowId,
        _request: &ncui_webview::WindowRequest,
        _seam: &Arc<ncui_webview::HostSeam>,
    ) -> Option<Box<dyn ncui_webview::WebViewBackend>> {
        self.spawned.fetch_add(1, Ordering::SeqCst);
        Some(Box::new(Alive))
    }
}

/// The host the counting factory spawns: alive, and otherwise honest about doing nothing.
#[derive(Debug)]
struct Alive;

impl ncui_webview::WebViewBackend for Alive {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn last_error(&self) -> Option<String> {
        None
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn poll_page_message(&self) -> Option<ncui_webview::PageMessage> {
        None
    }

    fn evaluate_script(&self, _script: &str) -> bool {
        true
    }
}

#[test]
fn with_host_factory_installs_the_callers_host_over_the_given_origins() {
    let spawned = Arc::new(AtomicUsize::new(0));
    let factory = Box::new(Counting {
        spawned: Arc::clone(&spawned),
    });
    let runtime = Runtime::start(Limits::default())
        .expect("runtime should start")
        .with_host_factory(
            factory,
            &[ncui_webview::Origin::new(
                ncui_webview::Scheme::Https,
                "example.com",
                None,
            )],
        )
        .expect("the host installs");

    // The factory the caller chose is the one windows open in — that is the whole point of
    // the constructor, and the count is the proof.
    let id = runtime
        .windows
        .open(
            &ncui_webview::WindowRequest::new("https://example.com"),
            None,
        )
        .expect("a window on an allowed origin opens");
    assert_eq!(spawned.load(Ordering::SeqCst), 1);
    assert!(
        runtime
            .windows
            .summaries()
            .expect("summaries")
            .iter()
            .any(|summary| summary.id == id),
        "the window the caller's host opened is the window the runtime lists"
    );

    // The navigation policy is wired exactly as it is for the default host: a page may not
    // navigate somewhere the origins did not name, no matter whose factory is installed.
    let refused = runtime.windows.open(
        &ncui_webview::WindowRequest::new("https://elsewhere.invalid"),
        None,
    );
    assert!(
        refused.is_err(),
        "an origin the allow-list does not name must be refused"
    );
    assert_eq!(
        spawned.load(Ordering::SeqCst),
        1,
        "a refused navigation never reaches the host"
    );
}

/// A directory that is removed when the test ends.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn application(manifest: &str, page: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "ncui-host-injection-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("public")).expect("the assets directory is created");
        std::fs::write(path.join("ncui.app.json"), manifest).expect("the manifest is written");
        std::fs::write(path.join("public").join("index.html"), page).expect("the page is written");
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

const MANIFEST: &str = r#"{
    "name": "host-injection",
    "version": "1.0.0",
    "entry": "index.html",
    "assets": "public",
    "window": { "title": "host injection", "width": 640, "height": 480 }
}"#;

const PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>host injection</title></head>
<body>host injection</body></html>
"#;

#[test]
fn launch_with_factory_carries_the_callers_host_through_the_whole_composition() {
    let scratch = Scratch::application(MANIFEST, PAGE);
    let spawned = Arc::new(AtomicUsize::new(0));
    let factory = Box::new(Counting {
        spawned: Arc::clone(&spawned),
    });

    let application = Application::launch_with_factory(
        scratch.path(),
        Limits::default(),
        None,
        None,
        |_| Ok(()),
        factory,
    )
    .expect("the application launches over the caller's host");

    assert_eq!(
        spawned.load(Ordering::SeqCst),
        1,
        "the application's window opened in the host the caller chose"
    );
    let summaries = application
        .runtime()
        .windows
        .summaries()
        .expect("the windows are listed");
    assert_eq!(summaries.len(), 1, "exactly one window is open");
    assert_eq!(summaries[0].content, "page");
}

#[test]
fn launch_with_factory_reports_what_setup_reports() {
    let scratch = Scratch::application(MANIFEST, PAGE);
    let factory = Box::new(Counting::default());

    let error = Application::launch_with_factory(
        scratch.path(),
        Limits::default(),
        None,
        None,
        |_| {
            Err(ncui_core::NcError::new(ncui_core::ErrorKind::Validation {
                field: "setup".to_owned(),
                reason: "the setup's own refusal".to_owned(),
            }))
        },
        factory,
    )
    .expect_err("a failing setup fails the launch");
    assert!(
        format!("{error}").contains("the setup's own refusal"),
        "the setup's error is the launch's error, unchanged: {error}"
    );
}
