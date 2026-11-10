//! An application, end to end, on a real platform.
//!
//! This test is the whole application story in one: a directory with a manifest, an asset
//! server serving it over loopback HTTP, a real `wry` window loading that page, the page
//! calling the runtime through the bridge, and a native command answering.
//!
//! Nothing here is a stand-in. The manifest is read by the real reader, the server binds a
//! real socket, the engine is the platform's own, and the command that finally runs is a
//! real closure the test registered.
//!
//! It runs on Windows first, because that is where the engine is verified; the same test
//! runs on any platform whose engine is available.

#![cfg(feature = "wry-host")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ncui_core::{CapabilitySet, Limits};
use ncui_runtime::application::Application;

/// A directory that is removed when the test ends.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Creates an application directory with a manifest and a page.
    fn application(manifest: &str, page: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ncui-application-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("public")).expect("the assets directory is created");
        std::fs::write(path.join("ncui.app.json"), manifest).expect("the manifest is written");
        std::fs::write(path.join("public").join("index.html"), page).expect("the page is written");
        Self { path }
    }

    /// The directory itself.
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The manifest of the application under test.
const MANIFEST: &str = r#"{
    "name": "end-to-end",
    "version": "1.0.0",
    "entry": "index.html",
    "assets": "public",
    "window": { "title": "NC-UI end to end", "width": 800, "height": 600 }
}"#;

/// The page under test.
///
/// It calls a native command the host registered, and reports what came back by asking
/// the host to record it. The recording is the observation point: the test asserts on the
/// native side, where the truth is.
const PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>end to end</title></head>
<body>
<script>
  window.ncui.invoke('record', { value: 42, name: 'from the page' })
    .then(function (answer) {
      return window.ncui.invoke('record', { value: answer.echoed, name: 'again' });
    })
    .catch(function (error) {
      window.ncui.invoke('record', { value: -1, name: String(error && error.message) });
    });
</script>
</body></html>
"#;

/// A two-by-two bitmap: two blue pixels on the bottom row, two red ones on the top.
///
/// Written by hand rather than checked into the repository as a fixture: the bytes are the format,
/// so a fixture would hide the one thing this test is about.
fn bitmap() -> Vec<u8> {
    let mut file = Vec::new();
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&70_u32.to_le_bytes());
    file.extend_from_slice(&0_u32.to_le_bytes());
    file.extend_from_slice(&54_u32.to_le_bytes());
    file.extend_from_slice(&40_u32.to_le_bytes());
    file.extend_from_slice(&2_i32.to_le_bytes());
    file.extend_from_slice(&2_i32.to_le_bytes());
    file.extend_from_slice(&1_u16.to_le_bytes());
    file.extend_from_slice(&24_u16.to_le_bytes());
    file.extend_from_slice(&0_u32.to_le_bytes());
    file.extend_from_slice(&0_u32.to_le_bytes());
    file.extend_from_slice(&0_u32.to_le_bytes());
    file.extend_from_slice(&0_u32.to_le_bytes());
    file.extend_from_slice(&0_u32.to_le_bytes());
    file.extend_from_slice(&0_u32.to_le_bytes());
    // The rows are stored bottom up, each padded to a four-byte boundary.
    file.extend_from_slice(&[0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00]);
    file.extend_from_slice(&[0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00]);
    file
}

/// The manifest of an application that names an icon.
fn manifest_with_icon(icon: &str) -> String {
    format!(
        r#"{{
            "name": "with-an-icon",
            "version": "1.0.0",
            "entry": "index.html",
            "assets": "public",
            "window": {{ "title": "With an icon", "icon": "{icon}" }}
        }}"#
    )
}

#[test]
fn an_application_whose_manifest_names_an_icon_opens_with_it() {
    // The whole path, on a real platform: the manifest names a file, the file is read and decoded
    // into pixels, and the pixels are handed to the platform's own window-creation call.
    let scratch = Scratch::application(&manifest_with_icon("icon.bmp"), PAGE);
    std::fs::write(scratch.path().join("icon.bmp"), bitmap()).expect("the icon is written");
    let mut application = Application::launch(scratch.path(), Limits::default())
        .expect("the application launches with its icon");
    assert!(application.is_running(), "the window is open");
    application.shutdown();
}

#[test]
fn an_icon_that_cannot_be_read_is_refused_by_name() {
    // A manifest that names an icon that is not there is wrong, and the person who can fix it is
    // the one reading the error — so the error names the file they named.
    let scratch = Scratch::application(&manifest_with_icon("missing.bmp"), PAGE);
    let error = Application::launch(scratch.path(), Limits::default())
        .expect_err("an icon that is not there is refused");
    let message = error.to_string();
    assert!(message.contains("icon"), "{message}");
    assert!(message.contains("missing.bmp"), "{message}");
}

#[test]
fn an_icon_in_a_format_this_runtime_cannot_read_is_refused_by_name() {
    let scratch = Scratch::application(&manifest_with_icon("icon.bmp"), PAGE);
    std::fs::write(scratch.path().join("icon.bmp"), b"not a bitmap").expect("the file is written");
    let error = Application::launch(scratch.path(), Limits::default())
        .expect_err("an icon that cannot be decoded is refused");
    let message = error.to_string();
    assert!(message.contains("icon"), "{message}");
    assert!(message.contains("icon.bmp"), "{message}");
}

/// The manifest of an application that declares one global shortcut.
fn manifest_with_shortcut(accelerator: &str) -> String {
    format!(
        r#"{{
            "name": "with-a-shortcut",
            "version": "1.0.0",
            "entry": "index.html",
            "assets": "public",
            "shortcuts": [{{ "accelerator": "{accelerator}", "command": "toggle" }}]
        }}"#
    )
}

#[test]
fn an_application_holds_the_shortcut_its_manifest_declares() {
    // The whole path, on a real platform: the manifest names a key, the platform is asked for it,
    // and the application holds it while it runs.
    let scratch = Scratch::application(&manifest_with_shortcut("Ctrl+Alt+F13"), PAGE);
    #[cfg(feature = "global-shortcuts")]
    {
        let mut application = Application::launch(scratch.path(), Limits::default())
            .expect("the application launches and takes the key");
        assert_eq!(application.shortcuts(), vec!["Ctrl+Alt+F13".to_owned()]);
        application.shutdown();
        assert!(
            application.shortcuts().is_empty(),
            "the key is released when the application stops"
        );
    }
    // A build that cannot hold shortcuts refuses a manifest that declares one, rather than
    // launching an application whose shortcut silently does nothing.
    #[cfg(not(feature = "global-shortcuts"))]
    {
        let error = Application::launch(scratch.path(), Limits::default())
            .expect_err("a manifest naming a shortcut is refused by a build that has none");
        assert!(
            error.to_string().contains("global-shortcuts"),
            "the refusal must say which feature is missing: {error}"
        );
    }
}

