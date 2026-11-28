//! Audio stream command handlers.
//!
//! The dispatch half of the audio contract: the page declares streams and
//! playback intents, the registry owns the lifecycle, the producer owns time
//! and pushes samples natively. These methods are the page's half of that
//! bargain — create, control, teardown, inspect — and each is narrow on
//! purpose, mirroring the video surface seam because the problem is the same
//! problem with the geometry subtracted.

use ncui_core::Result;
use ncui_core::error::validation;
use ncui_gpu::surface::{ControlIntent, FramePts};
use serde_json::{Value, json};

use super::params::window_id_param;
use crate::Runtime;
use crate::audio::AudioStreamError;

/// Parses the stream id every method after create carries.
fn stream_id_param(params: &Value) -> Result<u64> {
    params.get("stream").and_then(Value::as_u64).ok_or_else(|| {
        validation("stream", "must be a stream id integer")
            .operation("dispatch::audio::stream_id_param")
    })
}

/// Maps the registry's typed refusal into the runtime error the wire expects.
#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err(refuse) passes the error by value — the function-pointer shape requires it"
)]
fn refuse(error: AudioStreamError) -> ncui_core::NcError {
    match error {
        AudioStreamError::NoSuchWindow(_) | AudioStreamError::NoSuchStream(_, _) => {
            validation("stream", error.to_string()).operation("dispatch::audio")
        }
        _ => validation("window", error.to_string()).operation("dispatch::audio"),
    }
}

/// `ncui.audioStream.create` — declare an audio output stream on this window.
///
/// The answer is the stream id the page names it by from then on. Creation is
/// where the device stream opens, so a stream that cannot play fails here, not
/// when the first chunk arrives — and the ring starts empty, which means the
/// first callbacks underrun and count their silence honestly until the
/// producer catches up.
#[cfg(target_os = "windows")]
pub(crate) fn invoke_audio_stream_create(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    const OPERATION: &str = "dispatch::audio_stream_create";
    let window = window_id_param(params)?;
    if runtime.windows.native_window_handle(window).is_none() {
        return Err(
            validation("window", format!("window {window} is not open")).operation(OPERATION)
        );
    }
    let stream = runtime
        .windows
        .audio_streams()
        .create(window)
        .map_err(refuse)?;
    Ok(json!({ "stream": stream }))
}

/// `ncui.audioStream.control` — a playback intent: pause, resume, seek.
///
/// The intent queues for the producer in arrival order, never coalesced and
/// never reordered; this method's answer is acceptance, not the effect. What
/// the intents mean in samples — when pause actually silences, how a seek
/// flushes — is the engine's business, because the engine owns time.
#[cfg(target_os = "windows")]
pub(crate) fn invoke_audio_stream_control(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    const OPERATION: &str = "dispatch::audio_stream_control";
    let window = window_id_param(params)?;
    let stream = stream_id_param(params)?;
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
        Some(other) => {
            return Err(validation(
                "intent",
                format!("unknown audio control intent '{other}' — pause, resume or seek"),
            )
            .operation(OPERATION));
        }
        None => {
            return Err(validation("intent", "must be pause, resume or seek").operation(OPERATION));
        }
    };
    runtime
        .windows
        .audio_streams()
        .control(window, stream, intent)
        .map_err(refuse)?;
    Ok(Value::Null)
}

/// `ncui.audioStream.teardown` — destroy a stream before its window dies.
///
/// The device stream stops and the ring's accounting closes. A page destroying
/// its own resource is not consent-worthy — the same shape as video surface
/// teardown.
#[cfg(target_os = "windows")]
pub(crate) fn invoke_audio_stream_teardown(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let window = window_id_param(params)?;
    let stream = stream_id_param(params)?;
    runtime
        .windows
        .audio_streams()
        .teardown(window, stream)
        .map_err(refuse)?;
    Ok(Value::Null)
}

/// `ncui.audioStream.inspect` — a stream's truth: the ring's loss accounting
/// and the sync reading the lip-sync measurement consumes.
///
/// This is the introspection half of the contract: positions, underruns and
/// drift are read where they live rather than inferred from what the page
/// hoped happened.
#[cfg(target_os = "windows")]
pub(crate) fn invoke_audio_stream_inspect(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    let window = window_id_param(params)?;
    let stream = stream_id_param(params)?;
    let (accounting, sync) = runtime
        .windows
        .audio_streams()
        .inspect(window, stream)
        .ok_or_else(|| {
            validation(
                "stream",
                format!("window {window} has no audio stream {stream}"),
            )
            .operation("dispatch::audio_stream_inspect")
        })?;
    let sync_json = sync.map_or(Value::Null, |reading| {
        json!({
            "deviceDelayMicros": reading.device_delay.as_micros(),
            "driftMicros": reading.clock_drift.as_micros(),
            "position": reading.position.map(|(value, timescale)| json!({
                "pts": value,
                "timescale": timescale,
            })),
            "frames": reading.frames,
        })
    });
    Ok(json!({
        "underruns": accounting.underruns,
        "overflowRefusals": accounting.overflow_refusals,
        "bufferedFrames": accounting.buffered_frames,
        "sync": sync_json,
    }))
}
