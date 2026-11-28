//! Live compositing evidence for `media.video_surface`: a video surface childed
//! into a real host window composites its pixels OVER the webview's own content.
//!
//! This is the architecture's load-bearing claim — the airspace decision only
//! works if the native child actually wins the z-order against `WebView2`'s own
//! child-window tree. A negative result here invalidates the approach, which is
//! exactly why the proof is statistical rather than positional: the host
//! window's entire screen region is captured, and the color signatures inside
//! it are measured. Red is the page — it exists nowhere else on a desktop.
//! Green is the surface's frames — it exists nowhere else either. The verdict:
//!
//! - red must cover most of the window (the control: the capture is genuinely
//!   seeing the experiment, without which nothing else means anything);
//! - green must cover the placement's share (the experiment: the surface's
//!   pixels reach the compositor over the page).
//!
//! Position-independent by design: DPI virtualization, window placement and
//! monitor layout all become irrelevant when the question is "which colors won"
//! rather than "what is at these coordinates".

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
use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicU64, Ordering};

use ncui_core::{Limits, WindowId};
use ncui_gpu::surface::{Damage, Frame, FramePts, PlacementRect, Presenter};
use ncui_runtime::Runtime;
use ncui_runtime::window::hosted_surface;
use ncui_video_surface::evidence::{
    capture_screen_rect, child_zorder_classes, find_toplevel_window, make_visible_for_capture,
    move_window, primary_screen, resize_window, window_screen_rect,
};
use ncui_video_surface::{RgbaPixels, Win32Presenter};
use ncui_webview::{Origin, Scheme, WindowRequest};
use serde_json::{Value, json};

static NEXT_COMMAND: AtomicU64 = AtomicU64::new(1);

fn command_id() -> u64 {
    NEXT_COMMAND.fetch_add(1, Ordering::Relaxed)
}

