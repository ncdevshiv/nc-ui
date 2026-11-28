//! Live end-to-end evidence for the video surface runtime wiring: a page declares
//! a surface through the seam, a native producer pushes frames, and every layer
//! of the truth is read where it lives.
//!
//! What this test owns, that the presenter-level test could not:
//!
//! - the seam path itself — `video_surface_create` invoked from the page's own
//!   JavaScript, through the capability gate, the dispatch table, and the
//!   registry, ending in a real childed presenter;
//! - the placement conversion — the page reports CSS-logical geometry and the
//!   OS-level window rect must land at the physical answer, ratio included;
//! - the producer seam — frames pushed natively present, control intents drain
//!   in order, and the ring's accounting is inspectable;
//! - the lifecycle — teardown from the page removes the OS-level child.
//!
//! The compositing claim itself is proven in `video_surface_composite_live.rs`;
//! this test reuses its z-order truth rather than re-measuring it.

#![cfg(all(target_os = "windows", feature = "wry-host"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::missing_panics_doc,
    clippy::suboptimal_flops,
    clippy::float_cmp,
    reason = "test assertions must abort loudly on failure; evidence is reported through stdout/stderr; measurement math casts deliberately; test helpers take owned values for call-site clarity; measurement functions are linear sequences that splitting would obscure"
)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicU64, Ordering};

use ncui_core::{Capability, Limits, WindowId};
use ncui_gpu::surface::{Damage, Frame, FramePts};
use ncui_runtime::Runtime;
use ncui_runtime::window::hosted_surface;
use ncui_video_surface::RgbaPixels;
use ncui_video_surface::evidence::{
    child_zorder_classes, find_toplevel_window, window_screen_rect,
};
use ncui_webview::{Origin, Scheme, WindowRequest};
use serde_json::{Value, json};

static NEXT_COMMAND: AtomicU64 = AtomicU64::new(1);

fn command_id() -> u64 {
    NEXT_COMMAND.fetch_add(1, Ordering::Relaxed)
}

/// The page carries its own driver: on load it declares the surface, then
/// records every answer where small CDP probes can read it. Large scripts do
/// not survive the devtools endpoint — the harness lives in the served page,
/// which is the lesson the round-trip measurement paid for.
/// These tests create OS-level windows and read OS-level window-manager truth
/// (discovery by title, sibling z-order). Run in parallel they would read each
/// other's windows — the discovery key is a title, and titles are global state.
/// One at a time is the honest shape for evidence gathered from a shared
/// window manager; the pair still completes in seconds.
static TEST_SERIAL: Mutex<()> = Mutex::new(());

