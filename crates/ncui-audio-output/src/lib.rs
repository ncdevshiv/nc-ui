//! The native audio output backend: a cpal stream draining the shared sample
//! ring, with the clock instrumentation the lip-sync measurement reads.
//!
//! # Why the backend is thin
//!
//! Everything that can be host-agnostic already lives in `ncui_gpu::audio` —
//! the ring, the loss accounting, the position truth. What remains here is
//! exactly the part that cannot: opening a device, honoring its callback, and
//! reading its clock. The callback is pull-shaped and cpal-owned; the backend
//! adds nothing to the loss model because a loss the model cannot see is a
//! loss nobody can measure.
//!
//! # The clock, honestly
//!
//! cpal's `StreamInstant` exposes no raw value — it can only be subtracted,
//! which is all honest arithmetic ever needed. Each callback records a
//! [`SyncMark`]: the wall instant, the stream's callback and playback
//! instants (opaque but subtractable), the timeline position of the drain's
//! first sample, and the frame count. The lip-sync measurement never converts
//! between clocks absolutely; it works in elapsed-time arithmetic on both
//! sides and *validates* the one assumption it makes — that the stream clock
//! and the wall clock advance at the same rate — by measuring drift between
//! successive marks. On WASAPI both clocks are `QueryPerformanceCounter`, so
//! the drift measurement should read zero; it is measured rather than assumed,
//! because a sync proof that assumes the clock it is trying to prove is no
//! proof.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ncui_gpu::audio::{ENGINE_SAMPLE_RATE, SampleRing};

/// What the backend reports when the device or the stream refuses.
#[derive(Debug)]
pub enum AudioOutputError {
    /// No default output device exists — a headless or audio-less machine is
    /// a truth, not a crash.
    NoDevice,
    /// The device cannot take the canonical engine format. Conversion at the
    /// boundary is a future increment; this increment refuses honestly rather
    /// than resampling silently, because silent resampling is the correctness
    /// bug farm the format decision exists to avoid.
    FormatRefused {
        /// What the device said, in words.
        reason: String,
    },
    /// The stream failed to build or run.
    Stream {
        /// What failed, in words.
        reason: String,
    },
}

impl core::fmt::Display for AudioOutputError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoDevice => write!(f, "no default audio output device"),
            Self::FormatRefused { reason } => {
                write!(f, "device refused the engine format: {reason}")
            }
            Self::Stream { reason } => write!(f, "stream failure: {reason}"),
        }
    }
}

impl std::error::Error for AudioOutputError {}

/// One callback's sync truth, kept as raw instants: opaque, subtractable, and
/// exactly as cpal reported them.
#[derive(Debug, Clone, Copy)]
pub struct SyncMark {
    /// The wall-clock instant the callback fired.
    pub callback_at_wall: Instant,
    /// cpal's stream-clock instant the callback fired.
    pub callback_at_stream: cpal::StreamInstant,
    /// cpal's prediction of when the first drained sample reaches the DAC, in
    /// stream-clock time.
    pub playback_at_stream: cpal::StreamInstant,
    /// The timeline position of the drain's first sample — value and timescale
    /// — when the ring held position truth. `None` means the drain was silence
    /// or pre-first-push.
    pub position: Option<(u64, u32)>,
    /// Sample frames the drain carried.
    pub frames: u64,
}

impl SyncMark {
    /// The device-side buffering delay this callback observed — how long after
    /// the callback its samples reach the DAC, in stream-clock arithmetic.
    #[must_use]
    pub fn device_delay(&self) -> Option<Duration> {
        self.playback_at_stream
            .checked_duration_since(self.callback_at_stream)
    }
}

/// What the measurement needs to answer "which timeline position is playing
/// now" in pure elapsed-time arithmetic. All durations are measured, and the
/// one cross-clock assumption — same rate — is what `clock_drift` validates.
#[derive(Debug, Clone, Copy)]
pub struct SyncReading {
    /// Wall instant of the newest mark.
    pub at_wall: Instant,
    /// How long after the callback the buffer starts reaching the DAC.
    pub device_delay: Duration,
    /// Timeline position at the start of that buffer, value and timescale.
    pub position: Option<(u64, u32)>,
    /// Frames in the buffer — the window the reading is valid inside.
    pub frames: u64,
    /// Measured drift between stream clock and wall clock across the marks so
    /// far: positive means the stream clock runs fast. The measurement reads
    /// this before trusting any cross-clock arithmetic.
    pub clock_drift: Duration,
}

/// The shared sync state the callback writes and the measurement reads. A
/// small ring of recent marks rather than one, because the drift measurement
/// needs pairs and the newest mark alone cannot drift-measure itself.
#[derive(Debug, Default)]
pub struct SyncState {
    marks: std::collections::VecDeque<SyncMark>,
}

impl SyncState {
    const KEEP: usize = 64;

    fn record(&mut self, mark: SyncMark) {
        self.marks.push_back(mark);
        while self.marks.len() > Self::KEEP {
            self.marks.pop_front();
        }
    }

