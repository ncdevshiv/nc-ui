//! Live evidence for the audio output backend against the machine's real
//! default device. The payload is silence throughout: the evidence is in
//! positions, drain behavior, and clock truth — none of which depend on what
//! the samples sound like, and the owner's machine owes no noise.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::panic,
    clippy::print_stderr,
    clippy::print_stdout,
    clippy::too_many_lines,
    clippy::unwrap_used,
    missing_docs,
    reason = "live evidence harness — diagnostics and honest skips are the point"
)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ncui_audio_output::AudioOutput;
use ncui_gpu::audio::{AudioChunk, ENGINE_SAMPLE_RATE, SampleRing};
use ncui_gpu::surface::FramePts;

fn silent_chunk(pts_value: u64, frames: u64) -> AudioChunk {
    AudioChunk {
        pts: FramePts {
            value: pts_value,
            timescale: ENGINE_SAMPLE_RATE,
        },
        samples: vec![0.0; (frames * 2) as usize],
    }
}

/// The end-to-end truth: a real stream drains a real ring, positions flow
/// through the callback into the sync marks, and the clock drift measurement
/// validates the cross-clock arithmetic the lip-sync proof will rely on.
#[test]
fn stream_drains_ring_with_position_truth() {
    let ring = Arc::new(Mutex::new(
        SampleRing::new(250).expect("ring capacity inside the maximum"),
    ));
    let output = match AudioOutput::start(Arc::clone(&ring)) {
        Ok(output) => output,
        Err(error) => {
            eprintln!("skipping — no usable output device: {error}");
            return;
        }
    };

    // First truth: an empty ring underruns — the device pulls, nothing is
    // there, and the accounting counts the silence honestly.
    std::thread::sleep(Duration::from_millis(200));
    let early = ring
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .accounting();
    eprintln!(
        "after 200ms starved: underruns={} buffered={}",
        early.underruns, early.buffered_frames
    );
    assert!(
        early.underruns > 0,
        "a starved ring must count its silence — got zero underruns after 200ms"
    );

    // Second truth: positions flow. Push a continuous stream at the natural
    // rate for two seconds — chunks of 480 frames (10ms) at pts advancing in
    // sample units, timescale = rate, so pts == sample index.
    let producer_ring = Arc::clone(&ring);
    let producer = std::thread::spawn(move || {
        let start = Instant::now();
        let mut next_pts = 0u64;
        let chunk_frames = 480u64; // 10ms at 48kHz
        while start.elapsed() < Duration::from_secs(2) {
            {
                let mut ring = producer_ring
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if ring.push(&silent_chunk(next_pts, chunk_frames)).is_err() {
                    // Full ring — the producer outran the device; wait a slice
                    // and retry the same chunk, because audio never drops.
                    drop(ring);
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
            }
            next_pts += chunk_frames;
            std::thread::sleep(Duration::from_millis(8));
        }
    });

    // Let the stream run and collect sync evidence mid-flight.
    std::thread::sleep(Duration::from_millis(1200));
    let reading = output
        .sync_reading()
        .expect("a running stream must produce sync marks");
    eprintln!(
        "mid-stream reading: position={:?} device_delay={:?} frames={} drift={:?}",
        reading.position, reading.device_delay, reading.frames, reading.clock_drift
    );

    // The position truth: the drain position must exist (the producer has
    // pushed by now) and must be sane — inside the first two seconds of the
    // timeline.
    let (value, timescale) = reading
        .position
        .expect("positions must flow once the producer is pushing");
    assert_eq!(timescale, ENGINE_SAMPLE_RATE);
    assert!(
        value < u64::from(ENGINE_SAMPLE_RATE) * 3,
        "position {value} ran ahead of the timeline — impossible for a pull-drained ring"
    );

    // The device delay is real and bounded: WASAPI shared-mode buffers are
    // single-digit to low-tens of milliseconds. A zero delay would mean the
    // timestamps are fabricated; a huge one means the clock is broken.
    assert!(
        reading.device_delay > Duration::ZERO,
        "device delay of zero means cpal is not reporting real timestamps"
    );
    assert!(
        reading.device_delay < Duration::from_millis(100),
        "device delay {:?} is beyond any plausible WASAPI buffer",
        reading.device_delay
    );

    producer.join().expect("producer thread completes");

    // Third truth: drift. After enough callbacks the drift measurement across
    // the kept window must be tiny — milliseconds over the run, not percent.
    std::thread::sleep(Duration::from_millis(400));
    let late = output
        .sync_reading()
        .expect("sync marks continue while the stream runs");
    eprintln!("late drift measurement: {:?}", late.clock_drift);
    assert!(
        late.clock_drift < Duration::from_millis(50),
        "clock drift {:?} over the kept window refutes the same-rate assumption",
        late.clock_drift
    );

    // Fourth truth: overrun discipline. With the producer gone the ring
    // empties; the accounting's underrun count keeps climbing — silence is
    // counted, never free.
    let final_accounting = output
        .ring()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .accounting();
    eprintln!(
        "final accounting: underruns={} overflow_refusals={} buffered={}",
        final_accounting.underruns,
        final_accounting.overflow_refusals,
        final_accounting.buffered_frames
    );
    assert!(
        final_accounting.underruns > early.underruns,
        "underruns must keep counting after the producer stops"
    );

    eprintln!("audio output evidence: all truths green");
}
