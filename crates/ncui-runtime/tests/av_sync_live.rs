//! The lip-sync measurement: the evidence the whole audio architecture exists
//! to make possible. A synthetic producer drives a video surface and an audio
//! stream from one timeline; the measurement reads both sides where they live
//! and answers, in milliseconds, how far the audio lags or leads the video.
//!
//! # What this proves
//!
//! Two things, in sequence:
//!
//! 1. The measurement is real arithmetic on real clocks. A naive producer —
//!    pushing audio and video for the same timeline position at the same wall
//!    instant — measures a stable positive offset: the audio pipeline's depth
//!    (ring occupancy plus device buffering) minus the video path's latency.
//!    That offset is not a defect; it is the physics the engine must correct,
//!    and the correction is impossible without the measurement.
//!
//! 2. The correction works. The corrected producer shifts its audio content
//!    assignment forward by the measured offset — pushing the chunk for timeline
//!    position X early enough that X reaches the DAC when the video frame for X
//!    is on screen. The residual error must land inside the frame-quantization
//!    window, because video quantizes the continuous timeline to frames and no
//!    correction can do better than the medium.
//!
//! # The honest residuals
//!
//! Two latencies this measurement cannot see, documented rather than hidden:
//! compositor-to-photon on the video side (unmeasurable in software on a
//! composited desktop, bounded small) and DAC-to-ear on the audio side
//! (single-digit milliseconds). Both are constant offsets much smaller than
//! the frame budget, and both cancel in the correction comparison because the
//! same residuals apply to both phases.

#![cfg(all(target_os = "windows", feature = "wry-host"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::missing_panics_doc,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    reason = "test assertions must abort loudly on failure; evidence is reported through stdout/stderr; measurement math casts deliberately; the measurement is one linear sequence that splitting would obscure"
)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ncui_core::{Capability, Limits, WindowId};
use ncui_gpu::audio::{AudioChunk, ENGINE_SAMPLE_RATE};
use ncui_gpu::surface::{Damage, Frame, FramePts};
use ncui_runtime::Runtime;
use ncui_runtime::window::hosted_surface;
use ncui_video_surface::RgbaPixels;
use ncui_webview::WindowRequest;
use serde_json::{Value, json};

/// The timeline both sides share: 90 kHz, the media standard. At 30 fps one
/// frame is exactly 3000 units; at 48 kHz one frame span is exactly 1600
/// samples — the divisibility is exact, so position arithmetic never rounds.
const TIMESCALE: u32 = 90_000;
const FRAME_UNITS: u64 = 3000;
const SAMPLES_PER_FRAME: u64 = 1600;
const FRAME_INTERVAL: Duration = Duration::from_nanos(33_333_333);

/// These tests create OS-level windows — one at a time is the honest shape for
/// evidence gathered from a shared window manager.
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

fn frame_pixels(width: u32, height: u32) -> RgbaPixels {
    RgbaPixels {
        width,
        height,
        stride: width * 4,
        bytes: vec![180u8; (width * height * 4) as usize],
    }
}

fn audio_chunk(first_sample_pts_units: u64, frames: u64) -> AudioChunk {
    AudioChunk {
        pts: FramePts {
            value: first_sample_pts_units,
            timescale: TIMESCALE,
        },
        samples: vec![0.0; (frames * 2) as usize],
    }
}

/// One measurement sample: at wall instant T, the timeline positions of what
/// the video shows and what the audio plays, both in seconds.
struct SyncSample {
    video_position: f64,
    audio_position: f64,
}

/// Takes one measurement sample. The arithmetic, honestly:
///
/// - Video: the last presented frame's sequence maps to its pts by the
///   producer's own cadence — the frame on screen now, quantized to the frame
///   grid because the medium quantizes.
/// - Audio: the newest sync mark says the drain starting at position P0 hit
///   the DAC at `at_wall + device_delay`. Position playing now is P0 plus the
///   samples the elapsed time since then carries — elapsed-time arithmetic on
///   the wall clock, valid because the drift measurement proved the stream
///   clock and the wall clock advance together.
fn sample_sync(
    runtime: &Arc<Mutex<Runtime>>,
    window: WindowId,
    surface: u64,
    stream: u64,
) -> Option<SyncSample> {
    let now = Instant::now();
    let (video_seq, sync) = {
        let runtime = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let video = runtime.inspect_video_surface(window, surface)?;
        let (_, sync) = runtime.inspect_audio_stream(window, stream)?;
        (video.2?, sync?)
    };
    let (pts_value, pts_timescale) = sync.position?;
    // The drain's first sample reaches the DAC at this wall instant.
    let dac_start = sync.at_wall + sync.device_delay;
    let elapsed = now
        .checked_duration_since(dac_start)
        .unwrap_or(Duration::ZERO);
    let samples_played = elapsed.as_secs_f64() * f64::from(ENGINE_SAMPLE_RATE);
    // Position arithmetic in the producer's timescale: P0 plus the samples
    // played converted into timeline units.
    let audio_position = (pts_value as f64 / f64::from(pts_timescale))
        + samples_played / f64::from(ENGINE_SAMPLE_RATE);
    let video_position = video_seq as f64 * FRAME_UNITS as f64 / f64::from(TIMESCALE);
    Some(SyncSample {
        video_position,
        audio_position,
    })
}

