//! Live end-to-end evidence for the audio stream runtime wiring: a stream is
//! created through the runtime registry on a real window, a native producer
//! pushes samples through the producer handle, and the truth is read where it
//! lives — the ring's accounting and the device's sync marks.
//!
//! What this test owns, that the backend-level test could not: the runtime
//! wiring itself — stream creation keyed to a real window, the producer
//! handle's escape from the runtime lock, the inspect path, and the teardown
//! lifecycle. The device-level truths (positions, drift, underrun counting)
//! are proven in `ncui-audio-output`'s own live test; this test reuses them
//! rather than re-measuring.
//!
//! The payload is silence throughout — the evidence is positions and
//! accounting, and the owner's machine owes no noise.

#![cfg(all(target_os = "windows", feature = "wry-host"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    reason = "test assertions must abort loudly on failure; evidence is reported through stdout/stderr; the measurement is one linear sequence that splitting would obscure"
)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ncui_core::{Capability, Limits, WindowId};
use ncui_gpu::audio::{AudioChunk, ENGINE_SAMPLE_RATE};
use ncui_gpu::surface::FramePts;
use ncui_runtime::Runtime;
use ncui_runtime::window::hosted_surface;
use ncui_webview::WindowRequest;
use serde_json::{Value, json};

static NEXT_COMMAND: AtomicU64 = AtomicU64::new(1);

fn command_id() -> u64 {
    NEXT_COMMAND.fetch_add(1, Ordering::Relaxed)
}

/// These tests create OS-level windows — one at a time is the honest shape for
/// evidence gathered from a shared window manager.
static TEST_SERIAL: Mutex<()> = Mutex::new(());

fn silent_chunk(pts_value: u64, frames: u64) -> AudioChunk {
    AudioChunk {
        pts: FramePts {
            value: pts_value,
            timescale: ENGINE_SAMPLE_RATE,
        },
        samples: vec![0.0; (frames * 2) as usize],
    }
}

/// A trivial page server — the window needs somewhere real to navigate, and
/// the wiring evidence does not care what the page shows.
fn serve_page() -> (String, ncui_webview::Origin) {
    serve_html("<!doctype html><title>audio-wiring</title><body>audio</body>")
}

/// Serves one HTML body on a loopback port, returning the url and its origin.
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
    window: WindowId,
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

/// Polls a page-side flag until it is set, with the transient-tolerance the
/// devtools endpoint needs. The strictness lives in what the flag says, not in
/// any single connect succeeding.
fn wait_for_flag(
    runtime: &Arc<Mutex<Runtime>>,
    window: WindowId,
    expression: &str,
    timeout: Duration,
) -> Value {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match evaluate(runtime, window, expression) {
            Ok(value) if !value.is_null() => return value,
            Ok(_) | Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Ok(_) => panic!("the flag never set within {}s", timeout.as_secs()),
            Err(error) => panic!("the flag could not be read: {error}"),
        }
    }
}

