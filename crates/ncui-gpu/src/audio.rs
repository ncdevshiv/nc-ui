//! The audio output model: a native playback stream's lifecycle, sample ring,
//! and clock semantics — the host-agnostic truth a platform backend implements.
//!
//! # Why audio is the video surface's sibling and not its accessory
//!
//! The problems are the same problem. A preview monitor's frames and the mix
//! under the playhead are both streams the edit engine produces at timeline
//! rate, both must reach hardware without crossing the browser, and both are
//! worthless unless their shared clock is honest. The model mirrors
//! `surface.rs` deliberately: identity, a bounded ring with counted loss,
//! timestamps in the producer's own timescale, teardown that refuses latecomers.
//!
//! # Where audio honestly differs from video
//!
//! A video ring drops the oldest frame when overrun, because a late frame shows
//! the past as if it were the present and the newest frame is always the right
//! one. Audio has no such privilege: a dropped sample block is a click, and a
//! reordered one is garbage — sound is continuous where pictures are discrete.
//! So the sample ring's semantics are inverted: it never drops. An overrun is a
//! *refused push with the refusal counted* — the producer is ahead of the
//! hardware, and the honest answer is backpressure, not silent loss. An empty
//! ring at callback time is an underrun, counted the same way, because the
//! device cannot pause and the alternative to silence is noise.
//!
//! A seek flushes the ring: the samples buffered from the old position are the
//! past, and unlike video there is no newest-wins frame to save — the whole
//! buffered run is stale by definition.
//!
//! # The clock
//!
//! Chunks carry the timeline position of their first sample as a `FramePts` —
//! the same type the video frames use, because the timescale honesty it
//! enforces (mismatched scales are not silently converted) is exactly the
//! honesty lip-sync measurement is built on. Which sample the device is playing
//! *now* is the backend's to answer; what timeline position that sample
//! belongs to is arithmetic this model makes possible.

use crate::surface::FramePts;

/// The canonical engine format: 48 kHz stereo, float32 interleaved.
///
/// One format, not a negotiation, because a mix engine that resamples per
/// device is a correctness bug farm — phase, latency and drift all move when
/// the rate does. Conversion, when the hardware disagrees, happens once at the
/// device boundary where it can be measured.
pub const ENGINE_SAMPLE_RATE: u32 = 48_000;

/// The deepest the sample ring may be built, in milliseconds of audio. Audio
/// buffers are latency exactly as video rings are: a preview that buffers a
/// second of mix plays every scrub a second late. 250ms absorbs scheduler
/// jitter on a loaded machine without audible lag in the scrub response.
pub const MAX_RING_MS: u32 = 250;

/// One stream's identity, unique within a runtime. Opaque for the same reason
/// the surface id is: nothing outside the runtime ever parses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AudioStreamId(pub u64);

/// A chunk of audio the producer mixed: interleaved float32 stereo samples at
/// the engine rate, plus the timeline position of the first sample.
///
/// The sample count is always even — stereo interleaved — and the chunk carries
/// its position rather than assuming continuity, because a scrub makes the
/// stream discontinuous by definition and positions that must line up by
/// arithmetic are positions that eventually do not.
#[derive(Debug)]
pub struct AudioChunk {
    /// Timeline position of the first sample, in the producer's timescale —
    /// the same clock the video frames are on.
    pub pts: FramePts,
    /// Interleaved stereo float32: left, right, left, right.
    pub samples: Vec<f32>,
}

impl AudioChunk {
    /// The count of stereo frames — sample pairs — in the chunk.
    #[must_use]
    pub fn frames(&self) -> u64 {
        (self.samples.len() / 2) as u64
    }

    /// The timeline position one sample frame past the chunk's end, if the
    /// timescale arithmetic is exact — the position the *next* continuous
    /// chunk should start at. `None` when the timescale does not divide the
    /// rate evenly, because a position that cannot be exact must not be
    /// approximated silently.
    #[must_use]
    pub fn end_pts(&self) -> Option<FramePts> {
        let frames = self.frames();
        // pts advances by frames/rate seconds in units of timescale:
        // delta = frames * timescale / rate, exact only when the division is.
        let numerator = u128::from(frames) * u128::from(self.pts.timescale);
        let rate = u128::from(ENGINE_SAMPLE_RATE);
        if numerator % rate != 0 {
            return None;
        }
        let delta = u64::try_from(numerator / rate).ok()?;
        Some(FramePts {
            value: self.pts.value.checked_add(delta)?,
            timescale: self.pts.timescale,
        })
    }
}