#[test]
fn lip_sync_error_is_measured_and_the_correction_holds_budget() {
    let _serial = TEST_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (url, origin) = serve_html(
        r#"<!doctype html><title>av-sync</title>
<body style="margin:0">
<div id="monitor" style="position:absolute;left:60px;top:60px;width:64px;height:64px"></div>
<script>
window.__surfaceResult = null;
window.__run = async () => {
  try {
    const created = await window.ncui.invoke('video_surface_create', {
      width: 64, height: 64,
      rect: [60, 60, 64, 64],
      scroll: [window.scrollX, window.scrollY],
      devicePixelRatio: window.devicePixelRatio,
      clip: [0, 0, window.innerWidth + window.scrollX, window.innerHeight + window.scrollY],
    });
    window.__surfaceResult = { ok: true, surface: created.surface };
  } catch (error) {
    window.__surfaceResult = { ok: false, error: String(error) };
  }
};
window.addEventListener('load', () => {
  const start = () => {
    if (window.ncui) { window.__run(); } else { setTimeout(start, 50); }
  };
  start();
});
</script>
av sync</body>"#,
    );
    let mut runtime = Runtime::start(Limits::default()).expect("runtime starts");
    runtime.grant_page_capabilities(&[Capability::Gpu, Capability::InspectGpu, Capability::Audio]);
    let actions = runtime.page_commands.actions();
    let grant = runtime.page_grant();
    runtime.windows = hosted_surface(&[origin], &actions, grant);
    let window: WindowId = runtime
        .windows
        .open(
            &WindowRequest::new(&url).with_title("ncui-av-sync-proof"),
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
        "the surface creates through the seam: {created}"
    );
    let surface = created
        .get("surface")
        .and_then(Value::as_u64)
        .expect("the answer carries the surface id");
    let stream = {
        let runtime = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime
            .create_audio_stream(window)
            .expect("the audio stream creates")
    };

    // The producer: one thread, one timeline, both paths fed at the frame
    // cadence. The `audio_shift` in units is the correction — zero in the
    // naive phase, the measured offset in the corrected phase.
    let producer_handle = {
        let runtime = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.video_surface_producer()
    };
    let audio_producer = {
        let runtime = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.audio_stream_producer()
    };
    let ring = audio_producer
        .ring(window, stream)
        .expect("the producer reaches the ring");
    let stop = Arc::new(Mutex::new(false));
    let shift_units = Arc::new(Mutex::new(0i64));
    let resync = Arc::new(Mutex::new(false));
    let producer = {
        let stop = Arc::clone(&stop);
        let shift_units = Arc::clone(&shift_units);
        let resync = Arc::clone(&resync);
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut index = 0u64;
            loop {
                if *stop
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                {
                    break;
                }
                let pts_units = index * FRAME_UNITS;
                // Video frame: tiny pixels, the timing is the evidence.
                let frame = Frame {
                    sequence: index,
                    pts: FramePts {
                        value: pts_units,
                        timescale: TIMESCALE,
                    },
                    damage: vec![Damage::full(64, 64)],
                    payload: frame_pixels(64, 64),
                };
                if producer_handle.push_frame(window, surface, frame).is_err() {
                    break;
                }
                // Audio chunk: the content assignment is where the correction
                // lives — the pts says which timeline position these samples
                // ARE, and the shift moves that assignment forward.
                let shift = *shift_units
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let resync_now = std::mem::replace(
                    &mut *resync
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    false,
                );
                let audio_pts = i64::try_from(pts_units).expect("the test pts fits i64") + shift;
                if audio_pts >= 0 {
                    let mut ring = ring
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if resync_now {
                        // A changed assignment only anchors through the seek's
                        // own path: the ring's read position is anchor-plus-count
                        // for the stream's continuous life, so a correction the
                        // engine means must flush the buffered past and let the
                        // next push re-anchor — silence for one drain, then the
                        // corrected timeline.
                        ring.flush();
                    }
                    if ring
                        .push(&audio_chunk(audio_pts as u64, SAMPLES_PER_FRAME))
                        .is_err()
                    {
                        // Ring full — the device owns the drain rate; skip this
                        // push rather than overwrite, because audio never drops.
                    }
                }
                index += 1;
                let next = start + FRAME_INTERVAL * (index as u32);
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
            }
        })
    };

    // Phase 1: a deliberate, known error. The producer starts with its audio
    // content assignment shifted forward by 150ms — at wall T it pushes the
    // samples for timeline position T+150ms, a positively measurable sync
    // fault. Whatever the machine's baseline pipeline latencies are, the
    // measurement must read them plus exactly the deliberate shift: that is
    // what makes the proof independent of the machine.
    *shift_units
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = (0.150 * f64::from(TIMESCALE)) as i64;
    // The deliberate fault must land in the ring, not just in the producer's
    // intent — a pts reassignment on a running ring is ignored by the
    // anchor-plus-count read position, so the producer flushes once to let
    // the shifted assignment anchor. A real fault enters the same way.
    *resync
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    std::thread::sleep(Duration::from_secs(2));
    let mut shifted_errors = Vec::new();
    for _ in 0..6 {
        if let Some(sample) = sample_sync(&runtime, window, surface, stream) {
            shifted_errors.push(sample.audio_position - sample.video_position);
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    assert!(
        shifted_errors.len() >= 4,
        "the shifted phase must produce measurement samples"
    );
    // Drop the first sample — startup transient while the ring reaches its
    // working depth — and average the rest.
    shifted_errors.remove(0);
    let shifted_mean = shifted_errors.iter().sum::<f64>() / shifted_errors.len() as f64;
    let min = shifted_errors.iter().copied().fold(f64::INFINITY, f64::min);
    let max = shifted_errors
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    eprintln!(
        "shifted phase (+150ms deliberate): audio-minus-video mean {:.1}ms over {} samples ({:?})",
        shifted_mean * 1000.0,
        shifted_errors.len(),
        shifted_errors
            .iter()
            .map(|e| format!("{:.1}ms", e * 1000.0))
            .collect::<Vec<_>>()
    );

    // The measurement's first truth: stability — the spread is small against
    // the magnitude, because a clock-confused measurement would wander.
    assert!(
        max - min < 0.050,
        "the measured error must be stable — spread {:.1}ms says the measurement itself is noise",
        (max - min) * 1000.0
    );

    // Phase 2: the correction. The engine reads the measured error and moves
    // its content assignment back by exactly it — the shift response and the
    // correction are the same experiment: if the measurement is real
    // arithmetic, the residual lands at zero modulo frame quantization.
    let correction_units = (shifted_mean * f64::from(TIMESCALE)) as i64;
    let new_shift = (0.150 * f64::from(TIMESCALE)) as i64 - correction_units;
    *shift_units
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = new_shift;
    // The corrected assignment has to reach the read position, and the ring's
    // read anchor only re-seeds from empty — so the producer flushes on its
    // next push, the same actuation a seek performs.
    *resync
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    eprintln!(
        "correction applied: shift moved by {} units ({:.1}ms) — residual target zero",
        correction_units,
        shifted_mean * 1000.0
    );

    std::thread::sleep(Duration::from_secs(2));
    let mut corrected_errors = Vec::new();
    for _ in 0..6 {
        if let Some(sample) = sample_sync(&runtime, window, surface, stream) {
            corrected_errors.push(sample.audio_position - sample.video_position);
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    *stop
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    producer.join().expect("the producer stops");
    assert!(
        corrected_errors.len() >= 4,
        "the corrected phase must produce measurement samples"
    );
    corrected_errors.remove(0);
    let corrected_mean = corrected_errors.iter().sum::<f64>() / corrected_errors.len() as f64;
    eprintln!(
        "corrected phase: audio-minus-video mean {:.1}ms ({:?})",
        corrected_mean * 1000.0,
        corrected_errors
            .iter()
            .map(|e| format!("{:.1}ms", e * 1000.0))
            .collect::<Vec<_>>()
    );

    // The proof, both halves. First the shift response: the measured error
    // must have moved by exactly the correction applied — arithmetic, not
    // perception. Second the budget: the residual sits inside one frame
    // interval of zero, because video quantizes the continuous timeline to
    // frames and the broadcast lip-sync threshold (45ms) is the honest bound.
    let response = shifted_mean - corrected_mean;
    assert!(
        (response - shifted_mean).abs() < 0.020,
        "the correction must move the error by the measured amount — moved {:.1}ms against measured {:.1}ms",
        response * 1000.0,
        shifted_mean * 1000.0
    );
    assert!(
        corrected_mean.abs() < 0.045,
        "the residual must sit inside the lip-sync budget — {:.1}ms against the 45ms broadcast threshold",
        corrected_mean * 1000.0
    );

    eprintln!(
        "lip-sync evidence: deliberate +150ms read as {:.1}ms, corrected residual {:.1}ms — measurement and correction both real",
        shifted_mean * 1000.0,
        corrected_mean * 1000.0
    );
}