#[test]
fn a_stream_created_on_a_window_flows_samples_and_tears_down() {
    let _serial = TEST_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (url, origin) = serve_page();
    let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
    let actions = runtime.page_commands.actions();
    let grant = runtime.page_grant();
    runtime.windows = hosted_surface(&[origin], &actions, grant);
    let window: WindowId = runtime
        .windows
        .open(
            &WindowRequest::new(&url).with_title("ncui-audio-wiring-proof"),
            None,
        )
        .expect("the window opens");

    // Creation: the stream exists and inspect answers — the registry wiring.
    let stream = runtime
        .create_audio_stream(window)
        .expect("the stream creates on a live window");
    let (accounting, _) = runtime
        .inspect_audio_stream(window, stream)
        .expect("a created stream inspects");
    eprintln!("created stream {stream}: initial accounting {accounting:?}");

    // The producer path: take the ring once through the producer handle — the
    // convoy-proof shape — and push two seconds of continuous silence at the
    // natural rate.
    let producer = runtime.audio_stream_producer();
    let ring = producer
        .ring(window, stream)
        .expect("the producer reaches the ring");
    let mut next_pts = 0u64;
    let chunk_frames = 480u64; // 10ms at 48kHz
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        {
            let mut ring = ring
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if ring.push(&silent_chunk(next_pts, chunk_frames)).is_err() {
                drop(ring);
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
        }
        next_pts += chunk_frames;
        std::thread::sleep(Duration::from_millis(8));
    }

    // The flow truth: positions reached the device, and the sync reading
    // carries the stream's own clock truth.
    let (accounting, sync) = runtime
        .inspect_audio_stream(window, stream)
        .expect("the stream still inspects mid-flow");
    eprintln!("mid-flow accounting {accounting:?} sync {sync:?}");
    let sync = sync.expect("a running device stream produces sync marks");
    let (value, timescale) = sync
        .position
        .expect("positions flow through the runtime-wired stream");
    assert_eq!(timescale, ENGINE_SAMPLE_RATE);
    assert!(
        value < u64::from(ENGINE_SAMPLE_RATE) * 3,
        "position {value} ran ahead of the timeline"
    );

    // Teardown: the stream dies, and inspect refuses honestly.
    runtime
        .teardown_audio_stream(window, stream)
        .expect("teardown succeeds");
    assert!(
        runtime.inspect_audio_stream(window, stream).is_none(),
        "a torn-down stream must not inspect"
    );

    eprintln!("audio wiring evidence: create, flow, inspect, teardown — all green");
}