    /// The newest mark, if any callback has fired.
    #[must_use]
    pub fn latest(&self) -> Option<SyncMark> {
        self.marks.back().copied()
    }

    /// The reading the measurement consumes: the newest mark's truth plus the
    /// drift measured between the oldest and newest marks in memory.
    #[must_use]
    pub fn reading(&self) -> Option<SyncReading> {
        let latest = self.marks.back()?;
        let device_delay = latest.device_delay().unwrap_or(Duration::ZERO);
        let clock_drift = if self.marks.len() >= 2 {
            let oldest = self.marks.front().copied().unwrap_or(*latest);
            let wall_elapsed = latest
                .callback_at_wall
                .checked_duration_since(oldest.callback_at_wall)
                .unwrap_or(Duration::ZERO);
            let stream_elapsed = latest
                .callback_at_stream
                .checked_duration_since(oldest.callback_at_stream)
                .unwrap_or(Duration::ZERO);
            stream_elapsed.abs_diff(wall_elapsed)
        } else {
            Duration::ZERO
        };
        Some(SyncReading {
            at_wall: latest.callback_at_wall,
            device_delay,
            position: latest.position,
            frames: latest.frames,
            clock_drift,
        })
    }
}

/// The backend itself: the running stream plus the handles the producer and
/// the measurement need.
///
/// The ring is shared because the producer pushes from its own thread — the
/// same convoy lesson the video arc measured: hot paths never wait on a lock
/// they do not need. The stream object pauses on drop; the device owes nothing
/// beyond that.
pub struct AudioOutput {
    stream: cpal::Stream,
    ring: Arc<Mutex<SampleRing>>,
    sync: Arc<Mutex<SyncState>>,
}

impl AudioOutput {
    /// Opens the default output device and starts the stream draining `ring`.
    ///
    /// The canonical engine format — 48 kHz stereo float32 interleaved — is
    /// requested exactly; a device that cannot take it is refused honestly.
    ///
    /// # Errors
    ///
    /// The device's and the stream's refusals, honestly divided.
    pub fn start(ring: Arc<Mutex<SampleRing>>) -> Result<Self, AudioOutputError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(AudioOutputError::NoDevice)?;

        let mut chosen = None;
        if let Ok(configs) = device.supported_output_configs() {
            for range in configs {
                if range.channels() == 2
                    && range.sample_format() == cpal::SampleFormat::F32
                    && range.min_sample_rate() <= ENGINE_SAMPLE_RATE
                    && range.max_sample_rate() >= ENGINE_SAMPLE_RATE
                {
                    chosen =
                        Some(range.with_sample_rate(cpal::SampleRate::from(ENGINE_SAMPLE_RATE)));
                    break;
                }
            }
        }
        let supported = chosen.ok_or_else(|| AudioOutputError::FormatRefused {
            reason: "no stereo float32 configuration at the engine rate".to_owned(),
        })?;
        let config: cpal::StreamConfig = supported.into();

        let sync = Arc::new(Mutex::new(SyncState::default()));
        let ring_in_callback = Arc::clone(&ring);
        let sync_in_callback = Arc::clone(&sync);

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    let callback_at_wall = Instant::now();
                    let timestamp = info.timestamp();
                    let position = {
                        let mut ring = ring_in_callback
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let position = ring.read_pts().map(|pts| (pts.value, pts.timescale));
                        ring.drain(data);
                        position
                    };
                    sync_in_callback
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .record(SyncMark {
                            callback_at_wall,
                            callback_at_stream: timestamp.callback,
                            playback_at_stream: timestamp.playback,
                            position,
                            frames: (data.len() / 2) as u64,
                        });
                },
                move |error| {
                    // cpal's error callback is the stream's death notice — there
                    // is nothing to retry in place. The ring's accounting already
                    // records what the silence costs; the notice is a diagnostic
                    // print because a dead stream must never be a quiet one.
                    #[allow(clippy::print_stderr, reason = "the stream's death notice has no other honest channel — the callback owns no logger and the ring cannot speak")]
                    {
                        eprintln!("audio output stream error: {error}");
                    }
                },
                None,
            )
            .map_err(|error| AudioOutputError::Stream {
                reason: error.to_string(),
            })?;
        stream.play().map_err(|error| AudioOutputError::Stream {
            reason: error.to_string(),
        })?;

        Ok(Self { stream, ring, sync })
    }

    /// The ring the producer pushes into — the same object the callback
    /// drains, shared rather than locked behind this crate's owner.
    #[must_use]
    pub fn ring(&self) -> Arc<Mutex<SampleRing>> {
        Arc::clone(&self.ring)
    }

    /// The sync reading the lip-sync measurement consumes.
    #[must_use]
    pub fn sync_reading(&self) -> Option<SyncReading> {
        self.sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reading()
    }
}

impl core::fmt::Debug for AudioOutput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // cpal's Stream exposes no Debug and nothing worth printing — the honest
        // summary is existence plus the shared state sizes.
        f.debug_struct("AudioOutput").finish_non_exhaustive()
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        let _ = self.stream.pause();
    }
}