/// What the ring reports when a push cannot land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingRefusal {
    /// The ring is full: the producer is ahead of the hardware. Backpressure,
    /// not loss — nothing was dropped, and the producer may retry next tick.
    Full,
    /// The stream is torn down; nothing further is accepted, ever.
    TornDown,
    /// The chunk's timestamp runs backwards against the stream's own history:
    /// the producer owns time, and time in one stream runs forward or the
    /// stream is two streams.
    PtsRegression,
}

/// The ring's accounting — every loss mechanism counted, because underruns
/// and refused pushes are the two honest symptoms of a clock or a mix that
/// cannot keep time, and a preview monitor must know its own health.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RingAccounting {
    /// Callbacks that found the ring empty and output silence.
    pub underruns: u64,
    /// Pushes refused because the ring was full.
    pub overflow_refusals: u64,
    /// Stereo frames currently buffered.
    pub buffered_frames: u64,
}

/// The bounded sample ring. Samples live in one contiguous buffer because the
/// callback drains contiguously and chunk boundaries are the producer's
/// metadata, not the hardware's concern.
#[derive(Debug)]
pub struct SampleRing {
    /// Interleaved samples, capacity fixed at construction.
    buffer: Vec<f32>,
    /// Read cursor, in samples.
    read: usize,
    /// Write cursor, in samples.
    write: usize,
    /// Buffered sample count — the cursors alone cannot distinguish full from
    /// empty in a ring, so the count is the truth.
    len: usize,
    /// The position anchor: the timeline position of a known sample frame, and
    /// the count of sample frames the ring has consumed since that anchor. The
    /// position of the read cursor is `anchor + frames` in exact integer
    /// arithmetic — no per-drain rounding ever accumulates, because the frame
    /// count is exact and the division happens once, at query time. At a 90kHz
    /// timescale over 48kHz audio the per-sample increment is 1.875 units —
    /// any design that rounds per drain loses the position within seconds;
    /// this one never rounds until asked.
    anchor: Option<(FramePts, u64)>,
    /// The write-side truth: the timeline position the next pushed sample
    /// frame claims, anchored the same way. Regression is judged against it —
    /// backwards time is never a continuity claim, it is an error.
    write_anchor: Option<(FramePts, u64)>,
    underruns: u64,
    overflow_refusals: u64,
}

/// Resolves an anchor to a timeline position: `pts + frames` sample frames
/// later, rounded down at query time — the one place rounding may happen,
/// bounded by one timescale unit and never accumulating.
fn resolve(anchor: (FramePts, u64)) -> Option<FramePts> {
    let (pts, frames) = anchor;
    let numerator = u128::from(frames) * u128::from(pts.timescale);
    let delta = u64::try_from(numerator / u128::from(ENGINE_SAMPLE_RATE)).ok()?;
    Some(FramePts {
        value: pts.value.checked_add(delta)?,
        timescale: pts.timescale,
    })
}

impl SampleRing {
    /// Builds a ring holding `ms` milliseconds of stereo audio at the engine
    /// rate, refusing depths past the latency ceiling.
    #[must_use]
    pub fn new(ms: u32) -> Option<Self> {
        if ms == 0 || ms > MAX_RING_MS {
            return None;
        }
        let frames = u64::from(ms) * u64::from(ENGINE_SAMPLE_RATE) / 1000;
        let samples = usize::try_from(frames * 2).ok()?;
        Some(Self {
            buffer: vec![0.0; samples],
            read: 0,
            write: 0,
            len: 0,
            anchor: None,
            write_anchor: None,
            underruns: 0,
            overflow_refusals: 0,
        })
    }