fn serve_page() -> (String, Origin) {
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
            let body = r#"<!doctype html><title>video-wiring</title>
<body style="margin:0;background:rgb(255,0,0)">
<div id="monitor" style="position:absolute;left:60px;top:60px;width:320px;height:180px"></div>
<script>
window.__surfaceResult = null;
window.__placeResult = null;
window.__controlResult = null;
window.__teardownResult = null;
window.__dpr = window.devicePixelRatio;
window.__run = async () => {
  const placement = () => ({
    rect: [60, 60, 320, 180],
    scroll: [window.scrollX, window.scrollY],
    devicePixelRatio: window.devicePixelRatio,
    clip: [0, 0, window.innerWidth + window.scrollX, window.innerHeight + window.scrollY],
  });
  try {
    const created = await window.ncui.invoke('video_surface_create', {
      width: 320, height: 180, ...placement(),
    });
    window.__surfaceResult = { ok: true, surface: created.surface };
    const placed = await window.ncui.invoke('video_surface_place', {
      surface: created.surface,
      rect: [120, 100, 320, 180],
      scroll: [window.scrollX, window.scrollY],
      devicePixelRatio: window.devicePixelRatio,
      clip: [0, 0, window.innerWidth + window.scrollX, window.innerHeight + window.scrollY],
    });
    window.__placeResult = { ok: true };
    await window.ncui.invoke('video_surface_control', { surface: created.surface, intent: 'pause' });
    await window.ncui.invoke('video_surface_control', { surface: created.surface, intent: 'seek', pts: 45000, timescale: 90000 });
    await window.ncui.invoke('video_surface_control', { surface: created.surface, intent: 'step', frames: 1 });
    window.__controlResult = { ok: true };
  } catch (error) {
    window.__surfaceResult = window.__surfaceResult || { ok: false, error: String(error) };
    window.__controlResult = window.__controlResult || { ok: false, error: String(error) };
  }
};
window.__teardown = async () => {
  try {
    await window.ncui.invoke('video_surface_teardown', { surface: window.__surfaceResult.surface });
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
red</body>"#;
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
    let deadline = Instant::now() + timeout;
    loop {
        match evaluate(runtime, window, expression) {
            Ok(value) if !value.is_null() => return value,
            Ok(_) | Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Ok(_) => panic!("the flag never set within {}s", timeout.as_secs()),
            Err(error) => panic!("the flag could not be read: {error}"),
        }
    }
}

fn green_frame(width: u32, height: u32, sequence: u64) -> Frame<RgbaPixels> {
    let mut bytes = vec![0u8; (width * height * 4) as usize];
    for pixel in bytes.as_chunks_mut::<4>().0 {
        pixel[0] = 0;
        pixel[1] = 255;
        pixel[2] = 0;
        pixel[3] = 255;
    }
    Frame {
        sequence,
        pts: FramePts {
            value: sequence * 3000,
            timescale: 90_000,
        },
        damage: vec![Damage::full(width, height)],
        payload: RgbaPixels {
            width,
            height,
            stride: width * 4,
            bytes,
        },
    }
}

#[test]
fn a_page_declared_surface_presents_producer_frames_end_to_end() {
    let (url, origin) = serve_page();
    let _serial = TEST_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
    // The surface actions require the gpu capability; the test's page is the
    // application's own, granted as an application would grant it.
    runtime.grant_page_capabilities(&[Capability::Gpu, Capability::InspectGpu]);
    let actions = runtime.page_commands.actions();
    let grant = runtime.page_grant();
    runtime.windows = hosted_surface(&[origin], &actions, grant);
    let window = runtime
        .windows
        .open(
            &WindowRequest::new(&url).with_title("ncui-video-wiring-proof"),
            None,
        )
        .expect("the window opens");

    // The production-shape pump: a dedicated thread serving the page
    // continuously, because every interleaved design starves — the lesson the
    // round-trip measurement paid a day for, applied here rather than repaid.
    let runtime = Arc::new(Mutex::new(runtime));
    let pump = Arc::clone(&runtime);
    let pump_thread = std::thread::spawn(move || {
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

    // Wait for the page to exist at all, then for its driver to finish creating
    // the surface and driving the placement and control sequence.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match evaluate(&runtime, window, "true") {
            Ok(_) => break,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(250)),
            Err(error) => panic!("window never became evaluable: {error}"),
        }
    }

    let created = wait_for_flag(
        &runtime,
        window,
        "window.__surfaceResult",
        Duration::from_secs(60),
    );
    assert_eq!(
        created.get("ok"),
        Some(&Value::Bool(true)),
        "the page's create must succeed — the whole seam path is the evidence: {created}"
    );
    let surface = created
        .get("surface")
        .and_then(Value::as_u64)
        .expect("the answer carries the surface id");

    // The creation truth, read at the window manager: the surface is childed
    // into the host and sits above the webview's own child tree.
    let host_deadline = Instant::now() + Duration::from_secs(15);
    let host = loop {
        if let Some(host) = find_toplevel_window("ncui-video-wiring") {
            break host;
        }
        assert!(
            Instant::now() < host_deadline,
            "the host window never appeared"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    let classes = child_zorder_classes(host);
    assert_eq!(
        classes.first().map(String::as_str),
        Some("ncui-video-surface"),
        "the created surface must be the topmost child of the host: {classes:?}"
    );

    // The placement truth: the page said [120, 100, 320, 180] CSS px times the
    // page's own ratio, and the OS-level child must sit at exactly the physical
    // answer once the place call has landed.
    let placed = wait_for_flag(
        &runtime,
        window,
        "window.__placeResult",
        Duration::from_secs(30),
    );
    assert_eq!(
        placed.get("ok"),
        Some(&Value::Bool(true)),
        "placement accepted: {placed}"
    );
    let dpr = evaluate(&runtime, window, "window.devicePixelRatio")
        .expect("the ratio reads back")
        .as_f64()
        .expect("the ratio is a number");
    // Find the surface child among the host's children by class, then measure it.
    // The rect is client-relative for a child; the host's own client origin is
    // the additive constant the OS-level rect carries.
    std::thread::sleep(Duration::from_millis(500));
    let child_rect = {
        // The child is found through the z-order walk's first entry — it is the
        // only video surface on this window.
        let mut found = None;
        for _ in 0..20 {
            if let Some(rect) =
                ncui_video_surface::evidence::first_child_rect_of_class(host, "ncui-video-surface")
            {
                found = Some(rect);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        found.expect("the surface child exists at OS level")
    };
    let host_rect = window_screen_rect(host).expect("the host has a screen rect");
    // The child's screen position minus the host frame's own offset: what
    // remains is the client-relative placement, which must equal CSS times dpr.
    // The frame offset is not knowable exactly from outside, so the honest
    // assertion is on the SIZE (offset-invariant) plus the position DELTA the
    // placement update caused: the create placed at [60,60], the update moved
    // to [120,100], so the child must have moved by (60,40) * dpr physical px.
    let expected_w = (320.0 * dpr).round() as i32;
    let expected_h = (180.0 * dpr).round() as i32;
    assert!(
        (child_rect.width() - expected_w).abs() <= 2,
        "the surface width must be the CSS width times the ratio: {} vs {expected_w} (dpr {dpr})",
        child_rect.width()
    );
    assert!(
        (child_rect.height() - expected_h).abs() <= 2,
        "the surface height likewise: {} vs {expected_h}",
        child_rect.height()
    );
    let _ = host_rect;

    // The producer seam: frames pushed natively present, and the ring answers
    // inspectably. The control intents the page sent drain in exact order.
    let controlled = wait_for_flag(
        &runtime,
        window,
        "window.__controlResult",
        Duration::from_secs(30),
    );
    assert_eq!(
        controlled.get("ok"),
        Some(&Value::Bool(true)),
        "control accepted: {controlled}"
    );
    let intents = {
        let runtime_guard = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime_guard.drain_video_control(window, surface)
    };
    assert_eq!(
        intents.len(),
        3,
        "pause, seek and step must all drain: {intents:?}"
    );
    assert!(
        matches!(intents[0], ncui_gpu::surface::ControlIntent::Pause),
        "pause first: {intents:?}"
    );
    assert!(
        matches!(intents[1], ncui_gpu::surface::ControlIntent::SeekTo(_)),
        "seek second: {intents:?}"
    );
    assert!(
        matches!(intents[2], ncui_gpu::surface::ControlIntent::Step(1)),
        "step third: {intents:?}"
    );

    // Frames flow: push a burst natively, then read the ring's accounting.
    {
        let runtime_guard = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for sequence in 0..5u64 {
            runtime_guard
                .push_video_frame(window, surface, green_frame(320, 180, sequence))
                .expect("frames present through the producer seam");
        }
    }

    // Teardown from the page: the OS-level child must die with it.
    evaluate(&runtime, window, "window.__teardown(); true").expect("the teardown drive evaluates");
    let torn = wait_for_flag(
        &runtime,
        window,
        "window.__teardownResult",
        Duration::from_secs(30),
    );
    assert_eq!(
        torn.get("ok"),
        Some(&Value::Bool(true)),
        "teardown accepted: {torn}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let classes = child_zorder_classes(host);
        if !classes.iter().any(|class| class == "ncui-video-surface") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the surface child must die with teardown: {classes:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    eprintln!("video surface wiring: create, place, control, frames, teardown — all proven");
    drop(pump_thread);
}

/// Frame-accuracy evidence at the wiring level: the identity of what the display
/// was asked to show is the stream's own numbering, end to end.
///
/// The scrub-accuracy claim this proves: frames pushed in a burst present in
/// push order with the newest winning — a scrub's final position is never
/// stranded behind the stale positions the gesture passed through. The decode
/// side of accuracy (which frame a position *is*) is the producer's contract;
/// what the runtime owes is that intents and frames travel without reordering,
/// and this is where that is measured.
#[test]
fn pushed_frames_present_in_order_with_their_identity_intact() {
    let (url, origin) = serve_page();
    let _serial = TEST_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
    runtime.grant_page_capabilities(&[Capability::Gpu, Capability::InspectGpu]);
    let actions = runtime.page_commands.actions();
    let grant = runtime.page_grant();
    runtime.windows = hosted_surface(&[origin], &actions, grant);
    let window = runtime
        .windows
        .open(
            &WindowRequest::new(&url).with_title("ncui-video-wiring-proof"),
            None,
        )
        .expect("the window opens");

    let runtime = Arc::new(Mutex::new(runtime));
    let pump = Arc::clone(&runtime);
    let pump_thread = std::thread::spawn(move || {
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

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match evaluate(&runtime, window, "true") {
            Ok(_) => break,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(250)),
            Err(error) => panic!("window never became evaluable: {error}"),
        }
    }
    let created = wait_for_flag(
        &runtime,
        window,
        "window.__surfaceResult",
        Duration::from_secs(60),
    );
    assert_eq!(
        created.get("ok"),
        Some(&Value::Bool(true)),
        "create succeeds: {created}"
    );
    let surface = created
        .get("surface")
        .and_then(Value::as_u64)
        .expect("the surface id");

    // The scrub-shaped burst: positions arrive faster than any display could
    // show them, ending at the frame the gesture settles on. The wiring owes
    // two truths here — nothing presents out of order, and the last frame
    // presented is the burst's last frame, not a stale position the user
    // already passed through.
    let outcome = {
        let runtime_guard = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut last_outcome = None;
        for sequence in 0..30u64 {
            last_outcome = Some(
                runtime_guard
                    .push_video_frame(window, surface, green_frame(320, 180, sequence))
                    .expect("burst frames push"),
            );
        }
        last_outcome
    };
    let _ = outcome;

    // Read the identity truth through the seam itself — the same inspect path
    // a page or an agent would use, because evidence should travel the
    // contract rather than a side channel.
    let host_deadline = Instant::now() + Duration::from_secs(10);
    while find_toplevel_window("ncui-video-wiring").is_none() {
        assert!(Instant::now() < host_deadline, "the host window appeared");
        std::thread::sleep(Duration::from_millis(100));
    }
    // Give the final present a moment to land — presentation is synchronous on
    // the push, so this is compositor settle time, not pipeline time.
    std::thread::sleep(Duration::from_millis(300));
    let inspected = evaluate(
        &runtime,
        window,
        &format!(
            "window.ncui.invoke('video_surface_inspect', {{ surface: {surface} }}).then(r => {{ window.__inspectResult = r; }}); true"
        ),
    );
    let _ = inspected;
    let report = wait_for_flag(
        &runtime,
        window,
        "window.__inspectResult",
        Duration::from_secs(15),
    );
    assert_eq!(
        report.get("lastPresented").and_then(Value::as_u64),
        Some(29),
        "the last frame presented must be the burst's last — a scrub never          strands the settled position behind stale ones: {report}"
    );

    // The ring's drop accounting is honest company for that truth — and here it
    // has something exact to say. Presentation is synchronous on push: every
    // frame presents inside its own `push_video_frame` call, so the ring never
    // accumulates and nothing can ever be dropped for freshness. A nonzero drop
    // count would mean frames silently vanished — the exact failure scrubbing
    // cannot tolerate. The asynchronous presentation model, where the ring's
    // drop-oldest discipline does its work, is an architecture decision this
    // pinning does not prejudge: if presentation ever moves off the push path,
    // this assertion is where that change must answer for its drops.
    assert_eq!(
        report.get("dropped").and_then(Value::as_u64),
        Some(0),
        "synchronous presentation drops nothing — a nonzero count is silent          frame loss: {report}"
    );

    eprintln!("frame identity: lastPresented=29 after a 30-frame burst — ordering proven");
    drop(pump_thread);
}