#[test]
fn a_shortcut_the_vocabulary_does_not_know_is_refused_at_launch() {
    // The accelerator's meaning is checked where it is used, because the vocabulary that reads it
    // lives with the platform binding rather than with the manifest reader.
    #[cfg(feature = "global-shortcuts")]
    {
        let scratch = Scratch::application(&manifest_with_shortcut("Ctrl+Banana"), PAGE);
        let error = Application::launch(scratch.path(), Limits::default())
            .expect_err("an accelerator this runtime does not know is refused");
        assert!(
            error.to_string().contains("Banana"),
            "the refusal must name the key: {error}"
        );
        assert!(
            error.to_string().contains("toggle"),
            "and the command it belonged to: {error}"
        );
    }
}

/// The manifest of an application that shows a tray icon.
fn manifest_with_tray(icon: &str) -> String {
    format!(
        r#"{{
            "name": "with-a-tray",
            "version": "1.0.0",
            "entry": "index.html",
            "assets": "public",
            "tray": {{ "icon": "{icon}", "tooltip": "NC-UI counter" }}
        }}"#
    )
}

#[test]
fn an_application_shows_the_tray_icon_its_manifest_declares() {
    // The whole path, on a real platform: the manifest names an image, the image is decoded, and
    // the platform is asked for a tray icon. A platform that refused it would refuse the launch —
    // an application whose tray silently never appears is worse than one that says so.
    let scratch = Scratch::application(&manifest_with_tray("tray.bmp"), PAGE);
    std::fs::write(scratch.path().join("tray.bmp"), bitmap()).expect("the tray icon is written");
    #[cfg(feature = "tray-icon")]
    {
        let mut application = Application::launch(scratch.path(), Limits::default())
            .expect("the application launches and shows its tray icon");
        assert!(application.is_running(), "the window is open");
        application.shutdown();
    }
    // A build that cannot show a tray refuses a manifest that declares one.
    #[cfg(not(feature = "tray-icon"))]
    {
        let error = Application::launch(scratch.path(), Limits::default())
            .expect_err("a manifest naming a tray is refused by a build that has none");
        assert!(
            error.to_string().contains("tray-icon"),
            "the refusal must say which feature is missing: {error}"
        );
    }
}

#[test]
fn a_tray_icon_that_cannot_be_read_is_refused_by_name() {
    #[cfg(feature = "tray-icon")]
    {
        let scratch = Scratch::application(&manifest_with_tray("missing.bmp"), PAGE);
        let error = Application::launch(scratch.path(), Limits::default())
            .expect_err("a tray icon that is not there is refused");
        let message = error.to_string();
        assert!(message.contains("tray"), "{message}");
        assert!(message.contains("missing.bmp"), "{message}");

        // And one that is there but is not an image, which is the other half of the same mistake.
        let scratch = Scratch::application(&manifest_with_tray("tray.bmp"), PAGE);
        std::fs::write(scratch.path().join("tray.bmp"), b"not an image")
            .expect("the file is written");
        let error = Application::launch(scratch.path(), Limits::default())
            .expect_err("a tray icon that cannot be decoded is refused");
        let message = error.to_string();
        assert!(message.contains("tray.bmp"), "{message}");
    }
}

#[test]
fn the_window_reports_the_platforms_theme() {
    // The theme is the machine's setting for every window on it, so the answer has to come from the
    // platform: a window whose engine cannot ask reports nothing, and a real window on a real
    // platform reports one of the two. Which one it is belongs to the machine this runs on.
    let scratch = Scratch::application(MANIFEST, PAGE);
    let mut application =
        Application::launch(scratch.path(), Limits::default()).expect("the application launches");
    let theme = application
        .runtime()
        .windows
        .window_theme(application.window());
    assert!(
        matches!(
            theme,
            Some(ncui_webview::Theme::Light | ncui_webview::Theme::Dark)
        ),
        "a real window reports the platform's theme, got {theme:?}"
    );
    application.shutdown();
}

#[test]
fn a_real_window_can_be_hidden_and_shown_again() {
    // Visibility is the platform's answer, the same way the theme and the geometry are: the window is
    // asked to hide and then asked whether it is hidden, so what comes back is what the platform did
    // rather than what the runtime asked for. Hiding is not closing — the page stays alive — which is
    // what makes a splash-then-main window or a background window possible.
    let scratch = Scratch::application(MANIFEST, PAGE);
    let mut application =
        Application::launch(scratch.path(), Limits::default()).expect("the application launches");
    let id = application.window();
    let windows = &application.runtime().windows;

    let state = windows
        .set_window_state(id, ncui_webview::WindowChange::Visible(false))
        .expect("the window is open")
        .expect("a real engine can hide a window");
    assert!(!state.visible, "a hidden window reports itself hidden");

    let state = windows
        .set_window_state(id, ncui_webview::WindowChange::Visible(true))
        .expect("the window is open")
        .expect("a real engine can show a window again");
    assert!(state.visible, "and a shown one reports itself shown");

    application.shutdown();
}

#[test]
fn a_real_window_reports_the_frame_its_shape_asked_for() {
    // `ncui.window.shape` changes a window's frame and `ncui.window.state` reads it back. The two
    // agree because the read comes from the platform rather than from the runtime's memory of the
    // shape: a page that made its window float and then asked whether it floats has to be told the
    // platform's answer, which is the only one that describes the screen. The toggle starts from
    // whatever the platform's default is, so the test does not assume one.
    let scratch = Scratch::application(MANIFEST, PAGE);
    let mut application =
        Application::launch(scratch.path(), Limits::default()).expect("the application launches");
    let id = application.window();
    let windows = &application.runtime().windows;

    let before = windows.window_state(id).expect("a real engine can report");
    let shape = ncui_webview::WindowShape {
        always_on_top: Some(!before.always_on_top),
        resizable: Some(!before.resizable),
        ..ncui_webview::WindowShape::default()
    };
    windows
        .set_window_shape(id, &shape)
        .expect("a real engine can reshape a window");
    let after = windows.window_state(id).expect("a real engine can report");
    assert_eq!(
        after.always_on_top, !before.always_on_top,
        "whether the window floats must be the platform's answer after the change"
    );
    assert_eq!(
        after.resizable, !before.resizable,
        "and so must whether the user may resize it"
    );

    application.shutdown();
}

/// The page that listens for its own window's events and reports each one back.
///
/// It records `ready` once it has subscribed, because an event that arrives before the listener
/// exists is an event the page never hears — the test has to know the page is listening before it
/// moves the window.
const EVENT_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>events</title></head>
<body>
<script>
  window.ncui.on('resized', function (payload) {
    window.ncui.invoke('record', { name: 'resized', value: payload });
  });
  window.ncui.on('moved', function (payload) {
    window.ncui.invoke('record', { name: 'moved', value: payload });
  });
  window.ncui.invoke('record', { name: 'ready', value: 0 });
</script>
</body></html>
"#;

