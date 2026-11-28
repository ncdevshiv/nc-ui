//! The runtime's audio stream registry: the wiring between the producer's
//! samples, the shared ring, and the platform device stream.
//!
//! The architecture mirrors the video surface registry with the geometry
//! subtracted: audio has no placement — a stream is a stream — but everything
//! else carries over. The producer owns time: samples arrive when the engine
//! mixed them, never on a clock the registry invented. The registry owns
//! everything in between: the ring's loss accounting, the stream lifecycle,
//! and the teardown that ties every stream to its window.
//!
//! # One device stream per audio stream
//!
//! Each audio stream opens its own cpal stream on the default device. WASAPI
//! shared mode mixes concurrent streams in the system audio engine, so the
//! mixing an application-level mixer would do is already done — by the
//! platform, sample-accurately, with no code here to get wrong. The cost is
//! one stream object per audio stream, which is small next to the correctness
//! bug farm an application mixer would be.
//!
//! What this module deliberately does not do is decode. Samples arrive as
//! [`AudioChunk`] — interleaved float32 stereo at the engine rate — because
//! the first honest transport is the one that works everywhere; compressed
//! payload paths are an optimization the seam already anticipates by carrying
//! positions rather than assuming continuity.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ncui_audio_output::{AudioOutput, SyncReading};
use ncui_core::WindowId;
use ncui_gpu::audio::{AudioChunk, RingAccounting, SampleRing};
use ncui_gpu::surface::ControlIntent;

/// One audio stream the runtime is wiring: the device stream, which owns the
/// ring and the sync truth, plus the control intents the producer has not yet
/// drained.
struct AudioStreamEntry {
    /// The live backend — dropping it stops playback.
    output: AudioOutput,
    /// Control intents not yet drained by the producer, in arrival order.
    pending_control: Vec<ControlIntent>,
}

impl core::fmt::Debug for AudioStreamEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AudioStreamEntry").finish_non_exhaustive()
    }
}

/// Why an audio stream operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioStreamError {
    /// The window named is not open.
    NoSuchWindow(WindowId),
    /// The stream named does not exist on the window.
    NoSuchStream(WindowId, u64),
    /// The device or the stream refused — no device, a format refusal, a
    /// stream failure.
    Device(String),
    /// The ring refused the push — full, or a position regression.
    Ring(String),
}

impl core::fmt::Display for AudioStreamError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSuchWindow(window) => write!(f, "window {window} is not open"),
            Self::NoSuchStream(window, stream) => {
                write!(f, "window {window} has no audio stream {stream}")
            }
            Self::Device(reason) => write!(f, "the audio device refused: {reason}"),
            Self::Ring(reason) => write!(f, "the sample ring refused: {reason}"),
        }
    }
}

/// The registry itself: streams keyed by window and id, with the id supply.
///
/// Behind one mutex for the registry structure; the rings inside synchronize
/// themselves — the producer's pushes lock only the one ring they push into,
/// never the registry, because the video arc measured what a shared lock does
/// to a hot media path and this design is the answer.
#[derive(Debug, Default)]
pub(crate) struct AudioStreams {
    entries: Mutex<HashMap<(WindowId, u64), AudioStreamEntry>>,
    next_id: AtomicU64,
}

/// The producer's own handle onto the registry — cloneable, shareable, and
/// independent of the runtime's lock.
///
/// Sample pushes are a hot path: forty-eight thousand samples a second in
/// chunks sized by the engine's mixing cadence. The producer handle carries
/// the ring directly — after stream creation the registry is out of the push
/// path entirely, because the only exclusion a push should ever wait on is
/// the ring's own.
#[derive(Debug)]
pub struct AudioStreamProducer {
    registry: Arc<AudioStreams>,
}

impl AudioStreamProducer {
    pub(crate) fn new(registry: Arc<AudioStreams>) -> Self {
        Self { registry }
    }

    /// The ring handle for one stream: the producer clones it once and pushes
    /// for the stream's lifetime without ever re-entering the registry.
    ///
    /// # Errors
    ///
    /// The registry's typed refusal when the stream does not exist.
    #[cfg(target_os = "windows")]
    pub fn ring(
        &self,
        window: WindowId,
        stream: u64,
    ) -> Result<Arc<Mutex<SampleRing>>, AudioStreamError> {
        self.registry.ring(window, stream)
    }

    /// Drains the playback control intents not yet seen — pull-shaped because
    /// the mix loop's cadence is the engine's own.
    #[cfg(target_os = "windows")]
    #[must_use]
    pub fn drain_control(&self, window: WindowId, stream: u64) -> Vec<ControlIntent> {
        self.registry.drain_control(window, stream)
    }
}