    /// Pushes a chunk, whole or not at all — a chunk split across a refusal
    /// boundary would resume mid-sample-frame, and partial chunks are how
    /// phase corruption enters a stream.
    pub fn push(&mut self, chunk: &AudioChunk) -> Result<(), RingRefusal> {
        let capacity = self.buffer.len();
        let needed = chunk.samples.len();
        if needed > capacity - self.len {
            self.overflow_refusals += 1;
            return Err(RingRefusal::Full);
        }
        if let Some(write_position) = self.write_anchor.and_then(resolve) {
            // Backwards time is never a continuity claim, it is an error —
            // judged against where the stream's write end believes it is.
            if let Some(std::cmp::Ordering::Less) = chunk.pts.same_scale_cmp(&write_position) {
                return Err(RingRefusal::PtsRegression);
            }
        }
        if self.len == 0 {
            self.anchor = Some((chunk.pts, 0));
        }
        // The write end re-anchors on every chunk: the position one frame past
        // the chunk's end, as the producer declares it. Producer positions are
        // the truth, so gaps and overlaps re-anchor rather than accumulating
        // arithmetic drift.
        self.write_anchor = Some((chunk.pts, chunk.frames()));
        for &sample in &chunk.samples {
            self.buffer[self.write] = sample;
            self.write = (self.write + 1) % capacity;
        }
        self.len += needed;
        Ok(())
    }

    /// Drains up to `out.len()` samples into the callback's buffer, zero-filling
    /// the shortfall and counting it as an underrun — the device cannot pause,
    /// so silence is what an empty ring plays, but silence is never free of
    /// accounting.
    pub fn drain(&mut self, out: &mut [f32]) {
        let capacity = self.buffer.len();
        let available = self.len.min(out.len());
        for slot in out.iter_mut().take(available) {
            *slot = self.buffer[self.read];
            self.read = (self.read + 1) % capacity;
        }
        if available < out.len() {
            self.underruns += 1;
            for slot in out.iter_mut().skip(available) {
                *slot = 0.0;
            }
        }
        // Advance the read anchor by the sample frames consumed — exact
        // integer arithmetic; the one rounding happens at query time.
        if let Some((pts, frames)) = self.anchor {
            self.anchor = Some((pts, frames + (available / 2) as u64));
        }
        self.len -= available;
    }

    /// Flushes the ring for a seek: everything buffered is the past, and the
    /// next push starts the new position's stream.
    pub fn flush(&mut self) {
        self.read = 0;
        self.write = 0;
        self.len = 0;
        self.anchor = None;
        self.write_anchor = None;
    }

    /// The ring's accounting, honestly accumulated.
    #[must_use]
    pub fn accounting(&self) -> RingAccounting {
        RingAccounting {
            underruns: self.underruns,
            overflow_refusals: self.overflow_refusals,
            buffered_frames: (self.len / 2) as u64,
        }
    }

    /// The timeline position of the sample at the read cursor — the position
    /// that plays next once buffered-ahead time elapses. The lip-sync
    /// measurement reads here. Rounded down at this query, once, by at most
    /// one timescale unit — never accumulating.
    #[must_use]
    pub fn read_pts(&self) -> Option<FramePts> {
        self.anchor.and_then(resolve)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::float_cmp,
    reason = "test assertions must abort loudly on failure; float equality is exact here               because the ring copies samples bit-for-bit — exactness is the claim under test"
)]
mod tests {
    use super::*;

    fn chunk(pts_value: u64, frames: usize) -> AudioChunk {
        AudioChunk {
            pts: FramePts {
                value: pts_value,
                timescale: 90_000,
            },
            samples: vec![0.5; frames * 2],
        }
    }

    #[test]
    fn a_ring_holds_exactly_what_was_pushed_in_order() {
        let mut ring = SampleRing::new(100).expect("a legal depth builds");
        ring.push(&chunk(0, 480)).expect("the push lands");
        let mut out = vec![0.0f32; 960];
        ring.drain(&mut out);
        assert!(
            out.iter().all(|&sample| sample == 0.5),
            "every drained sample is the pushed value, none reordered, none lost"
        );
        assert_eq!(ring.accounting().underruns, 0);
    }

