//! Live evidence for `native.os_file_drop`: a real window, the real host event
//! loop, and the page-side DOM event read back through CDP.
//!
//! The OLE drag itself is wry's tested machinery; what this test owns is the
//! chain nc-ui added — wry's drag-drop handler forwards into the host loop,
//! the loop applies the `filedrop` disclosure policy, and the page receives a
//! `ncui:file-drop` `CustomEvent` whose paths are normalized and whose position
//! is the drop's own.
//!
//! The engine-window identity is deterministic here by construction: this
//! binary creates exactly one window, and `NEXT_WINDOW` in the host starts at
//! 1 — so the window under test is engine id 1. That is why the test lives in
//! its own binary rather than sharing one with other window-creating tests.

#![cfg(all(target_os = "windows", feature = "wry-host"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicU64, Ordering};

use ncui_core::{Limits, WindowId};
use ncui_runtime::Runtime;
use ncui_runtime::window::hosted_surface;
use ncui_webview::{Origin, Scheme, WindowRequest};
use serde_json::{Value, json};

static NEXT_COMMAND: AtomicU64 = AtomicU64::new(1);

fn command_id() -> u64 {
    NEXT_COMMAND.fetch_add(1, Ordering::Relaxed)
}

fn serve_page() -> (String, Origin) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the local page server");
    let port = listener.local_addr().expect("server address").port();
    std::thread::spawn(move || {
        for _ in 0..16 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let body = "<!doctype html><title>drop-target</title><body>drop target</body>";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (
        format!("http://127.0.0.1:{port}/"),
        Origin::new(Scheme::Http, "127.0.0.1", Some(port)),
    )
}

fn cdp(runtime: &Runtime, window: WindowId, method: &str, params: &Value) -> Result<Value, String> {
    runtime
        .windows
        .cdp_send(
            window,
            &json!({
                "id": command_id(),
                "method": method,
                "params": params.clone(),
            }),
        )
        .map_err(|rejection| format!("window {window} rejected the CDP request: {rejection:?}"))?
        .map_err(|error| error.to_string())
}

fn evaluate(runtime: &Runtime, window: WindowId, expression: &str) -> Result<Value, String> {
    let answer = cdp(
        runtime,
        window,
        "Runtime.evaluate",
        &json!({ "expression": expression, "returnByValue": true }),
    )?;
    if answer.get("exceptionDetails").is_some() {
        return Err(format!("Runtime.evaluate failed: {answer}"));
    }
    answer
        .pointer("/result/result/value")
        .cloned()
        .ok_or_else(|| format!("Runtime.evaluate returned no value: {answer}"))
}

#[test]
fn a_file_drop_reaches_the_page_as_a_dom_event() {
    let (url, origin) = serve_page();
    let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
    let actions = runtime.page_commands.actions();
    let grant = runtime.page_grant();
    runtime.windows = hosted_surface(&[origin], &actions, grant);
    let window = runtime
        .windows
        .open(
            &WindowRequest::new(&url).with_title("ncui-filedrop-proof"),
            None,
        )
        .expect("the window opens");

    // Wait until CDP can evaluate in the page, then install the capture hook.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match evaluate(&runtime, window, "true") {
            Ok(_) => break,
            Err(error) if Instant::now() < deadline => {
                assert!(
                    error.contains("cdp_target_bind_failed"),
                    "window never became evaluable: {error}"
                );
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(error) => panic!("window never became evaluable: {error}"),
        }
    }
    evaluate(
        &runtime,
        window,
        "window.__drops = []; window.addEventListener('ncui:file-drop', (e) => window.__drops.push(e.detail)); true",
    )
    .expect("the capture hook installs");

    // Inject the drop through the host loop exactly as the wry handler does.
    // This window is engine id 1 — the binary's first and only window.
    let proxy = ncui_webview_wry::engine_proxy().expect("the engine is running");
    proxy
        .send_event(ncui_webview_wry::HostEvent::FileDrop {
            window: 1,
            paths: vec![
                std::path::PathBuf::from("C:\\media\\clip-one.mp4"),
                std::path::PathBuf::from("C:\\media\\x\\..\\clip-two.mp4"),
            ],
            position: (120, 80),
        })
        .expect("the drop event enters the loop");

    // The page's capture must show both paths, the second normalized, with the
    // drop position — the whole chain proven in a real window.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let captured = evaluate(&runtime, window, "JSON.stringify(window.__drops)")
            .expect("the capture reads back");
        let text = captured.as_str().unwrap_or_default().to_owned();
        if !text.is_empty() && text != "[]" {
            assert!(text.contains("clip-one.mp4"), "first path present: {text}");
            assert!(
                text.contains("C:\\\\media\\\\clip-two.mp4"),
                "second path normalized upward: {text}"
            );
            assert!(
                !text.contains(".."),
                "no unresolved parent survives: {text}"
            );
            assert!(text.contains("[120,80]"), "the drop position: {text}");
            return;
        }
        assert!(Instant::now() < deadline, "the drop never reached the page");
        std::thread::sleep(Duration::from_millis(250));
    }
}