#[test]
fn a_page_hears_its_own_window_resized() {
    // The window is the page's own frame, and a layout built for the old size is a layout the user has
    // already left. The page is told through the same event channel the theme uses, and it reports
    // what it received through a native command — so the assertion is on what the page really heard
    // rather than on what the host believes it sent.
    let scratch = Scratch::application(MANIFEST, EVENT_PAGE);
    let recorded: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&recorded);
    let mut application =
        Application::launch_with(scratch.path(), Limits::default(), None, move |runtime| {
            let _registration = runtime
                .register_page_command(
                    "record",
                    CapabilitySet::empty(),
                    "Record what the page sent.",
                    move |payload| {
                        recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(payload.clone());
                        Ok(serde_json::json!({ "echoed": true }))
                    },
                )
                .map_err(|error| {
                    ncui_core::NcError::new(ncui_core::ErrorKind::Validation {
                        field: "record".to_owned(),
                        reason: error.to_string(),
                    })
                    .operation("test::launch")
                })?;
            Ok(())
        })
        .expect("the application launches");

    let heard = |want: &str| {
        recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|record| record.get("name").and_then(serde_json::Value::as_str) == Some(want))
    };
    // The page's calls only reach the runtime when the runtime is pumped, so waiting for one means
    // pumping as well as sleeping.
    let wait_for = |application: &mut Application, want: &str| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline && !heard(want) {
            application.pump();
            std::thread::sleep(Duration::from_millis(25));
        }
    };

    wait_for(&mut application, "ready");
    assert!(
        heard("ready"),
        "the page must report that it is listening before the window is moved"
    );

    let id = application.window();
    {
        let windows = &application.runtime().windows;
        windows
            .set_window_shape(
                id,
                &ncui_webview::WindowShape {
                    width: Some(940),
                    height: Some(700),
                    ..ncui_webview::WindowShape::default()
                },
            )
            .expect("a real engine can resize a window");
    }

    wait_for(&mut application, "resized");
    let reported = recorded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|record| record.get("name").and_then(serde_json::Value::as_str) == Some("resized"))
        .cloned()
        .expect("the page must hear its window resized");
    let width = reported
        .pointer("/value/width")
        .and_then(serde_json::Value::as_f64)
        .expect("the event carries the new width");
    assert!(
        (width - 940.0).abs() < 1.0,
        "the page must hear the size the platform gave the window, got {reported}"
    );

    application.shutdown();
}

#[test]
fn an_application_serves_its_page_and_the_page_calls_native_code() {
    let scratch = Scratch::application(MANIFEST, PAGE);

    // What the native command recorded. The command is the observation point: the page
    // has no other way to tell the test what happened.
    let recorded: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&recorded);

    let mut application =
        Application::launch_with(scratch.path(), Limits::default(), None, |runtime| {
            let _registration = runtime
                .register_page_command(
                    "record",
                    CapabilitySet::empty(),
                    "Record what the page sent.",
                    move |payload| {
                        let value = payload
                            .get("value")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(payload.clone());
                        Ok(serde_json::json!({ "echoed": value }))
                    },
                )
                .map_err(|error| {
                    ncui_core::NcError::new(ncui_core::ErrorKind::Validation {
                        field: "record".to_owned(),
                        reason: error.to_string(),
                    })
                    .operation("test::launch")
                })?;
            Ok(())
        })
        .expect("the application launches");

    assert_eq!(application.name(), "end-to-end");
    assert_eq!(application.version(), "1.0.0");
    assert!(
        application.url().starts_with("http://127.0.0.1:"),
        "the page must be served from the loopback interface; got: {}",
        application.url()
    );

    // The page is served: the URL the application reports is really being answered.
    let served = fetch(application.url());
    assert!(
        served.contains("end to end"),
        "the entry page must be served; got: {served}"
    );

    // The page calls the native command. The window is real, so this takes as long as the
    // engine takes to load the page.
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        application.pump();
        let count = recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        if count >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let entries = recorded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(
        entries.len() >= 2,
        "the page must reach the native command twice; got: {entries:?}"
    );
    assert_eq!(
        entries[0]["value"], 42,
        "the page's own payload must arrive unchanged"
    );
    assert_eq!(entries[0]["name"], "from the page");
    assert_eq!(
        entries[1]["value"], 42,
        "the answer the page received must be the answer the command returned"
    );

    application.shutdown();
}

/// Fetches a URL over HTTP and returns the body.
fn fetch(url: &str) -> String {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let authority = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let path = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/').map(|(_, path)| path.to_owned()))
        .unwrap_or_else(|| "/".to_owned());
    let mut stream = TcpStream::connect(&authority).expect("the server accepts");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("the timeout is set");
    let request = format!("GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("the request is written");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("the response is read");
    let text = String::from_utf8_lossy(&raw).into_owned();
    text.split_once("\r\n\r\n")
        .map_or(text.clone(), |(_, body)| body.to_owned())
}

/// A manifest that grants the page the read capability.
const READ_MANIFEST: &str = r#"{
    "name": "reader",
    "version": "1.0.0",
    "entry": "index.html",
    "assets": "public",
    "capabilities": ["filesystem.read"]
}"#;

/// A page that reads a file through the native command and reports what it got.
const READ_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>reader</title></head>
<body>
<script>
  window.ncui.invoke('fs.read_text', { path: 'note.txt' })
    .then(function (file) {
      return window.ncui.invoke('record', { value: file.text, bytes: file.bytes });
    })
    .catch(function (error) {
      window.ncui.invoke('record', { value: 'FAILED: ' + String(error && error.message) });
    });
</script>
</body></html>
"#;

#[test]
fn an_application_with_the_read_capability_reads_a_file_from_the_page() {
    let scratch = Scratch::application(READ_MANIFEST, READ_PAGE);
    // The file lives in the application directory, not in the served assets: the page can
    // read it because the manifest grants `filesystem.read`, not because it is public.
    std::fs::write(scratch.path().join("note.txt"), "read from the page")
        .expect("the fixture is written");

    let recorded: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&recorded);
    let mut application =
        Application::launch_with(scratch.path(), Limits::default(), None, |runtime| {
            let _registration = runtime
                .register_page_command(
                    "record",
                    CapabilitySet::empty(),
                    "Record what the page sent.",
                    move |payload| {
                        recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(payload.clone());
                        Ok(serde_json::json!({ "recorded": true }))
                    },
                )
                .map_err(|error| {
                    ncui_core::NcError::new(ncui_core::ErrorKind::Validation {
                        field: "record".to_owned(),
                        reason: error.to_string(),
                    })
                    .operation("test::launch")
                })?;
            Ok(())
        })
        .expect("the application launches");

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        application.pump();
        let count = recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        if count >= 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let entries = recorded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        entries.len(),
        1,
        "the page must report exactly once; got: {entries:?}"
    );
    assert_eq!(
        entries[0]["value"], "read from the page",
        "the page must receive the file's contents; got: {entries:?}"
    );
    assert_eq!(entries[0]["bytes"], 18, "the size must be the file's own");

    application.shutdown();
}

/// A manifest that grants the page the spawn capability.
const SPAWN_MANIFEST: &str = r#"{
    "name": "runner",
    "version": "1.0.0",
    "entry": "index.html",
    "assets": "public",
    "capabilities": ["process.spawn"]
}"#;