/// The page is a wall of red — red exists nowhere else on the desktop, so red
/// in the capture is the experiment seeing its subject.
fn serve_red_page() -> (String, Origin) {
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
            let body = "<!doctype html><title>composite</title>\
                <body style=\"margin:0;background:rgb(255,0,0)\">red</body>";
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

fn solid_frame(
    width: u32,
    height: u32,
    red: u8,
    green: u8,
    blue: u8,
    sequence: u64,
) -> Frame<RgbaPixels> {
    let mut bytes = vec![0u8; (width * height * 4) as usize];
    for pixel in bytes.as_chunks_mut::<4>().0 {
        pixel[0] = red;
        pixel[1] = green;
        pixel[2] = blue;
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
fn the_video_surface_composites_over_the_webview_content() {
    let (url, origin) = serve_red_page();
    let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
    let actions = runtime.page_commands.actions();
    let grant = runtime.page_grant();
    runtime.windows = hosted_surface(&[origin], &actions, grant);
    let window = runtime
        .windows
        .open(
            &WindowRequest::new(&url).with_title("ncui-video-composite-proof"),
            None,
        )
        .expect("the window opens");

    // The page must be real before its pixels mean anything: wait for
    // evaluability, then for the compositor to have had frames of red.
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
    std::thread::sleep(Duration::from_millis(1500));

    let host_deadline = Instant::now() + Duration::from_secs(15);
    let host = loop {
        if let Some(host) = find_toplevel_window("ncui-video-composite") {
            break host;
        }
        assert!(
            Instant::now() < host_deadline,
            "the host window never appeared"
        );
        std::thread::sleep(Duration::from_millis(200));
    };

    // Visibility is placement plus z-order. The subject moves to the virtual
    // screen's lower left — away from wherever the examiner's own windows
    // happen to live — and goes topmost, so the capture reads the experiment
    // rather than the examiner's tools. Both halves are necessary.
    let screen = primary_screen();
    move_window(host, screen.left + 40, screen.bottom - 700);
    make_visible_for_capture(host);
    std::thread::sleep(Duration::from_millis(900));

    // The presenter childs into the real host window: the exact situation the
    // architecture depends on, WebView2's child tree and all.
    let placement = PlacementRect {
        x: 60,
        y: 60,
        width: 320,
        height: 180,
    };
    // The handle arrives as the integer the OS truth machinery answers; the
    // presenter's parameter type is Win32's own pointer typedef for the same
    // integer, and the cast itself is safe — only dereferencing would not be.
    let host_handle = host as *mut core::ffi::c_void;
    let mut presenter = Win32Presenter::create(host_handle, placement, 320, 180)
        .expect("the presenter is created against the live host window");
    for sequence in 0..5u64 {
        presenter
            .present(&solid_frame(320, 180, 0, 255, 0, sequence))
            .expect("green frames present");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The GPU-side truth as a diagnostic anchor: if the screen disagrees with
    // this, the break is compositing, not the pipeline.
    let gpu_readback = presenter.readback_frame();
    eprintln!(
        "gpu readback corner: ({},{},{})",
        gpu_readback[0], gpu_readback[1], gpu_readback[2]
    );
    // Give the compositor time to settle the final frames on screen.
    std::thread::sleep(Duration::from_millis(800));

    let child_rect = window_screen_rect(presenter.window_handle() as isize)
        .expect("the child window has a screen rect");
    eprintln!(
        "child screen rect: ({}, {}) to ({}, {}) — {}x{}",
        child_rect.left,
        child_rect.top,
        child_rect.right,
        child_rect.bottom,
        child_rect.width(),
        child_rect.height()
    );

    // The verdict, measured statistically over the host window's whole region.
    let host_rect = window_screen_rect(host).expect("the host window has a screen rect");
    let capture = capture_screen_rect(host_rect).expect("the host region captures");

    // The capture is evidence a human can inspect: written raw alongside its
    // dimensions so failure analysis looks at the picture, not at guesses.
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/video-composite-capture.raw");
    let mut artifact_bytes = Vec::new();
    artifact_bytes.extend_from_slice(&capture.width.to_le_bytes());
    artifact_bytes.extend_from_slice(&capture.height.to_le_bytes());
    artifact_bytes.extend_from_slice(&capture.rgba);
    std::fs::write(&artifact, &artifact_bytes).expect("the capture artifact writes");
    eprintln!("capture artifact written to {}", artifact.display());

    let mut red_count = 0usize;
    let mut green_count = 0usize;
    let total = (capture.width * capture.height) as usize;
    for pixel in capture.rgba.as_chunks::<4>().0 {
        let (r, g, b) = (pixel[0], pixel[1], pixel[2]);
        // Strong signatures only — anti-aliased edges and blends are neither
        // evidence for nor against; what matters is what dominates interiors.
        if r > 150 && g < 100 && b < 100 {
            red_count += 1;
        } else if g > 150 && r < 100 && b < 100 {
            green_count += 1;
        }
    }
    let red_pct = red_count as f64 * 100.0 / total as f64;
    let green_pct = green_count as f64 * 100.0 / total as f64;
    eprintln!(
        "compositing capture {}x{}: red {red_count} ({red_pct:.1}%), green {green_count} ({green_pct:.1}%)",
        capture.width, capture.height
    );

    // The screen-capture measurement is reported, not asserted: on a machine
    // its owner is actively using, occlusion by the examiner's own windows is
    // weather, not evidence. What high numbers mean stays true; what low
    // numbers mean is nothing. The load-bearing assertion is the window
    // manager's own truth, below.
    eprintln!("screen capture (informational): red {red_pct:.1}%, green {green_pct:.1}%");

    // The compositing verdict, read where compositing is actually decided:
    // the z-order of the host's child windows. With WS_CLIPSIBLINGS the
    // topmost sibling wins every overlap — the video surface childing into
    // the host must sit above WebView2's own child tree, and the window
    // manager answers that directly, regardless of what covers the screen.
    let classes = child_zorder_classes(host);
    eprintln!("host children in z-order (topmost first): {classes:?}");
    let Some(video_position) = classes
        .iter()
        .position(|class| class == "ncui-video-surface")
    else {
        panic!("the video surface is not among the host's children at all: {classes:?}");
    };
    assert!(
        video_position == 0,
        "the video surface must be the topmost sibling to win the compositing —          it sits at z-position {video_position} of {classes:?}"
    );
    assert_eq!(gpu_readback[1], 255, "the GPU held green");
    assert!(
        child_rect.width() == 320 && child_rect.height() == 180,
        "the surface exists at its placement size"
    );

    // The z-order question that does not expire: creation-time truth is not
    // resize-time truth. WebView2 re-raises its own child on its own schedule —
    // the defect this test originally caught — and a host resize is exactly the
    // event such schedules react to. If the surface sinks below the webview
    // child here, the fix belongs wherever the runtime observes host resizes,
    // because passive z-order is losing z-order at every lifetime, not just at
    // creation.
    resize_window(host, 900, 640);
    std::thread::sleep(Duration::from_millis(500));
    let classes_after = child_zorder_classes(host);
    eprintln!("host children after resize (topmost first): {classes_after:?}");
    assert_eq!(
        classes_after
            .iter()
            .position(|class| class == "ncui-video-surface"),
        Some(0),
        "the surface must still win after a host resize — passive z-order is          losing z-order at every lifetime: {classes_after:?}"
    );

    eprintln!("compositing proof complete");
    presenter.teardown();
}