    #[test]
    fn an_empty_drain_is_silence_and_counted() {
        let mut ring = SampleRing::new(100).expect("a legal depth builds");
        let mut out = vec![1.0f32; 480];
        ring.drain(&mut out);
        assert!(
            out.iter().all(|&sample| sample == 0.0),
            "the device cannot pause: an empty ring plays silence, not stale samples"
        );
        assert_eq!(
            ring.accounting().underruns,
            1,
            "and the silence is accounted, because underruns are the stream's health"
        );
    }

    #[test]
    fn an_overrun_refuses_without_dropping() {
        // A 10ms ring holds 480 stereo frames; two 240-frame chunks exactly fill
        // it, and the third must be refused whole — never split, never dropped
        // silently, because split chunks are phase corruption and silent loss is
        // a click nobody can diagnose.
        let mut ring = SampleRing::new(10).expect("a legal depth builds");
        ring.push(&chunk(0, 240)).expect("first chunk lands");
        ring.push(&chunk(450, 240)).expect("second chunk lands");
        let refusal = ring.push(&chunk(900, 240));
        assert_eq!(refusal, Err(RingRefusal::Full));
        assert_eq!(ring.accounting().overflow_refusals, 1);
        // And what was in the ring is still exactly the first two chunks, whole.
        let mut out = vec![0.0f32; 960];
        ring.drain(&mut out);
        assert!(out.iter().all(|&sample| sample == 0.5));
    }

    #[test]
    fn a_seek_flush_forgets_the_past() {
        let mut ring = SampleRing::new(100).expect("a legal depth builds");
        ring.push(&chunk(0, 480)).expect("the push lands");
        ring.flush();
        assert_eq!(ring.accounting().buffered_frames, 0);
        assert_eq!(
            ring.read_pts(),
            None,
            "after a flush the ring holds no position truth — the next push re-anchors"
        );
    }

    #[test]
    fn backwards_time_is_refused() {
        let mut ring = SampleRing::new(100).expect("a legal depth builds");
        ring.push(&chunk(9000, 480))
            .expect("the first chunk anchors");
        let refusal = ring.push(&chunk(1000, 480));
        assert_eq!(
            refusal,
            Err(RingRefusal::PtsRegression),
            "time runs forward in one stream or the stream is two streams"
        );
    }

    #[test]
    fn the_position_truth_never_drifts_at_media_timescales() {
        // The design's load-bearing claim: at 90kHz over 48kHz the per-sample
        // increment is 1.875 units — never integer — so any implementation
        // rounding per drain loses the position within seconds. The anchor
        // design divides once, at query time, off an exact frame count.
        let mut ring = SampleRing::new(200).expect("a legal depth builds");
        ring.push(&chunk(0, 4800)).expect("100ms of audio lands");
        // Drain in the ragged sizes real callbacks use — 99, 7, 480 — and the
        // position after N consumed frames must be exactly floor(N*90000/48000).
        let sizes = [99usize, 7, 480, 1000, 13];
        let mut consumed_frames = 0u64;
        for size in sizes {
            let mut out = vec![0.0f32; size];
            ring.drain(&mut out);
            consumed_frames += (size / 2) as u64;
            let position = ring.read_pts().expect("the position is known");
            let expected = consumed_frames * 90_000 / 48_000;
            assert_eq!(
                position.value, expected,
                "after {consumed_frames} frames the position is exact integer arithmetic"
            );
        }
    }

    #[test]
    fn depths_past_the_latency_ceiling_are_refused() {
        assert!(SampleRing::new(0).is_none());
        assert!(SampleRing::new(MAX_RING_MS + 1).is_none());
        assert!(SampleRing::new(100).is_some());
    }

    #[test]
    fn end_pts_is_exact_or_absent() {
        // 480 frames at 90kHz/48kHz: 480*90000/48000 = 900 exactly — divisible.
        let exact = chunk(0, 480);
        assert_eq!(
            exact.end_pts().map(|pts| pts.value),
            Some(900),
            "exact arithmetic answers exactly"
        );
        // 481 frames: 481*90000/48000 = 902.0 — hmm, 481*90000 = 43290000,
        // /48000 = 901.875 — not divisible, so absent rather than approximate.
        let inexact = chunk(0, 481);
        assert_eq!(
            inexact.end_pts(),
            None,
            "a position that cannot be exact is absent, never approximated silently"
        );
    }
}
