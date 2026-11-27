//! Launching an application whose manifest declares a native root window.
//!
//! ADR-020 gave a window's content two kinds, and the manifest's `entry` is where an
//! application declares which kind its root window is. These tests drive the real launcher
//! — manifest read from a real directory, assets served from a real socket, the window
//! opened through the real registry — over a recording host factory, so what is pinned is
//! the launcher's half of the contract: which request the root window is opened with, and
//! what never touches the filesystem. No display is involved, because none is needed to
//! answer those questions.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ncui_core::{ErrorKind, Limits, WindowId};
use ncui_runtime::application::Application;
use ncui_webview::{HostFactory, HostSeam, PageMessage, WebViewBackend, WindowRequest};

/// What the factory saw, shared with the test: the whole request the root window was
/// opened with, because the launcher's decisions are exactly what it puts into one.
#[derive(Debug, Default)]
struct Recorded {
    requests: Mutex<Vec<WindowRequest>>,
}

/// A window host that is alive and otherwise honest about doing nothing: the launcher is
/// what is under test, and all the host has to be for that is present.
#[derive(Debug)]
struct TestHost;

impl WebViewBackend for TestHost {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn last_error(&self) -> Option<String> {
        None
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn poll_page_message(&self) -> Option<PageMessage> {
        None
    }

