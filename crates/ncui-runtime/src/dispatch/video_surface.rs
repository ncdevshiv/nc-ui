//! Video surface command handlers.
//!
//! The dispatch half of the surface contract: the page owns geometry and reports
//! it in document-logical terms, the registry converts and places, the producer
//! owns time and pushes frames natively. These methods are the page's half of
//! that bargain — create, place, control, teardown — and each is narrow on
//! purpose: a surface method that did more than its one job would be a second
//! placement path, and placement having exactly one home is what keeps the
//! native pixels over the page's intent.

use ncui_core::Result;
use ncui_core::error::validation;
use ncui_gpu::surface::{ControlIntent, FramePts, SurfaceSpec};
use serde_json::{Value, json};

use super::params::window_id_param;
use crate::Runtime;
use crate::video_surface::{PagePlacement, VideoSurfaceError};

/// Parses the surface id every method after create carries.
fn surface_id_param(params: &Value) -> Result<u64> {
    params
        .get("surface")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            validation("surface", "must be a surface id integer")
                .operation("dispatch::video_surface::surface_id_param")
        })
}

/// Parses the page-reported placement: document rect, scroll, ratio, clip.
///
/// Every field is mandatory rather than defaulted, because a default here is a
/// guess about geometry — and a guessed geometry is exactly the positioning bug
/// this contract exists to make impossible. The page has all four numbers; it
/// says all four.
fn placement_param(params: &Value) -> Result<PagePlacement> {
    const OPERATION: &str = "dispatch::video_surface::placement_param";
    let rect = params
        .get("rect")
        .and_then(Value::as_array)
        .filter(|a| a.len() == 4)
        .ok_or_else(|| {
            validation("rect", "must be [x, y, width, height] in CSS pixels").operation(OPERATION)
        })?;
    let scroll = params
        .get("scroll")
        .and_then(Value::as_array)
        .filter(|a| a.len() == 2)
        .ok_or_else(|| validation("scroll", "must be [x, y] in CSS pixels").operation(OPERATION))?;
    let clip = params
        .get("clip")
        .and_then(Value::as_array)
        .filter(|a| a.len() == 4)
        .ok_or_else(|| {
            validation("clip", "must be [x, y, width, height] in CSS pixels").operation(OPERATION)
        })?;
    let dpr = params
        .get("devicePixelRatio")
        .and_then(Value::as_f64)
        .ok_or_else(|| {
            validation("devicePixelRatio", "must be the window's ratio").operation(OPERATION)
        })?;
    let numbers = |array: &[Value]| -> Option<(f32, f32, f32, f32)> {
        let v: Vec<f32> = array
            .iter()
            .filter_map(Value::as_f64)
            .map(|v| v as f32)
            .collect();
        if v.len() == 4 {
            Some((v[0], v[1], v[2], v[3]))
        } else {
            None
        }
    };
    let rect4 = numbers(rect)
        .ok_or_else(|| validation("rect", "must be four numbers").operation(OPERATION))?;
    let clip4 = numbers(clip)
        .ok_or_else(|| validation("clip", "must be four numbers").operation(OPERATION))?;
    let sx = scroll[0]
        .as_f64()
        .ok_or_else(|| validation("scroll", "must be numbers").operation(OPERATION))?
        as f32;
    let sy = scroll[1]
        .as_f64()
        .ok_or_else(|| validation("scroll", "must be numbers").operation(OPERATION))?
        as f32;
    if !(dpr > 0.0 && dpr.is_finite()) {
        return Err(
            validation("devicePixelRatio", "must be positive and finite").operation(OPERATION),
        );
    }
    Ok(PagePlacement {
        rect: rect4,
        scroll: (sx, sy),
        device_pixel_ratio: dpr as f32,
        clip: clip4,
    })
}

/// Maps the registry's typed refusal into the runtime error the wire expects.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the error is consumed semantically — mapped and dropped — and the owned form is what the match arms want; a reference would be borrowing for its own sake"
)]
fn refuse(error: VideoSurfaceError) -> ncui_core::NcError {
    match error {
        VideoSurfaceError::NoSuchWindow(_) | VideoSurfaceError::NoSuchSurface(_, _) => {
            validation("surface", error.to_string()).operation("dispatch::video_surface")
        }
        _ => validation("window", error.to_string()).operation("dispatch::video_surface"),
    }
}

