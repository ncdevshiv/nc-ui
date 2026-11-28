//! Real `WebView2` page-target isolation through the runtime's CDP seam.
//!
//! Two windows share one browser endpoint but must never share a page target. Each write and
//! read below goes through the runtime id of the intended window, so a browser-scoped fallback
//! or a swapped target binding fails the assertions.

#![cfg(all(target_os = "windows", feature = "wry-host"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ncui_core::{Limits, WindowId};
use ncui_runtime::Runtime;
use ncui_runtime::window::hosted_surface;
use ncui_webview::{Origin, Scheme, WindowRequest};
use serde_json::{Value, json};

static NEXT_COMMAND: AtomicU64 = AtomicU64::new(1);

const PAGE_BODY: &str = "<!doctype html><title>CDP isolation</title><button id=\"target\" onclick=\"window.__ncuiCdpClicks=(window.__ncuiCdpClicks||0)+1\">target</button>";

fn command_id() -> u64 {
    NEXT_COMMAND.fetch_add(1, Ordering::Relaxed)
}

fn local_page() -> (String, Origin) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the local page server");
    let port = listener.local_addr().expect("server address").port();
    std::thread::spawn(move || {
        for _ in 0..8 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("bound the request read");
            let mut reader = BufReader::new(stream.try_clone().expect("clone the request stream"));
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).expect("read the request line") == 0
                    || line == "\r\n"
                {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PAGE_BODY}",
                PAGE_BODY.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("serve the local page");
        }
    });
    (
        format!("http://127.0.0.1:{port}/"),
        Origin::new(Scheme::Http, "127.0.0.1", Some(port)),
    )
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "test helper — callers build owned json! values inline; a reference would add noise without saving a clone the callers do not have"
)]
fn cdp(runtime: &Runtime, window: WindowId, method: &str, params: Value) -> Result<Value, String> {
    runtime
        .windows
        .cdp_send(
            window,
            &json!({
                "id": command_id(),
                "method": method,
                "params": params,
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
        json!({ "expression": expression, "returnByValue": true }),
    )?;
    if answer.get("exceptionDetails").is_some() {
        return Err(format!("Runtime.evaluate failed: {answer}"));
    }
    answer
        .pointer("/result/result/value")
        .cloned()
        .ok_or_else(|| format!("Runtime.evaluate returned no value: {answer}"))
}

/// Injects one click through the page target's own CDP channel.
///
/// This is the supported input path for interactive tests: OS-level synthesized input does
/// not reliably reach `WebView2`'s embedded browser window, while CDP input is delivered to
/// the same page target the test subsequently inspects.
fn click(runtime: &Runtime, window: WindowId, x: u32, y: u32) -> Result<(), String> {
    for event in ["mouseMoved", "mousePressed", "mouseReleased"] {
        cdp(
            runtime,
            window,
            "Input.dispatchMouseEvent",
            json!({ "type": event, "x": x, "y": y, "button": "left", "clickCount": 1 }),
        )?;
    }
    Ok(())
}

fn wait_until_evaluable(runtime: &Runtime, window: WindowId) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match evaluate(
            runtime,
            window,
            "window.__ncuiCdpIsolationReady = true; true",
        ) {
            Ok(Value::Bool(true)) => return,
            Ok(value) => panic!("window {window} returned an unexpected readiness value: {value}"),
            Err(error) if Instant::now() < deadline => {
                // A newly built page may not have an execution context yet. Target-binding
                // failures are not retried: those must fail closed rather than be hidden.
                assert!(
                    !error.contains("cdp_target_bind_failed"),
                    "window {window} has no page-target binding: {error}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("window {window} never became evaluable: {error}"),
        }
    }
}

#[test]
fn two_windows_keep_runtime_evaluate_on_their_own_pages() {
    let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
    let actions = runtime.page_commands.actions();
    let grant = runtime.page_grant();
    let (url, origin) = local_page();
    runtime.windows = hosted_surface(&[origin], &actions, grant);

    let first = runtime
        .windows
        .open(&WindowRequest::new(&url), None)
        .expect("the first real window opens");
    let second = runtime
        .windows
        .open(&WindowRequest::new(&url), None)
        .expect("the second real window opens");
    assert_ne!(first, second, "the runtime must issue distinct window ids");

    wait_until_evaluable(&runtime, first);
    wait_until_evaluable(&runtime, second);

    evaluate(
        &runtime,
        first,
        "window.__ncuiCdpIsolation = 'first-page'; window.__ncuiCdpIsolation",
    )
    .expect("the first page receives its distinct value");
    evaluate(
        &runtime,
        second,
        "window.__ncuiCdpIsolation = 'second-page'; window.__ncuiCdpIsolation",
    )
    .expect("the second page receives its distinct value");

    assert_eq!(
        evaluate(&runtime, first, "window.__ncuiCdpIsolation")
            .expect("the first host reads its own page"),
        Value::String("first-page".to_owned())
    );
    assert_eq!(
        evaluate(&runtime, second, "window.__ncuiCdpIsolation")
            .expect("the second host reads its own page"),
        Value::String("second-page".to_owned())
    );
}

#[test]
fn cdp_mouse_input_reaches_the_page_target_it_inspects() {
    let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
    let actions = runtime.page_commands.actions();
    let grant = runtime.page_grant();
    let (url, origin) = local_page();
    runtime.windows = hosted_surface(&[origin], &actions, grant);

    let window = runtime
        .windows
        .open(&WindowRequest::new(&url), None)
        .expect("the real window opens");
    wait_until_evaluable(&runtime, window);

    click(&runtime, window, 24, 24).expect("CDP delivers the click");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if evaluate(&runtime, window, "window.__ncuiCdpClicks || 0") == Ok(json!(1)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the click never reached the page"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