/// A page that runs a program and reports what came back.
///
/// `CONSENT` is replaced by the test: a destructive capability requires the page to state
/// its consent, and the two tests below are the two answers to that question.
fn spawn_page(consent: bool) -> String {
    let consent = if consent { ", \"consent\": true" } else { "" };
    let header = "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>runner</title></head>\n<body>\n<script>\n";
    let body = r"
  window.ncui.invoke('process.run', { program: 'PROGRAM', args: ARGS, stdin: 'ignored when the program does not read' CONSENT })
    .then(function (result) {
      return window.ncui.invoke('record', { value: result.stdout.trim(), status: result.status, timed_out: result.timed_out });
    })
    .catch(function (error) {
      return window.ncui.invoke('record', { value: 'REFUSED', code: String(error && error.code) });
    });
";
    let (program, args) = if cfg!(windows) {
        ("cmd", r#"["/C", "echo from the page"]"#)
    } else {
        ("sh", r#"["-c", "echo from the page"]"#)
    };
    let script = body
        .replace("PROGRAM", program)
        .replace("ARGS", args)
        .replace("CONSENT", consent);
    format!("{header}{script}</script>\n</body></html>\n")
}

/// Launches an application that records what its page sends.
fn launch_with_recorder(scratch: &Scratch) -> (Application, Arc<Mutex<Vec<serde_json::Value>>>) {
    let recorded: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&recorded);
    let application =
        Application::launch_with(scratch.path(), Limits::default(), None, |runtime| {
            let _registration = runtime
                .register_page_command(
                    "record",
                    CapabilitySet::empty(),
                    "Record what the page sent.",
                    move |payload| {
                        recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(payload.clone());
                        Ok(serde_json::json!({ "recorded": true }))
                    },
                )
                .map_err(|error| {
                    ncui_core::NcError::new(ncui_core::ErrorKind::Validation {
                        field: "record".to_owned(),
                        reason: error.to_string(),
                    })
                    .operation("test::launch_with_recorder")
                })?;
            Ok(())
        })
        .expect("the application launches");
    (application, recorded)
}

/// Pumps until the page has recorded at least `count` messages, or the budget runs out.
fn pump_until(
    application: &mut Application,
    recorded: &Arc<Mutex<Vec<serde_json::Value>>>,
    count: usize,
) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        application.pump();
        let entries = recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if entries.len() >= count {
            return entries;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    recorded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[test]
fn a_page_that_states_consent_runs_a_program_and_receives_its_output() {
    let scratch = Scratch::application(SPAWN_MANIFEST, &spawn_page(true));
    let (mut application, recorded) = launch_with_recorder(&scratch);

    let entries = pump_until(&mut application, &recorded, 1);
    assert_eq!(
        entries.len(),
        1,
        "the page must report once; got: {entries:?}"
    );
    assert_eq!(
        entries[0]["value"], "from the page",
        "the page must receive the program's output; got: {entries:?}"
    );
    assert_eq!(entries[0]["status"], 0, "the program exits zero");
    assert_eq!(entries[0]["timed_out"], false);

    application.shutdown();
}

#[test]
fn a_page_that_does_not_state_consent_is_refused_before_the_program_starts() {
    let scratch = Scratch::application(SPAWN_MANIFEST, &spawn_page(false));
    let (mut application, recorded) = launch_with_recorder(&scratch);

    let entries = pump_until(&mut application, &recorded, 1);
    assert_eq!(
        entries.len(),
        1,
        "the page must report once; got: {entries:?}"
    );
    assert_eq!(
        entries[0]["value"], "REFUSED",
        "a destructive capability without consent must be refused; got: {entries:?}"
    );
    assert_eq!(
        entries[0]["code"], "capability_denied",
        "the refusal must say why; got: {entries:?}"
    );

    application.shutdown();
}

/// A manifest with no capabilities: the reload loop needs none.
const RELOAD_MANIFEST: &str = r#"{
    "name": "reloader",
    "version": "1.0.0",
    "entry": "index.html",
    "assets": "public"
}"#;

/// A page that counts how many times it has loaded, across reloads.
///
/// `localStorage` survives a reload; a variable in the document does not. Counting there is
/// what lets the test tell "the page was reloaded" from "the page was served twice".
const RELOAD_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>reloader</title></head>
<body>
<script>
  const loads = Number(localStorage.getItem('ncui-loads') || '0') + 1;
  localStorage.setItem('ncui-loads', String(loads));
  window.ncui.invoke('record', { loads: loads });
</script>
</body></html>
"#;

#[test]
fn a_file_change_reloads_the_page_in_the_window() {
    let scratch = Scratch::application(RELOAD_MANIFEST, RELOAD_PAGE);
    let (mut application, recorded) = launch_with_recorder(&scratch);
    application.watch().expect("the application can be watched");

    let first = pump_until(&mut application, &recorded, 1);
    assert_eq!(first.len(), 1, "the page must load once; got: {first:?}");
    assert_eq!(first[0]["loads"], 1, "the first load is the first document");

    // Edit the entry page. The server serves from disk, so the next request sees this; the
    // watcher is what makes the page ask again.
    std::thread::sleep(Duration::from_millis(50));
    std::fs::write(
        scratch.path().join("public").join("index.html"),
        RELOAD_PAGE.replace("<body>", "<body data-edited=\"yes\">"),
    )
    .expect("the page is edited");

    let entries = pump_until(&mut application, &recorded, 2);
    assert!(
        entries.len() >= 2,
        "the page must load again after the edit; got: {entries:?}"
    );
    assert_eq!(
        entries[1]["loads"], 2,
        "the second load must be a second document, not a replay; got: {entries:?}"
    );
    assert!(
        application.reloads() >= 1,
        "the application must report the reload it performed"
    );
    let labels = ncui_observability::metrics::Labels::new();
    assert_eq!(
        application
            .runtime()
            .metrics
            .counter_value("app.reloads", &labels),
        Some(u64::try_from(application.reloads()).expect("a small count")),
        "every reload must be counted"
    );

    application.shutdown();
}

/// A manifest that lets its page open and close windows.
const WINDOW_MANIFEST: &str = r#"{
    "name": "windower",
    "version": "1.0.0",
    "entry": "index.html",
    "assets": "public",
    "capabilities": ["window"]
}"#;

/// A page that opens a second window at its own URL, then closes the one it is in.
const WINDOW_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>windower</title></head>
<body>
<script>
  // The second window loads this same page. Without the guard it would open a third
  // window, and a fourth, until the registry refused — which is a property of the test,
  // not of the framework. `localStorage` is shared by windows of one origin.
  if (!localStorage.getItem('ncui-window-test')) {
    localStorage.setItem('ncui-window-test', '1');
    window.ncui.invoke('create_window', { url: window.location.href, title: 'second' })
      .then(function (created) {
        return window.ncui.invoke('record', { created: true, id: created.id });
      })
      .then(function () {
        return window.ncui.invoke('close_window', {});
      })
      .catch(function (error) {
        window.ncui.invoke('record', { created: false, error: String(error && error.message) });
      });
  }
</script>
</body></html>
"#;