    fn evaluate_script(&self, _script: &str) -> bool {
        true
    }
}

/// The factory the launch is handed: it records every request it is asked to create and
/// answers with a host that is present, the way the contract says a factory may.
#[derive(Debug)]
struct RecordingFactory {
    recorded: Arc<Recorded>,
}

impl HostFactory for RecordingFactory {
    fn spawn(
        &self,
        _window: WindowId,
        request: &WindowRequest,
        _seam: &Arc<HostSeam>,
    ) -> Option<Box<dyn WebViewBackend>> {
        self.recorded
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        Some(Box::new(TestHost))
    }
}

/// A directory that is removed when the test ends.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Creates an application directory with the manifest and nothing else: no assets, no
    /// pages. Files the test needs are written explicitly, so a launch that succeeds
    /// without one is a launch that did not need it.
    fn application(manifest: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ncui-native-entry-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        // The assets directory itself exists and is empty: the server starts for either
        // content kind, because an application whose root window is native may still open
        // page windows — and it cannot start without a root.
        std::fs::create_dir_all(path.join("public")).expect("the assets directory is created");
        std::fs::write(path.join("ncui.app.json"), manifest).expect("the manifest is written");
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

/// Launches the application in `scratch` over a recording factory, returning both.
fn launch(scratch: &Scratch) -> (Application, Arc<Recorded>) {
    let recorded = Arc::new(Recorded::default());
    let factory = RecordingFactory {
        recorded: Arc::clone(&recorded),
    };
    let application = Application::launch_with_factory(
        scratch.path(),
        Limits::default(),
        Some(scratch.path().join("state")),
        None,
        |_runtime| Ok(()),
        Box::new(factory),
    )
    .expect("the application launches");
    (application, recorded)
}

/// The one request the root window was opened with.
fn the_request(recorded: &Recorded) -> WindowRequest {
    let requests = recorded
        .requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(requests.len(), 1, "the launch opens exactly one window");
    requests[0].clone()
}

#[test]
fn a_native_entry_opens_a_native_root_window_without_touching_the_filesystem() {
    // There is deliberately no `public/index.html` — nothing in `public` at all. A launch
    // that looked for the entry as a file could not succeed, so a success here is the
    // proof that a native entry never touches the filesystem.
    let scratch = Scratch::application(
        r#"{
            "name": "settings",
            "version": "1.0.0",
            "entry": {
                "kind": "native",
                "provider": "nc-ui-kit::settings-tabs",
                "props": { "initial_tab": "audio" }
            },
            "window": { "title": "Settings", "width": 640, "height": 480, "always_on_top": true }
        }"#,
    );
    let (application, recorded) = launch(&scratch);

    let request = the_request(&recorded);
    assert!(
        request.content.is_native(),
        "the root window's content is native: {:?}",
        request.content
    );
    assert_eq!(request.content.provider(), Some("nc-ui-kit::settings-tabs"));
    let ncui_webview::WindowContent::Native { props, .. } = &request.content else {
        panic!("the content is native");
    };
    assert_eq!(props["initial_tab"], "audio", "the manifest's props reach the request");
    assert!(
        request.url.is_empty(),
        "a native window carries no URL: there is nothing to navigate to"
    );

    // The window config carries over exactly as it does for a page: a native view is
    // still a window with a frame and a size.
    assert_eq!(request.title, "Settings");
    assert_eq!(request.width, 640);
    assert_eq!(request.height, 480);
    assert!(request.always_on_top);

    // And the application reports no serving URL, because there is none.
    assert_eq!(
        application.url(),
        "",
        "a native root window is not served anything, and the application says so"
    );
}

#[test]
fn a_page_entry_opens_a_page_root_window_exactly_as_before() {
    // The branch must not change what a page launch is: the entry is resolved as a file,
    // the window opens at the loopback URL, and the content is a page.
    let scratch = Scratch::application(
        r#"{"name":"counter","version":"1.0.0","window":{"title":"Counter","width":800,"height":600}}"#,
    );
    std::fs::write(scratch.path().join("public").join("index.html"), "<h1>hi</h1>")
        .expect("the entry page is written");
    let (application, recorded) = launch(&scratch);

    let request = the_request(&recorded);
    assert!(
        !request.content.is_native(),
        "a string entry is a page: {:?}",
        request.content
    );
    assert!(
        request.url.starts_with("http://127.0.0.1:"),
        "the window opens at the loopback server: {}",
        request.url
    );
    assert!(request.url.ends_with("/index.html"), "at the entry page: {}", request.url);
    assert_eq!(request.title, "Counter");
    assert_eq!(request.width, 800);
    assert_eq!(request.height, 600);
    assert_eq!(application.url(), request.url.as_str());
}

#[test]
fn a_missing_page_entry_is_still_refused_before_any_window_opens() {
    // The page half of the branch keeps the rule it always had: a window opened at a 404
    // is not an application running.
    let scratch =
        Scratch::application(r#"{"name":"a","version":"1.0.0","entry":"missing.html"}"#);
    let recorded = Arc::new(Recorded::default());
    let factory = RecordingFactory {
        recorded: Arc::clone(&recorded),
    };
    let error = Application::launch_with_factory(
        scratch.path(),
        Limits::default(),
        Some(scratch.path().join("state")),
        None,
        |_runtime| Ok(()),
        Box::new(factory),
    )
    .expect_err("a missing entry page refuses to launch");
    assert!(
        matches!(error.kind(), ErrorKind::NotFound { .. }),
        "the refusal is the missing entry page: {error:?}"
    );
    assert!(
        error.to_string().contains("missing.html"),
        "and it names the file that is not there: {error}"
    );
    assert!(
        recorded
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "no window is opened for an application that cannot start"
    );
}

#[test]
fn a_provider_the_contract_refuses_is_caught_at_launch() {
    // The manifest's check is deliberately coarse: the provider's character set is the
    // window contract's rule, and `WindowRequest::validate` is its single source of truth.
    // An uppercase provider passes the manifest and is refused where the window is opened
    // — this test is what pins that the refusal really does happen there.
    let scratch = Scratch::application(
        r#"{"name":"a","version":"1.0.0","entry":{"kind":"native","provider":"UPPERCASE"}}"#,
    );
    let recorded = Arc::new(Recorded::default());
    let factory = RecordingFactory {
        recorded: Arc::clone(&recorded),
    };
    let error = Application::launch_with_factory(
        scratch.path(),
        Limits::default(),
        Some(scratch.path().join("state")),
        None,
        |_runtime| Ok(()),
        Box::new(factory),
    )
    .expect_err("a provider the contract refuses cannot open a window");
    assert!(
        matches!(error.kind(), ErrorKind::Validation { .. }),
        "the refusal is the request's own validation: {error:?}"
    );
    assert!(
        recorded
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "a refused request is never handed to a host"
    );
}

#[test]
fn props_past_the_contracts_bound_are_refused_at_launch() {
    // The props bound is the contract's for the same reason the charset is: a page — or
    // here a manifest — names them and host-linked code parses them. The manifest does not
    // duplicate the check, so this is the other half of the single-source-of-truth pin:
    // oversized props reach the window request and are refused there.
    let oversized = "x".repeat(70_000);
    let raw = format!(
        r#"{{"name":"a","version":"1.0.0","entry":{{"kind":"native","provider":"nc-ui-kit::settings-tabs","props":{{"pad":"{oversized}"}}}}}}"#
    );
    let scratch = Scratch::application(&raw);
    let recorded = Arc::new(Recorded::default());
    let factory = RecordingFactory {
        recorded: Arc::clone(&recorded),
    };
    let error = Application::launch_with_factory(
        scratch.path(),
        Limits::default(),
        Some(scratch.path().join("state")),
        None,
        |_runtime| Ok(()),
        Box::new(factory),
    )
    .expect_err("props past the bound cannot open a window");
    assert!(
        matches!(error.kind(), ErrorKind::Validation { .. }),
        "the refusal is the request's own validation: {error:?}"
    );
}