/// The page-facing seam proof: the page declares the stream through
/// `ncui.invoke`, controls it, inspects it and tears it down — every layer of
/// the seam path in the evidence, with the host-side producer draining the
/// intents in exact order.
#[test]
fn a_page_declared_stream_flows_control_and_samples_end_to_end() {
    let _serial = TEST_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (url, origin) = serve_html(
        r"<!doctype html><title>audio-seam</title><body>
<script>
window.__createResult = null;
window.__controlResult = null;
window.__inspectResult = null;
window.__teardownResult = null;
window.__run = async () => {
  try {
    const created = await window.ncui.invoke('audio_stream_create', {});
    window.__createResult = { ok: true, stream: created.stream };
    await window.ncui.invoke('audio_stream_control', { stream: created.stream, intent: 'pause' });
    await window.ncui.invoke('audio_stream_control', { stream: created.stream, intent: 'seek', pts: 96000, timescale: 48000 });
    await window.ncui.invoke('audio_stream_control', { stream: created.stream, intent: 'resume' });
    window.__controlResult = { ok: true };
  } catch (error) {
    window.__createResult = window.__createResult || { ok: false, error: String(error) };
    window.__controlResult = window.__controlResult || { ok: false, error: String(error) };
  }
};
window.__inspect = async () => {
  try {
    const answer = await window.ncui.invoke('audio_stream_inspect', { stream: window.__createResult.stream });
    window.__inspectResult = { ok: true, answer };
  } catch (error) {
    window.__inspectResult = { ok: false, error: String(error) };
  }
};
window.__teardown = async () => {
  try {
    await window.ncui.invoke('audio_stream_teardown', { stream: window.__createResult.stream });
    window.__teardownResult = { ok: true };
  } catch (error) {
    window.__teardownResult = { ok: false, error: String(error) };
  }
};
window.addEventListener('load', () => {
  const start = () => {
    if (window.ncui) { window.__run(); } else { setTimeout(start, 50); }
  };
  start();
});
</script>
audio seam</body>",
    );
    let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
    // The stream actions require the audio capability; inspect requires the
    // runtime-inspection grant. The test's page is the application's own,
    // granted as an application would grant it.
    runtime.grant_page_capabilities(&[Capability::Audio, Capability::InspectRuntime]);
    let actions = runtime.page_commands.actions();
    let grant = runtime.page_grant();
    runtime.windows = hosted_surface(&[origin], &actions, grant);
    let window: WindowId = runtime
        .windows
        .open(
            &WindowRequest::new(&url).with_title("ncui-audio-seam-proof"),
            None,
        )
        .expect("the window opens");

    // The production-shape pump: a dedicated thread serving the page
    // continuously, because every interleaved design starves.
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

    // Wait for the page to exist, then for its driver to finish creating the
    // stream and sending the control sequence.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match evaluate(&runtime, window, "true") {
            Ok(_) => break,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(error) => panic!("window never became evaluable: {error}"),
        }
    }

    let created = wait_for_flag(
        &runtime,
        window,
        "window.__createResult",
        Duration::from_secs(60),
    );
    assert_eq!(
        created.get("ok"),
        Some(&Value::Bool(true)),
        "the page's create must succeed — the whole seam path is the evidence: {created}"
    );
    let stream = created
        .get("stream")
        .and_then(Value::as_u64)
        .expect("the answer carries the stream id");

    let controlled = wait_for_flag(
        &runtime,
        window,
        "window.__controlResult",
        Duration::from_secs(30),
    );
    assert_eq!(
        controlled.get("ok"),
        Some(&Value::Bool(true)),
        "the control sequence must be accepted: {controlled}"
    );

    // The control truth, host-side: the intents drain in exact order — pause,
    // seek to 96000/48000, resume — never coalesced, never reordered.
    let intents = {
        let runtime = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.drain_audio_control(window, stream)
    };
    assert_eq!(
        intents.len(),
        3,
        "three intents were sent; exactly three must drain: {intents:?}"
    );
    assert!(
        matches!(intents[0], ncui_gpu::surface::ControlIntent::Pause),
        "first intent is pause: {:?}",
        intents[0]
    );
    match &intents[1] {
        ncui_gpu::surface::ControlIntent::SeekTo(pts) => {
            assert_eq!(pts.value, 96000);
            assert_eq!(pts.timescale, 48000);
        }
        other => panic!("second intent is the seek: {other:?}"),
    }
    assert!(
        matches!(intents[2], ncui_gpu::surface::ControlIntent::Resume),
        "third intent is resume: {:?}",
        intents[2]
    );

    // The sample path: push through the producer handle, then have the page
    // inspect — positions and accounting answering through the seam.
    let producer = {
        let runtime = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.audio_stream_producer()
    };
    let ring = producer
        .ring(window, stream)
        .expect("the producer reaches the ring");
    {
        let mut ring = ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ring.push(&silent_chunk(0, 4800))
            .expect("a hundred milliseconds of silence pushes");
    }
    std::thread::sleep(Duration::from_millis(300));
    evaluate(&runtime, window, "window.__inspect()").ok();
    let inspected = wait_for_flag(
        &runtime,
        window,
        "window.__inspectResult",
        Duration::from_secs(30),
    );
    assert_eq!(
        inspected.get("ok"),
        Some(&Value::Bool(true)),
        "inspect must answer through the seam: {inspected}"
    );
    let answer = inspected.get("answer").expect("the answer exists");
    assert!(
        answer.get("underruns").and_then(Value::as_u64).is_some(),
        "the accounting shape is the contract's: {answer}"
    );
    assert!(
        answer.pointer("/sync/deviceDelayMicros").is_some(),
        "the sync truth rides the inspect answer: {answer}"
    );

    // Teardown through the seam, verified host-side.
    evaluate(&runtime, window, "window.__teardown()").ok();
    let torn = wait_for_flag(
        &runtime,
        window,
        "window.__teardownResult",
        Duration::from_secs(30),
    );
    assert_eq!(
        torn.get("ok"),
        Some(&Value::Bool(true)),
        "teardown through the seam must succeed: {torn}"
    );
    let gone = {
        let runtime = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.inspect_audio_stream(window, stream).is_none()
    };
    assert!(gone, "a seam-torn-down stream must not inspect");

    eprintln!("audio seam evidence: create, control order, inspect shape, teardown — all green");
}