#[test]
fn a_page_opens_a_second_window_and_closes_its_own() {
    let scratch = Scratch::application(WINDOW_MANIFEST, WINDOW_PAGE);
    let (mut application, recorded) = launch_with_recorder(&scratch);
    let caller = application.window();

    let entries = pump_until(&mut application, &recorded, 1);
    assert_eq!(
        entries.len(),
        1,
        "the page must report once; got: {entries:?}"
    );
    assert_eq!(
        entries[0]["created"], true,
        "the page must be allowed to open a window: {entries:?}"
    );

    // The page then closes the window it is in. Its own window must go and the one it
    // opened must stay.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut remaining: Vec<ncui_webview::WindowSummary> = Vec::new();
    while Instant::now() < deadline {
        application.pump();
        remaining = application
            .runtime()
            .windows
            .summaries()
            .expect("the window surface");
        if remaining.len() < 2 && remaining.iter().all(|summary| summary.id != caller) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        remaining.len(),
        1,
        "closing the caller's window must leave exactly the window it opened: {remaining:?}"
    );
    assert_ne!(
        remaining[0].id, caller,
        "the page must not have closed the window it opened"
    );
    assert_eq!(remaining[0].title, "second");

    // The second window is a real window; close it so the test leaves nothing behind.
    let _ = application.runtime().windows.close(remaining[0].id);
    application.shutdown();
}

/// A page that makes its own window maximised, reports what the platform said, and gives the
/// window back.
const PRESENTATION_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>presentation</title></head>
<body>
<script>
  window.ncui.invoke('window_state', { maximized: true })
    .then(function (changed) {
      return window.ncui.invoke('record', { maximized: changed.state.maximized });
    })
    .then(function () {
      return window.ncui.invoke('window_state', { maximized: false });
    })
    .then(function (restored) {
      return window.ncui.invoke('record', { restored: restored.state.maximized });
    })
    .catch(function (error) {
      window.ncui.invoke('record', { failed: String(error && error.message) });
    });
</script>
</body></html>
"#;

#[test]
fn a_page_changes_the_presentation_of_the_window_it_is_in() {
    // The whole path on a real platform: the page asks, the seam injects the caller's own window,
    // the runtime asks the engine, the engine asks the platform, and the answer the page receives
    // is what the platform reported — not what the page asked for.
    let scratch = Scratch::application(WINDOW_MANIFEST, PRESENTATION_PAGE);
    let (mut application, recorded) = launch_with_recorder(&scratch);

    let entries = pump_until(&mut application, &recorded, 2);
    assert_eq!(entries.len(), 2, "the page must report twice: {entries:?}");
    assert!(
        entries[0].get("failed").is_none() && entries[1].get("failed").is_none(),
        "the presentation calls must succeed: {entries:?}"
    );
    assert_eq!(
        entries[0]["maximized"], true,
        "the platform must report the window maximised after the change: {entries:?}"
    );
    assert_eq!(
        entries[1]["restored"], false,
        "and windowed again after the change back: {entries:?}"
    );

    // The runtime reports the same thing the page was told, because it is the same answer.
    let state = application
        .runtime()
        .windows
        .window_state(application.window())
        .expect("a real host reports how its window is presented");
    assert!(!state.maximized, "the window was given back");

    application.shutdown();
}

/// A page that gives its own window a new title and size, and reports what the runtime answered.
const RESHAPE_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>reshape</title></head>
<body>
<script>
  window.ncui.invoke('window_shape', { title: 'reshaped', width: 900, height: 700 })
    .then(function (answer) {
      return window.ncui.invoke('record', {
        title: answer.title,
        width: answer.width,
        height: answer.height
      });
    })
    .catch(function (error) {
      window.ncui.invoke('record', { failed: String(error && error.message) });
    });
</script>
</body></html>
"#;

#[test]
fn a_page_reshapes_the_window_it_is_in() {
    // The whole path on a real platform: the page asks, the seam injects its own window, the engine
    // asks the platform to resize, and the size the runtime then reports is the platform's own
    // answer rather than the number the page sent.
    let scratch = Scratch::application(WINDOW_MANIFEST, RESHAPE_PAGE);
    let (mut application, recorded) = launch_with_recorder(&scratch);

    let entries = pump_until(&mut application, &recorded, 1);
    assert_eq!(entries.len(), 1, "the page must report once: {entries:?}");
    assert!(
        entries[0].get("failed").is_none(),
        "the reshape must succeed: {entries:?}"
    );
    assert_eq!(entries[0]["title"], "reshaped", "{entries:?}");
    assert_eq!(entries[0]["width"], 900, "{entries:?}");

    // The platform's own answer: the window is the size it was asked to be, in logical pixels. The
    // tolerance covers the rounding a display with a fractional scale factor does.
    let geometry = application
        .runtime()
        .windows
        .geometry(application.window())
        .expect("a real host reports its window's geometry");
    assert!(
        geometry.width.abs_diff(900) <= 2 && geometry.height.abs_diff(700) <= 2,
        "the platform must have resized the window: {geometry:?}"
    );

    application.shutdown();
}

/// A manifest that grants the page the clipboard.
const CLIPBOARD_MANIFEST: &str = r#"{
    "name": "clipper",
    "version": "1.0.0",
    "entry": "index.html",
    "assets": "public",
    "capabilities": ["clipboard"]
}"#;

/// A page that writes a value to the clipboard, reads it back, and reports both.
const CLIPBOARD_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>clipper</title></head>
<body>
<script>
  const written = 'ncui page clipboard ' + Math.random().toString(36).slice(2);
  window.ncui.invoke('clipboard.write_text', { text: written })
    .then(function () {
      return window.ncui.invoke('clipboard.read_text', {});
    })
    .then(function (read) {
      return window.ncui.invoke('record', { written: written, read: read.text, bytes: read.bytes });
    })
    .catch(function (error) {
      window.ncui.invoke('record', { failed: String(error && error.message) });
    });
</script>
</body></html>
"#;

#[test]
fn a_page_writes_and_reads_the_real_clipboard() {
    let scratch = Scratch::application(CLIPBOARD_MANIFEST, CLIPBOARD_PAGE);
    let (mut application, recorded) = launch_with_recorder(&scratch);

    let entries = pump_until(&mut application, &recorded, 1);
    assert_eq!(
        entries.len(),
        1,
        "the page must report once; got: {entries:?}"
    );
    assert!(
        entries[0].get("failed").is_none(),
        "the clipboard call must succeed: {entries:?}"
    );
    assert_eq!(
        entries[0]["read"], entries[0]["written"],
        "the page must read back exactly what it wrote: {entries:?}"
    );
    assert!(
        entries[0]["written"]
            .as_str()
            .is_some_and(|text| text.starts_with("ncui page clipboard ")),
        "the value must be the page's own: {entries:?}"
    );

    application.shutdown();
}

/// A state directory for an instance lock, removed when the test ends.
struct InstanceState {
    path: PathBuf,
}

impl InstanceState {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ncui-instance-state-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        Self { path }
    }
}

impl Drop for InstanceState {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn a_second_launch_reaches_the_running_application_instead_of_opening_a_twin() {
    // The whole point of one instance per application: a second launch hands its arguments to
    // the instance that is already serving, and that instance reloads the page it has. Nothing
    // here is simulated — the hand-off crosses a real loopback socket, and the reload is posted
    // to a real window.
    let scratch = Scratch::application(MANIFEST, PAGE);
    let state = InstanceState::new();
    let guard = match ncui_native::InstanceGuard::acquire(&state.path, "end-to-end")
        .expect("the state directory is usable")
    {
        ncui_native::Acquired::First(guard) => guard,
        ncui_native::Acquired::AlreadyRunning { port, .. } => {
            panic!("a fresh state directory must not name a running instance (port {port})")
        }
    };
    let port = guard.port().expect("a bound listener has a port");
    let token = guard.token().to_owned();
    let lock = guard.path().to_path_buf();