impl AudioStreams {
    /// Creates an audio stream on `window`: opens the device stream with its
    /// own ring and returns the id the page will name it by.
    ///
    /// The device stream starts immediately — an audio stream that exists but
    /// cannot play is a half-truth the first chunk would discover too late.
    /// The ring starts empty; the device's first callbacks underrun and the
    /// accounting counts the silence honestly until the producer catches up.
    #[cfg(target_os = "windows")]
    pub(crate) fn create(&self, window: WindowId) -> Result<u64, AudioStreamError> {
        // 250ms of ring — the model's maximum: deep enough for the producer's
        // jitter, shallow enough that a scrub never waits out stale audio.
        const RING_MS: u32 = 250;
        let ring_inner = SampleRing::new(RING_MS)
            .ok_or_else(|| AudioStreamError::Ring("ring capacity outside the model".to_owned()))?;
        let ring = Arc::new(Mutex::new(ring_inner));
        let output = AudioOutput::start(ring)
            .map_err(|error| AudioStreamError::Device(error.to_string()))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                (window, id),
                AudioStreamEntry {
                    output,
                    pending_control: Vec::new(),
                },
            );
        Ok(id)
    }

    /// The ring handle for one stream — how the producer escapes the registry
    /// for its stream's lifetime.
    #[cfg(target_os = "windows")]
    pub(crate) fn ring(
        &self,
        window: WindowId,
        stream: u64,
    ) -> Result<Arc<Mutex<SampleRing>>, AudioStreamError> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(window, stream))
            .map(|entry| entry.output.ring())
            .ok_or(AudioStreamError::NoSuchStream(window, stream))
    }

    /// Pushes one chunk into a stream's ring — the convenience path for
    /// producers that push rarely enough not to care about the registry lock.
    /// Hot producers take [`Self::ring`] once instead.
    #[cfg(target_os = "windows")]
    #[allow(
        dead_code,
        reason = "the convenience path exists for cold producers and tests; the hot path is the ring handle"
    )]
    pub(crate) fn push(
        &self,
        window: WindowId,
        stream: u64,
        chunk: &AudioChunk,
    ) -> Result<(), AudioStreamError> {
        let ring = self.ring(window, stream)?;
        ring.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(chunk)
            .map_err(|error| AudioStreamError::Ring(format!("{error:?}")))
    }

    /// Records a control intent for the producer to drain.
    ///
    /// The runtime's whole job here is faithful transport: intents arrive in
    /// order, are never coalesced and never reordered, because a scrub's every
    /// position is one the user asked for. What the producer does with them —
    /// when pause actually silences, how a seek flushes — is the engine's
    /// business, because the engine owns time.
    #[cfg(target_os = "windows")]
    pub(crate) fn control(
        &self,
        window: WindowId,
        stream: u64,
        intent: ControlIntent,
    ) -> Result<(), AudioStreamError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries
            .get_mut(&(window, stream))
            .ok_or(AudioStreamError::NoSuchStream(window, stream))?;
        entry.pending_control.push(intent);
        Ok(())
    }

    /// Drains the control intents a producer has not yet seen.
    ///
    /// Pull-shaped for the same reason as video: the mix loop's cadence is the
    /// engine's own, and a push-shaped callback would be the runtime reaching
    /// into a thread it does not own.
    #[cfg(target_os = "windows")]
    pub(crate) fn drain_control(&self, window: WindowId, stream: u64) -> Vec<ControlIntent> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries
            .get_mut(&(window, stream))
            .map_or_else(Vec::new, |entry| std::mem::take(&mut entry.pending_control))
    }

    /// Tears one stream down: the device stream stops, the ring's accounting
    /// closes.
    #[cfg(target_os = "windows")]
    pub(crate) fn teardown(&self, window: WindowId, stream: u64) -> Result<(), AudioStreamError> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(window, stream))
            .ok_or(AudioStreamError::NoSuchStream(window, stream))?;
        Ok(())
    }

    /// Tears down every stream a window owns — the window-close path, where no
    /// stream may outlive the window its page died with.
    #[cfg(target_os = "windows")]
    pub(crate) fn teardown_window(&self, window: WindowId) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let keys: Vec<(WindowId, u64)> = entries
            .keys()
            .filter(|(owner, _)| *owner == window)
            .copied()
            .collect();
        for key in keys {
            entries.remove(&key);
        }
    }

    /// A stream's truth for the introspection surface: the ring's loss
    /// accounting and the sync reading the lip-sync measurement consumes.
    #[cfg(target_os = "windows")]
    pub(crate) fn inspect(
        &self,
        window: WindowId,
        stream: u64,
    ) -> Option<(RingAccounting, Option<SyncReading>)> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.get(&(window, stream)).map(|entry| {
            let accounting = entry
                .output
                .ring()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .accounting();
            (accounting, entry.output.sync_reading())
        })
    }
}
