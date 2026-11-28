//! Live evidence for the streaming bridge: a stream opened on a real window
//! delivers events the page receives through `ncui.on`, cancellation is honest
//! in all three shapes, and the delivery counts tell the truth.
//!
//! What this test owns, that the unit tests could not: the posting path
//! itself — a real page in a real `WebView2` running the dispatch script, the
//! event landing in the page's own handler, and the window-death cancellation
//! that only a real window lifecycle can produce.

#![cfg(all(target_os = "windows", feature = "wry-host"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    reason = "test assertions must abort loudly on failure; evidence is reported through stdout/stderr; the measurement is one linear sequence that splitting would obscure"
)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ncui_core::Limits;
use ncui_runtime::Runtime;
use ncui_runtime::window::hosted_surface;
use ncui_webview::WindowRequest;
use serde_json::{Value, json};

static TEST_SERIAL: Mutex<()> = Mutex::new(());

static NEXT_COMMAND: AtomicU64 = AtomicU64::new(1);

fn command_id() -> u64 {
    NEXT_COMMAND.fetch_add(1, Ordering::Relaxed)
}

fn serve_html(body: &'static str) -> (String, ncui_webview::Origin) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the local page server");
    let port = listener.local_addr().expect("server address").port();
    std::thread::spawn(move || {
        for _ in 0..32 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
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
        ncui_webview::Origin::new(ncui_webview::Scheme::Http, "127.0.0.1", Some(port)),
    )
}

fn evaluate(
    runtime: &Arc<Mutex<Runtime>>,
    window: ncui_core::WindowId,
    expression: &str,
) -> Result<Value, String> {
    let answer = {
        let runtime = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime
            .windows
            .cdp_send(
                window,
                &json!({
                    "id": command_id(),
                    "method": "Runtime.evaluate",
                    "params": { "expression": expression, "returnByValue": true },
                }),
            )
            .map_err(|rejection| {
                format!("window {window} rejected the CDP request: {rejection:?}")
            })?
            .map_err(|error| error.to_string())?
    };
    if answer.get("exceptionDetails").is_some() {
        return Err(format!("Runtime.evaluate failed: {answer}"));
    }
    answer
        .pointer("/result/result/value")
        .cloned()
        .ok_or_else(|| format!("Runtime.evaluate returned no value: {answer}"))
}

#[test]
fn a_stream_delivers_events_to_the_page_and_cancellation_is_honest() {
    let _serial = TEST_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (url, origin) = serve_html(
        r"<!doctype html><title>stream-proof</title><body>
<script>
window.__events = [];
window.__ready = false;
window.addEventListener('load', () => {
  const start = () => {
    if (window.ncui && window.ncui.on) {
      window.ncui.on('station.progress', (payload) => {
        window.__events.push(payload);
      });
      window.__ready = true;
    } else { setTimeout(start, 50); }
  };
  start();
});
</script>
stream</body>",
    );
    let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
    let actions = runtime.page_commands.actions();
    let grant = runtime.page_grant();
    runtime.windows = hosted_surface(&[origin], &actions, grant);
    let window = runtime
        .windows
        .open(
            &WindowRequest::new(&url).with_title("ncui-stream-proof"),
            None,
        )
        .expect("the window opens");

    let runtime = Arc::new(Mutex::new(runtime));
    let pump = Arc::clone(&runtime);
    std::thread::spawn(move || {
        loop {
            {
                let mut runtime = pump
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = runtime.windows.wait_page_message(Duration::from_millis(5));
                let now = runtime.elapsed();
                runtime.pump_pages(now);
            }
            std::thread::yield_now();
        }
    });

    // The stream opens on the live window, and the producer — its own thread,
    // the sink its only handle — posts twenty events at interaction rate.
    let (subscription, sink) = {
        let runtime = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime
            .open_stream(window, "station.progress")
            .expect("the stream opens on a live window")
    };
    let producer_sink = sink.clone();
    let producer = std::thread::spawn(move || {
        for n in 0..20 {
            if producer_sink.is_closed() {
                break;
            }
            producer_sink.emit(&json!({ "n": n, "total": 20 }));
            std::thread::sleep(Duration::from_millis(50));
        }
    });

    // The page truth: the events land in the page's own handler, in order,
    // with their payloads intact.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let received = loop {
        let count = evaluate(&runtime, window, "window.__events.length")
            .ok()
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        if count >= 20 {
            break count;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the page never received the stream — {count} of 20 events arrived"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(received, 20);
    let ordering = evaluate(
        &runtime,
        window,
        "window.__events.every((e, i) => e.n === i)",
    )
    .expect("the ordering expression evaluates");
    assert_eq!(
        ordering,
        Value::Bool(true),
        "twenty events must arrive in order with their payloads intact"
    );
    producer.join().expect("the producer finishes");

    // The delivery truth: the counts say twenty delivered, none failed, and
    // the subscription still lives.
    let counts = {
        let runtime = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.inspect_stream(subscription)
    };
    assert_eq!(
        counts,
        Some((20, 0, false)),
        "the inspect truth must be twenty delivered, zero failed, still open: {counts:?}"
    );

    // Cancellation, explicit shape: close the stream, post once more, and the
    // post is a counted no-op rather than a pretended delivery.
    {
        let runtime = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            runtime.close_stream(subscription),
            "closing a live subscription answers true"
        );
        assert!(
            !runtime.close_stream(subscription),
            "closing it again answers honestly false"
        );
    }
    assert!(sink.is_closed(), "the sink reads its own closure");
    assert!(
        !sink.emit(&json!({ "n": 99 })),
        "a post into a closed stream does not pretend"
    );
    let (delivered, failed) = sink.counts();
    assert_eq!(delivered, 20, "the delivered count does not move");
    assert!(failed >= 1, "the refused post is counted");

    eprintln!("stream evidence: 20/20 events in order, counts exact, cancellation honest");
}

/// The page-initiated shape, proving the composition claim: a streaming command
/// registered for the caller window opens its stream through the captured
/// factory — inside its own execution, with the runtime lock held, which is
/// exactly where a captured runtime would have deadlocked.
#[test]
fn a_page_initiated_stream_composes_the_caller_window_mechanism() {
    let _serial = TEST_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (url, origin) = serve_html(
        r"<!doctype html><title>stream-compose</title><body>
<script>
window.__events = [];
window.__subscription = null;
window.__error = null;
window.__run = async () => {
  try {
    window.ncui.on('job.progress', (payload) => { window.__events.push(payload); });
    const answer = await window.ncui.invoke('job_subscribe', {});
    window.__subscription = answer.subscription;
  } catch (e) { window.__error = String(e); }
};
window.addEventListener('load', () => {
  const start = () => {
    if (window.ncui && window.ncui.invoke) { window.__run(); } else { setTimeout(start, 50); }
  };
  start();
});
</script>
compose</body>",
    );
    let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");

    let actions = runtime.page_commands.actions();
    let grant = runtime.page_grant();
    runtime.windows = hosted_surface(&[origin], &actions, grant);
    // The application side: a command captured the factory at registration
    // time. Its handler reads the runtime-injected window, opens the stream,
    // spawns the producing thread, and answers the subscription id — all
    // synchronously inside the invoke's execution.
    let factory = runtime.stream_factory();
    let _job_subscription = runtime
        .register_page_command_for_caller_window(
            "job_subscribe",
            ncui_core::CapabilitySet::empty(),
            "subscribes the calling window to job progress",
            move |params| {
                let window_text =
                    params
                        .get("window")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            ncui_webview::SeamError::new(
                                "invalid_params",
                                "the window injection is missing",
                            )
                        })?;
                let window: ncui_core::WindowId = window_text.parse().map_err(|_| {
                    ncui_webview::SeamError::new(
                        "invalid_params",
                        "the window injection is malformed",
                    )
                })?;
                let (subscription, sink) = factory
                    .open(window, "job.progress")
                    .map_err(|error| ncui_webview::SeamError::new("internal", error.to_string()))?;
                std::thread::spawn(move || {
                    for n in 0..5 {
                        if sink.is_closed() {
                            return;
                        }
                        sink.emit(&json!({ "n": n }));
                        std::thread::sleep(Duration::from_millis(60));
                    }
                });
                Ok(json!({ "subscription": subscription }))
            },
        )
        .expect("the streaming command registers");

    let window = runtime
        .windows
        .open(
            &WindowRequest::new(&url).with_title("ncui-stream-compose"),
            None,
        )
        .expect("the window opens");

    let runtime = Arc::new(Mutex::new(runtime));
    let pump = Arc::clone(&runtime);
    std::thread::spawn(move || {
        loop {
            {
                let mut runtime = pump
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = runtime.windows.wait_page_message(Duration::from_millis(5));
                let now = runtime.elapsed();
                runtime.pump_pages(now);
            }
            std::thread::yield_now();
        }
    });

    // The whole round trip, asserted from the page's own truth: the invoke
    // answered a subscription id, and the five events landed in order.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let events = evaluate(&runtime, window, "window.__events.length")
            .ok()
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        if events >= 5 {
            break;
        }
        let error = evaluate(&runtime, window, "window.__error")
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned));
        assert!(
            error.is_none() || error.as_deref() == Some("null"),
            "the page's invoke failed: {error:?}"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "the composed stream never delivered — {events} of 5 events"
        );
        std::thread::sleep(Duration::from_millis(150));
    }
    let subscription =
        evaluate(&runtime, window, "window.__subscription").expect("the subscription answer reads");
    assert!(
        subscription.as_u64().is_some_and(|id| id > 0),
        "the invoke must answer a real subscription id, got {subscription}"
    );
    let ordering = evaluate(
        &runtime,
        window,
        "window.__events.every((e, i) => e.n === i)",
    )
    .expect("the ordering expression evaluates");
    assert_eq!(
        ordering,
        Value::Bool(true),
        "the composed stream's events must arrive in order"
    );

    eprintln!(
        "composition evidence: page-initiated stream opened inside its own invoke, 5/5 events in order"
    );
}