    let mut application = Application::launch(scratch.path(), Limits::default())
        .expect("the application launches")
        .with_instance(*guard);
    assert!(
        lock.is_file(),
        "the running instance must hold its lock, or the next launch cannot find it"
    );
    assert_eq!(application.handoffs(), 0);

    // What the second launch does.
    ncui_native::hand_over(port, &token, &["--open".to_owned(), "notes.txt".to_owned()])
        .expect("the running instance is reachable");

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && application.handoffs() == 0 {
        application.pump();
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        application.handoffs(),
        1,
        "the running application must have received the second launch"
    );

    // A hand-off without the token is refused, and the application says so rather than acting
    // on it: the port is reachable by any local process, and only the holder of the lock's token
    // may ask the instance to do anything.
    ncui_native::hand_over(port, "not-the-token", &["--open".to_owned()])
        .expect("the write itself succeeds");
    let refused_at = Instant::now() + Duration::from_secs(5);
    while Instant::now() < refused_at {
        application.pump();
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        application.handoffs(),
        1,
        "an unauthorised hand-off must not count as a launch"
    );

    application.shutdown();
    assert!(
        !lock.exists(),
        "a shut-down instance must release its lock rather than leave a stale one"
    );
}

/// A page that asks the runtime what link it was opened with, and records the answer.
const LINK_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>link</title></head>
<body>
<script>
window.ncui.invoke('app.link', {}).then(function (answer) {
  window.ncui.invoke('record', { asked: 'app.link', answer: answer });
});
</script>
</body></html>"#;

#[test]
fn a_link_that_arrives_by_hand_off_becomes_the_link_the_page_reads() {
    // A deep link that arrives while the application is running: the second launch hands its
    // argument vector over, the instance reloads, and the page asks what it was opened with. The
    // answer has to be the *new* link — an instance that kept answering the first one would be
    // showing the user a link they already acted on.
    let scratch = Scratch::application(MANIFEST, LINK_PAGE);
    let state = InstanceState::new();
    let guard = match ncui_native::InstanceGuard::acquire(&state.path, "end-to-end")
        .expect("the state directory is usable")
    {
        ncui_native::Acquired::First(guard) => *guard,
        ncui_native::Acquired::AlreadyRunning { port, .. } => {
            panic!("a fresh state directory must not name a running instance (port {port})")
        }
    };
    let port = guard.port().expect("a bound listener has a port");
    let token = guard.token().to_owned();

    let (application, recorded) = launch_with_recorder(&scratch);
    let mut application = application
        .with_instance(guard)
        .with_link(Some("ncui-counter://open/first".to_owned()));
    // The first link reaches the page, which records it.
    let first = pump_until(&mut application, &recorded, 1);
    assert_eq!(first[0]["answer"]["link"], "ncui-counter://open/first");

    // Now a second launch, carrying a link, as a registered scheme would.
    ncui_native::hand_over(
        port,
        &token,
        &[
            "run".to_owned(),
            scratch.path().display().to_string(),
            "ncui-counter://open/second".to_owned(),
        ],
    )
    .expect("the running instance is reachable");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut latest = Vec::new();
    while Instant::now() < deadline {
        application.pump();
        latest = recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if latest.len() >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        latest.len() >= 2,
        "the reloaded page must have asked again: {latest:?}"
    );
    assert_eq!(
        latest[1]["answer"]["link"], "ncui-counter://open/second",
        "the page must be told about the link that arrived, not the one it already had"
    );
    assert_eq!(application.handoffs(), 1);
    assert_eq!(
        application.link().as_deref(),
        Some("ncui-counter://open/second")
    );

    application.shutdown();
}

#[test]
fn a_page_reads_the_link_the_application_was_opened_with() {
    // A deep link reaches the process as an argument. The page is the application the link was
    // addressed to, so it can ask what it was — through the bridge, on the page's own initiative,
    // which is what makes it work whatever the page does with the answer.
    let scratch = Scratch::application(MANIFEST, LINK_PAGE);
    let (application, recorded) = launch_with_recorder(&scratch);
    let mut application = application.with_link(Some("ncui-counter://open/notes".to_owned()));

    let entries = pump_until(&mut application, &recorded, 1);
    let answer = &entries[0];
    assert_eq!(answer["asked"], "app.link");
    assert_eq!(answer["answer"]["opened_by_link"], true);
    assert_eq!(answer["answer"]["link"], "ncui-counter://open/notes");

    application.shutdown();
}

#[test]
fn a_page_opened_without_a_link_is_told_so() {
    // The absence is an answer, not a failure: a page that has to distinguish "no link" from
    // "the runtime could not say" would otherwise be guessing.
    let scratch = Scratch::application(MANIFEST, LINK_PAGE);
    let (mut application, recorded) = launch_with_recorder(&scratch);

    let entries = pump_until(&mut application, &recorded, 1);
    assert_eq!(entries[0]["answer"]["opened_by_link"], false);
    assert!(entries[0]["answer"]["link"].is_null());

    application.shutdown();
}

/// A window's geometry, written where an application would have written it.
fn write_geometry(path: &std::path::Path, x: i32, y: i32, width: u32, height: u32) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("the state directory is creatable");
    }
    let geometry = serde_json::json!({ "x": x, "y": y, "width": width, "height": height });
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&geometry).expect("a geometry encodes"),
    )
    .expect("the state file is writable");
}

#[test]
fn a_window_is_put_back_where_it_was_left() {
    // The behaviour every desktop application is expected to have: the user moves and resizes a
    // window, closes it, and the next run puts it back. The state file is what the *last* run
    // wrote, so this test writes one as that run would have and asserts the window is created
    // from it rather than from the manifest.
    let scratch = Scratch::application(MANIFEST, PAGE);
    let state = InstanceState::new();
    let state_file = state.path.join("window.json");
    write_geometry(&state_file, 120, 90, 640, 480);

    let mut application = Application::launch_with(
        scratch.path(),
        Limits::default(),
        Some(state_file.clone()),
        |_| Ok(()),
    )
    .expect("the application launches");
    let summary = application
        .runtime()
        .windows
        .summaries()
        .expect("the window list")
        .into_iter()
        .find(|summary| summary.id == application.window())
        .expect("the application's own window");
    assert_eq!(summary.width, 640, "the remembered width must win");
    assert_eq!(summary.height, 480, "and the remembered height");
    assert_eq!(summary.x, Some(120), "and the remembered place");
    assert_eq!(summary.y, Some(90));

    application.shutdown();
    // Closing the window writes down where it was, so the *next* run has something to restore —
    // which is the half of the loop a test that only restored would miss.
    let written: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&state_file).expect("the state file is written on shutdown"),
    )
    .expect("the state file holds a geometry");
    assert!(
        written["width"].as_u64().expect("a width") >= u64::from(ncui_webview::MIN_WINDOW_WIDTH),
        "the geometry written must be one a window may have: {written}"
    );
    assert!(written["x"].as_i64().is_some(), "{written}");
}