/// `ncui.videoSurface.create` — declare a surface and its first placement.
///
/// The answer is the surface id the page names it by from then on. Creation is
/// where the presenter is built — against the host window's real native handle —
/// so a surface that cannot present fails here, not when the first frame arrives.
#[cfg(target_os = "windows")]
pub(crate) fn invoke_video_surface_create(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    const OPERATION: &str = "dispatch::video_surface_create";
    let window = window_id_param(params)?;
    let width = params.get("width").and_then(Value::as_u64).ok_or_else(|| {
        validation("width", "must be the frame width in pixels").operation(OPERATION)
    })? as u32;
    let height = params
        .get("height")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            validation("height", "must be the frame height in pixels").operation(OPERATION)
        })? as u32;
    let placement = placement_param(params)?;
    let handle = runtime
        .windows
        .native_window_handle(window)
        .ok_or_else(|| {
            validation("window", format!("window {window} is not open")).operation(OPERATION)
        })?;
    let spec = SurfaceSpec { width, height };
    let surface = runtime
        .windows
        .video_surfaces()
        .create(window, handle, spec, placement)
        .map_err(refuse)?;
    Ok(json!({ "surface": surface }))
}

/// `ncui.videoSurface.place` — update where the surface sits over the page.
///
/// This is the scroll/resize/geometry-sync path, and it is cheap on purpose:
/// the conversion is arithmetic and the platform call is a positioning one, so
/// a page can honestly report at gesture rate rather than coalescing the truth
/// away.
#[cfg(target_os = "windows")]
pub(crate) fn invoke_video_surface_place(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let window = window_id_param(params)?;
    let surface = surface_id_param(params)?;
    let placement = placement_param(params)?;
    runtime
        .windows
        .video_surfaces()
        .place(window, surface, placement)
        .map_err(refuse)?;
    Ok(Value::Null)
}

/// `ncui.videoSurface.control` — a playback intent: pause, resume, seek, step.
///
/// The intent is recorded in the surface model and queued for the producer;
/// this method's answer is acceptance, not the effect — frame accuracy is the
/// producer's contract, measured against the model's playback state.
#[cfg(target_os = "windows")]
pub(crate) fn invoke_video_surface_control(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    const OPERATION: &str = "dispatch::video_surface_control";
    let window = window_id_param(params)?;
    let surface = surface_id_param(params)?;
    let intent = match params.get("intent").and_then(Value::as_str) {
        Some("pause") => ControlIntent::Pause,
        Some("resume") => ControlIntent::Resume,
        Some("seek") => {
            let value = params.get("pts").and_then(Value::as_u64).ok_or_else(|| {
                validation("pts", "a seek must name its position").operation(OPERATION)
            })?;
            let timescale = params
                .get("timescale")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    validation("timescale", "a seek must name its position's timescale")
                        .operation(OPERATION)
                })? as u32;
            ControlIntent::SeekTo(FramePts { value, timescale })
        }
        Some("step") => {
            let frames = params
                .get("frames")
                .and_then(Value::as_i64)
                .ok_or_else(|| {
                    validation("frames", "a step must name its signed frame count")
                        .operation(OPERATION)
                })?;
            ControlIntent::Step(frames as i32)
        }
        _ => {
            return Err(
                validation("intent", "must be one of pause, resume, seek, step")
                    .operation(OPERATION),
            );
        }
    };
    runtime
        .windows
        .video_surfaces()
        .control(window, surface, intent)
        .map_err(refuse)?;
    Ok(Value::Null)
}

/// `ncui.videoSurface.teardown` — destroy a surface before its window dies.
///
/// Windows tear their surfaces down on close regardless; this is the page's own
/// path for surfaces it is done with mid-session — a closed clip bin, a swapped
/// monitor — where waiting for the window would leak presenter memory for the
/// session's length.
#[cfg(target_os = "windows")]
pub(crate) fn invoke_video_surface_teardown(
    runtime: &mut Runtime,
    params: &Value,
) -> Result<Value> {
    let window = window_id_param(params)?;
    let surface = surface_id_param(params)?;
    runtime
        .windows
        .video_surfaces()
        .teardown(window, surface)
        .map_err(refuse)?;
    Ok(Value::Null)
}

/// `ncui.videoSurface.inspect` — the ring truth: drops and queue depth.
///
/// Read-only and cheap; the acceptance arc's drop accounting reads here, and a
/// page debugging stutter can see the ring's answer to its producer's rate
/// without guessing from the picture.
#[cfg(target_os = "windows")]
pub(crate) fn invoke_video_surface_inspect(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let window = window_id_param(params)?;
    let surface = surface_id_param(params)?;
    let (dropped, queued, last_presented) = runtime
        .windows
        .video_surfaces()
        .inspect(window, surface)
        .ok_or_else(|| {
            validation(
                "surface",
                format!("window {window} has no video surface {surface}"),
            )
            .operation("dispatch::video_surface_inspect")
        })?;
    Ok(json!({
        "dropped": dropped,
        "queued": queued,
        "lastPresented": last_presented,
    }))
}