#[test]
fn a_window_with_nothing_remembered_uses_the_manifest() {
    // A first run has no state, and that is not an error: the manifest's size is what a first run
    // gets, and the platform decides where the window goes.
    let scratch = Scratch::application(MANIFEST, PAGE);
    let state = InstanceState::new();
    let state_file = state.path.join("nothing-here.json");

    let mut application =
        Application::launch_with(scratch.path(), Limits::default(), Some(state_file), |_| {
            Ok(())
        })
        .expect("the application launches");
    let summary = application
        .runtime()
        .windows
        .summaries()
        .expect("the window list")
        .into_iter()
        .find(|summary| summary.id == application.window())
        .expect("the application's own window");
    assert_eq!(summary.width, 800, "the manifest's width");
    assert_eq!(summary.height, 600, "the manifest's height");
    assert_eq!(summary.x, None, "and no invented place");

    application.shutdown();
}

#[test]
fn a_remembered_geometry_no_window_could_have_is_ignored() {
    // The state file is a file an application wrote, so it is input: a size below the minimum is
    // ignored rather than handed to the platform, because a window asked for a size it cannot
    // have is a window that does not open.
    let scratch = Scratch::application(MANIFEST, PAGE);
    let state = InstanceState::new();
    let state_file = state.path.join("window.json");
    write_geometry(&state_file, 10, 10, 5, 5);

    let mut application =
        Application::launch_with(scratch.path(), Limits::default(), Some(state_file), |_| {
            Ok(())
        })
        .expect("the application launches");
    let summary = application
        .runtime()
        .windows
        .summaries()
        .expect("the window list")
        .into_iter()
        .find(|summary| summary.id == application.window())
        .expect("the application's own window");
    assert_eq!(
        summary.width, 800,
        "an unusable remembered size must be ignored, not used"
    );

    application.shutdown();
}

/// A page that loads a preference, records what it found, and stores a new one.
const STATE_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>state</title></head>
<body>
<script>
window.ncui.invoke('state.load', { key: 'theme' }).then(function (before) {
  return window.ncui.invoke('state.save', { key: 'theme', value: 'dark' }).then(function (saved) {
    window.ncui.invoke('record', { before: before, saved: saved });
  });
});
</script>
</body></html>"#;

/// A page that tries to store what the store will not hold, and records each refusal.
const REFUSAL_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>state refusals</title></head>
<body>
<script>
function answer(promise) {
  return promise.then(function (ok) { return { accepted: ok }; },
                      function (error) { return { code: error.code, message: error.message }; });
}
Promise.all([
  answer(window.ncui.invoke('state.save', { key: '' })),
  answer(window.ncui.invoke('state.save', { key: 'theme', value: 'x'.repeat(70000) })),
  answer(window.ncui.invoke('state.save', { value: 1 }))
]).then(function (answers) { window.ncui.invoke('record', { answers: answers }); });
</script>
</body></html>"#;

/// Launches an application that records what its page sends, with a state directory of its own.
fn launch_with_state(
    scratch: &Scratch,
    state_file: PathBuf,
) -> (Application, Arc<Mutex<Vec<serde_json::Value>>>) {
    let recorded: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&recorded);
    let application = Application::launch_with(
        scratch.path(),
        Limits::default(),
        Some(state_file),
        |runtime| {
            let _registration = runtime
                .register_page_command(
                    "record",
                    CapabilitySet::empty(),
                    "Record what the page sent.",
                    move |payload| {
                        recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(payload.clone());
                        Ok(serde_json::json!({ "recorded": true }))
                    },
                )
                .map_err(|error| {
                    ncui_core::NcError::new(ncui_core::ErrorKind::Validation {
                        field: "record".to_owned(),
                        reason: error.to_string(),
                    })
                    .operation("test::launch_with_state")
                })?;
            Ok(())
        },
    )
    .expect("the application launches");
    (application, recorded)
}

#[test]
fn a_page_preference_survives_a_restart() {
    // The claim the capability makes, end to end: the page stores a preference through the
    // bridge, and the *next* run of the application reads it back. Nothing is simulated — the
    // first run writes a real file and the second run reads it.
    let scratch = Scratch::application(MANIFEST, STATE_PAGE);
    let state = InstanceState::new();
    let state_file = state.path.join("window.json");

    {
        let (mut application, recorded) = launch_with_state(&scratch, state_file.clone());
        let entries = pump_until(&mut application, &recorded, 1);
        assert_eq!(
            entries[0]["before"]["found"], false,
            "nothing is stored before the page stores anything"
        );
        assert_eq!(entries[0]["saved"]["saved"], true);
        assert_eq!(entries[0]["saved"]["keys"], 1);
        application.shutdown();
    }

    // The second run: a fresh application, the same state directory.
    let (mut second, recorded) = launch_with_state(&scratch, state_file);
    let entries = pump_until(&mut second, &recorded, 1);
    assert_eq!(
        entries[0]["before"]["found"], true,
        "the preference stored by the first run must be there for the second"
    );
    assert_eq!(entries[0]["before"]["value"], "dark");
    second.shutdown();
}

#[test]
fn a_page_cannot_store_what_the_store_will_not_hold() {
    // The bounds are the store's, and they reach the page as refusals rather than as silent
    // truncation: a preference that was quietly cut is worse than one that was refused.
    let scratch = Scratch::application(MANIFEST, REFUSAL_PAGE);
    let state = InstanceState::new();
    let (mut application, recorded) = launch_with_state(&scratch, state.path.join("window.json"));

    let entries = pump_until(&mut application, &recorded, 1);
    let answers = &entries[0]["answers"];
    assert_eq!(
        answers[0]["code"], "state_refused",
        "an empty key is refused"
    );
    assert_eq!(
        answers[1]["code"], "state_refused",
        "a value over its bound is refused"
    );
    assert_eq!(answers[2]["code"], "invalid_params", "a key is required");
    assert!(
        answers[0].get("accepted").is_none(),
        "a refused save must not look like a save: {answers}"
    );
    assert!(
        answers[1]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("over the limit"),
        "the refusal must say which bound: {answers}"
    );
    application.shutdown();
}

#[test]
fn a_remembered_place_on_a_display_that_is_gone_lands_on_one_that_exists() {
    // A geometry stored while a second display was attached puts the window where that display
    // used to be: off-screen, and unreachable for a user who cannot drag what they cannot see.
    // The window is moved onto a display that exists, and keeps the size the user chose.
    let scratch = Scratch::application(MANIFEST, PAGE);
    let state = InstanceState::new();
    let state_file = state.path.join("window.json");
    write_geometry(&state_file, 100_000, 100_000, 640, 480);

    let mut application =
        Application::launch_with(scratch.path(), Limits::default(), Some(state_file), |_| {
            Ok(())
        })
        .expect("the application launches");
    let summary = application
        .runtime()
        .windows
        .summaries()
        .expect("the window list")
        .into_iter()
        .find(|summary| summary.id == application.window())
        .expect("the application's own window");
    assert_eq!(summary.width, 640, "the remembered size is kept");

    let monitors = application.runtime().windows.monitors();
    assert!(
        !monitors.is_empty(),
        "the machine running this test has at least one display"
    );
    let x = summary.x.expect("the window was given a place");
    let y = summary.y.expect("the window was given a place");
    assert!(
        monitors.iter().any(|monitor| monitor.contains(x, y)),
        "the window must land on a display that exists: ({x}, {y}) among {monitors:?}"
    );

    application.shutdown();
}

/// A page that opens a second window at a place it chose, and records where it landed.
///
/// The guard is the same one the window test uses: the second window loads this page too, and
/// without it the test would open a third window and then a fourth.
const PLACED_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>placed</title></head>
<body>
<script>
if (!localStorage.getItem('ncui-placed-test')) {
  localStorage.setItem('ncui-placed-test', '1');
  window.ncui.invoke('create_window', {
    url: window.location.href,
    title: 'placed',
    width: 500,
    height: 400,
    x: 60,
    y: 70
  }).then(function (created) {
    window.ncui.invoke('record', { created: created });
  }).catch(function (error) {
    window.ncui.invoke('record', { failed: error.code });
  });
}
</script>
</body></html>"#;

#[test]
fn a_page_places_a_window_it_creates() {
    // What the page could not do is place a window it opens. A second window is the page's to
    // arrange, and a page that cannot say *where* has to open it wherever the platform puts it
    // and then move it — a visible jump the user did not ask for.
    let scratch = Scratch::application(WINDOW_MANIFEST, PLACED_PAGE);
    let state = InstanceState::new();
    let (mut application, recorded) = launch_with_state(&scratch, state.path.join("window.json"));

    let entries = pump_until(&mut application, &recorded, 1);
    assert_eq!(
        entries[0]["failed"],
        serde_json::Value::Null,
        "the page must be allowed to open a window: {entries:?}"
    );
    let created = entries[0]["created"]["id"]
        .as_str()
        .expect("the page is told the window's id")
        .to_owned();

    let summaries = application
        .runtime()
        .windows
        .summaries()
        .expect("the window list");
    let placed = summaries
        .iter()
        .find(|summary| summary.id.to_string() == created)
        .expect("the window the page created");
    assert_eq!(placed.width, 500, "the size the page asked for");
    assert_eq!(placed.height, 400);
    assert_eq!(placed.x, Some(60), "the place the page asked for");
    assert_eq!(placed.y, Some(70));

    application.shutdown();
}

#[test]
fn a_page_asks_for_the_window_options_it_wants() {
    // The options a page can ask for are the ones a window can be: floating above other windows,
    // without a frame, fixed in size, with a floor it cannot be dragged below. Each is a property
    // the platform applies when the window is created, so the window the page gets is the window
    // it asked for rather than one it has to adjust afterwards.
    let scratch = Scratch::application(WINDOW_MANIFEST, OPTIONS_PAGE);
    let state = InstanceState::new();
    let (mut application, recorded) = launch_with_state(&scratch, state.path.join("window.json"));

    let entries = pump_until(&mut application, &recorded, 1);
    assert_eq!(
        entries[0]["failed"],
        serde_json::Value::Null,
        "the page must be allowed to open a window: {entries:?}"
    );
    let created = entries[0]["created"]["id"]
        .as_str()
        .expect("the page is told the window's id")
        .to_owned();

    let summaries = application
        .runtime()
        .windows
        .summaries()
        .expect("the window list");
    let opened = summaries
        .iter()
        .find(|summary| summary.id.to_string() == created)
        .expect("the window the page created");
    assert!(opened.always_on_top, "the page asked for a floating window");
    assert!(!opened.decorations, "and for one without a frame");
    assert!(!opened.resizable, "and for one the user cannot resize");
    assert_eq!(opened.min_width, Some(300), "and for a floor");
    assert_eq!(opened.min_height, Some(200));
    assert_eq!(opened.width, 600, "and for the size it named");
    assert_eq!(opened.height, 500);

    application.shutdown();
}

/// A page that asks for a window with options, and records whether it was opened.
const OPTIONS_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>options</title></head>
<body>
<script>
if (!localStorage.getItem('ncui-options-test')) {
  localStorage.setItem('ncui-options-test', '1');
  window.ncui.invoke('create_window', {
    url: window.location.href,
    title: 'options',
    width: 600,
    height: 500,
    always_on_top: true,
    decorations: false,
    resizable: false,
    min_width: 300,
    min_height: 200
  }).then(function (created) {
    window.ncui.invoke('record', { created: created });
  }).catch(function (error) {
    window.ncui.invoke('record', { failed: error.code, message: error.message });
  });
}
</script>
</body></html>"#;

/// A manifest that asks for the window options, and a page that records nothing in particular.
const OPTIONS_MANIFEST: &str = r#"{
    "name": "hud",
    "version": "1.0.0",
    "entry": "index.html",
    "assets": "public",
    "window": {
        "title": "hud",
        "width": 500,
        "height": 400,
        "always_on_top": true,
        "decorations": false,
        "resizable": false,
        "min_width": 300,
        "min_height": 200
    }
}"#;

#[test]
fn an_application_asks_for_its_own_window_options_in_its_manifest() {
    // The manifest is where an application says how its window should look, so the options a page
    // can ask for are the options an application can ask for. A manifest that named them and got
    // a window without them would be a manifest nobody could rely on.
    let scratch = Scratch::application(OPTIONS_MANIFEST, PAGE);
    let mut application =
        Application::launch(scratch.path(), Limits::default()).expect("the application launches");
    let summary = application
        .runtime()
        .windows
        .summaries()
        .expect("the window list")
        .into_iter()
        .find(|summary| summary.id == application.window())
        .expect("the application's own window");
    assert!(
        summary.always_on_top,
        "the manifest asked for a floating window"
    );
    assert!(!summary.decorations, "and for one without a frame");
    assert!(!summary.resizable, "and for one the user cannot resize");
    assert_eq!(summary.min_width, Some(300), "and for a floor");
    assert_eq!(summary.min_height, Some(200));
    assert_eq!(summary.width, 500, "and for the size it named");
    assert_eq!(summary.height, 400);
    application.shutdown();
}

#[test]
fn a_floor_added_to_a_manifest_does_not_refuse_a_window_the_user_shrank() {
    // The edge this closes: the user shrank the window before the manifest had a floor, and the
    // remembered size is now below it. The window is raised to the floor rather than refused —
    // a preference that makes an application unlaunchable is worse than a preference that is
    // adjusted, and the request itself refuses a floor above the window's own size.
    let scratch = Scratch::application(OPTIONS_MANIFEST, PAGE);
    let state = InstanceState::new();
    let state_file = state.path.join("window.json");
    write_geometry(&state_file, 40, 50, 250, 220);

    let mut application =
        Application::launch_with(scratch.path(), Limits::default(), Some(state_file), |_| {
            Ok(())
        })
        .expect("the application launches rather than refusing");
    let summary = application
        .runtime()
        .windows
        .summaries()
        .expect("the window list")
        .into_iter()
        .find(|summary| summary.id == application.window())
        .expect("the application's own window");
    assert_eq!(
        summary.width, 300,
        "the window is raised to the floor the manifest names"
    );
    assert_eq!(
        summary.height, 220,
        "the remembered height is above the floor, so it is kept"
    );
    assert_eq!(summary.x, Some(40), "and keeps the place the user left");
    application.shutdown();
}
