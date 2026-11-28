//! The video surface model: a persistent frame surface's lifecycle, frame ring,
//! and control semantics — the host-agnostic truth a platform presenter implements.
//!
//! # Why a surface is not an island
//!
//! A GPU island is a one-shot rasterisation: the page asks, the runtime draws, the
//! pixels cross to the page once. A video surface is a *persistent* object with an
//! identity and a frame lifecycle: a producer (the edit engine, or a synthetic
//! producer in the substrate's own evidence) emits frames at video rate, a
//! presenter (platform code — a child surface the OS composites, never pixels
//! through the browser) presents them, and the page controls playback position
//! through engine-time intents. Nothing here is HTTP-shaped because nothing at
//! video rate can afford to be.
//!
//! # The ring's honest semantics
//!
//! The frame ring is small and bounded on purpose. A preview monitor never wants
//! to fall behind: a frame presented late is worse than a frame never presented,
//! because the late one shows the past as if it were the present. So the ring
//! drops the *oldest* unconsumed frame when a producer overruns, counts every
//! drop, and the presenter always drains toward the newest. Frame accuracy in a
//! scrub is not the ring's burden at all — it is the producer's: the producer
//! emits exactly the frame the scrub position names, and that frame is by
//! construction the newest, so a latest-wins ring can never lose it.
//!
//! # What the model refuses
//!
//! A zero-sized or oversized surface is refused at construction rather than
//! clamped — the dimensions come from the page's layout, and a page's numbers
//! are input. Frames pushed after teardown begins are refused, because a
//! surface presenting after its teardown is a use-after-free wearing a type.
//! PTS values that run backwards inside one stream are refused: the producer
//! owns time, and time in one stream runs forward or the stream is two streams.

use crate::geometry::LogicalSize;

/// The most pixels one surface may carry: beyond UHD-4K at 8K the copy costs
/// even the zero-copy presenter pays in bandwidth stop being a preview monitor
/// and start being a stress test. 7680×4320, matching the island bound's logic.
pub const MAX_SURFACE_PIXELS: u64 = 7680 * 4320;

/// The deepest the frame ring may be built. A deeper ring is not smoothness, it
/// is latency: frames queue up behind the present and the preview runs behind
/// the edit. Triple buffering plus one for producer/presenter overlap is the
/// honest maximum.
pub const MAX_RING_DEPTH: usize = 4;

/// One surface's identity, unique within a runtime.
///
/// The id is opaque on purpose: the page names a surface by the element it
/// places, and the runtime maps element to id — so the id never needs to be
/// parseable, guessable, or ordered against anything but itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SurfaceId(pub u64);

/// A frame timestamp in the producer's own clock: engine time, not wall time.
///
/// The unit is the producer's timescale, carried alongside so two producers with
/// different timescales cannot be compared by accident — a 90 kHz media clock and
/// a millisecond clock both fit, and neither pretends to be the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FramePts {
    /// The timestamp value in units of `timescale` per second.
    pub value: u64,
    /// Units per second — `90_000` for a media clock, `1_000` for milliseconds.
    pub timescale: u32,
}

impl FramePts {
    /// Orders two timestamps of the *same* timescale; mismatched scales are not
    /// orderable, because converting them silently is how A/V sync errors are born.
    #[must_use]
    pub fn same_scale_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        (self.timescale == other.timescale).then(|| self.value.cmp(&other.value))
    }
}

/// A rectangular region of a frame that changed since the previous frame, in
/// frame pixels.
///
/// Damage is advisory for the presenter — a partial redraw is an optimisation —
/// but it is never advisory for correctness: a pixel outside every damage rect
/// is identical to the previous frame's, and a producer that lies about that is
/// a producer whose frames cannot be trusted anywhere else either.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Damage {
    /// Inclusive origin and exclusive extent, in frame pixels.
    pub x: u32,
    /// Vertical origin.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Damage {
    /// The smallest rect covering both — damage merges when frames coalesce in
    /// the ring, because the merge must not lose a changed pixel.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = (self.x + self.width).max(other.x + other.width);
        let y1 = (self.y + self.height).max(other.y + other.height);
        Self {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        }
    }

    /// The whole frame — the correct answer for the first frame and for a
    /// producer that does not track damage.
    #[must_use]
    pub fn full(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }
}

/// The dimensions and format a surface is built with.
///
/// Dimensions are frame pixels, which on the presenting side map to the child
/// surface's physical pixels — the logical-to-physical conversion is the
/// placement machinery's, not the spec's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceSpec {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

impl SurfaceSpec {
    /// Validates the spec a page's layout produced. Refusals, not clamps:
    /// a zero dimension is a layout bug the page must see, and an oversized one
    /// is either hostile or a unit error, and neither should draw quietly.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::InvalidSpec`] naming which bound the spec broke.
    pub fn validated(&self) -> Result<Self, SurfaceError> {
        if self.width == 0 || self.height == 0 {
            return Err(SurfaceError::InvalidSpec {
                reason: format!(
                    "a surface dimension may not be zero ({}x{})",
                    self.width, self.height
                ),
            });
        }
        if u64::from(self.width) * u64::from(self.height) > MAX_SURFACE_PIXELS {
            return Err(SurfaceError::InvalidSpec {
                reason: format!(
                    "{}x{} is past the {}-pixel surface bound",
                    self.width, self.height, MAX_SURFACE_PIXELS
                ),
            });
        }
        Ok(*self)
    }

    /// The logical size the placement contract uses.
    #[must_use]
    pub fn logical_size(&self) -> LogicalSize {
        LogicalSize::new(self.width as f32, self.height as f32)
    }
}

/// What the surface is doing, in the producer's time model.
///
/// The clock itself lives in the engine — the surface does not own time, it
/// carries the *control state* the page asked for, so a presenter deciding
/// whether to hold or advance can read the same truth the engine acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    /// Frames flow at the stream's own rate.
    Playing,
    /// The current frame holds; new frames may still be pushed (a paused
    /// producer keeps its place) but the presenter does not advance.
    Paused,
    /// The page is dragging the position: frames arrive out of real time,
    /// each one the exact frame of the dragged-to instant.
    Scrubbing,
}

/// A control intent the page issues against the surface.
///
/// Intents route to the producer — the surface is a conduit, not an executor:
/// only the engine can seek, because only the engine knows what frame lives at
/// a time. What the model guarantees is that intents are ordered, never
/// silently dropped, and answered — even when the answer is that the surface
/// is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlIntent {
    /// Pause at the current position.
    Pause,
    /// Resume playback.
    Resume,
    /// Jump to an engine-time position; the next presented frame is exactly
    /// that position's frame.
    SeekTo(FramePts),
    /// Move a signed count of frames from the current position — the frame-step
    /// keys every editor has.
    Step(i32),
}

/// One frame in the ring: metadata the model owns, payload the transport owns.
///
/// The payload is generic because the pixel transport is presenter-specific —
/// a GPU texture on the zero-copy path, a CPU buffer in the model's own tests —
/// and the ring's semantics (ordering, dropping, damage merging) are true of
/// the metadata regardless of what the pixels ride in.
#[derive(Debug)]
pub struct Frame<P> {
    /// Monotonic sequence within the stream: gaps are the ring's drop evidence.
    pub sequence: u64,
    /// Engine-time position of this frame.
    pub pts: FramePts,
    /// The regions that differ from the previous frame; empty means the
    /// producer did not say, which a presenter must read as full-frame damage.
    pub damage: Vec<Damage>,
    /// The pixels, in whatever form the transport carries them.
    pub payload: P,
}

/// The ring's answer to a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// The frame is in the ring.
    Accepted,
    /// The frame is in the ring; an older unconsumed frame was dropped to make
    /// room, and this is how many the ring has dropped in total.
    AcceptedDroppedOldest {
        /// Running count of every frame this ring has dropped.
        total_dropped: u64,
    },
}

/// The errors the model refuses with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceError {
    /// The spec broke a bound.
    InvalidSpec {
        /// Which bound, in words the page's developer can act on.
        reason: String,
    },
    /// A frame or intent arrived after teardown began.
    TornDown,
    /// A frame's PTS ran backwards within one stream.
    NonMonotonicPts {
        /// The sequence number of the offending frame.
        sequence: u64,
    },
    /// The ring was built deeper than the honesty bound.
    RingTooDeep {
        /// The depth that was asked for.
        requested: usize,
    },
}

impl core::fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidSpec { reason } => write!(f, "invalid surface spec: {reason}"),
            Self::TornDown => write!(f, "the surface is torn down"),
            Self::NonMonotonicPts { sequence } => {
                write!(f, "frame {sequence} has a PTS behind its predecessor")
            }
            Self::RingTooDeep { requested } => {
                write!(f, "a ring depth of {requested} is latency, not smoothness")
            }
        }
    }
}

impl std::error::Error for SurfaceError {}

/// The frame ring: bounded, drop-oldest, drop-counting.
///
/// See the module docs for why these semantics are the honest ones for a
/// preview surface. Everything the ring does is O(1) — a video-rate path has
/// no business allocating or scanning per frame.
impl<P> core::fmt::Debug for FrameRing<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FrameRing")
            .field("len", &self.frames.len())
            .field("capacity", &self.capacity)
            .field("dropped", &self.dropped)
            .finish_non_exhaustive()
    }
}

pub struct FrameRing<P> {
    frames: std::collections::VecDeque<Frame<P>>,
    capacity: usize,
    dropped: u64,
    last_pts: Option<FramePts>,
}

impl<P> FrameRing<P> {
    /// Builds a ring of the given depth, bounded by [`MAX_RING_DEPTH`].
    ///
    /// # Errors
    ///
    /// [`SurfaceError::RingTooDeep`] past the bound.
    pub fn new(capacity: usize) -> Result<Self, SurfaceError> {
        if capacity == 0 || capacity > MAX_RING_DEPTH {
            return Err(SurfaceError::RingTooDeep {
                requested: capacity,
            });
        }
        Ok(Self {
            frames: std::collections::VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
            last_pts: None,
        })
    }

    /// Pushes a frame, enforcing PTS monotonicity within the stream and
    /// dropping the oldest unconsumed frame when the ring is full.
    ///
    /// When a full ring drops, the dropped frame's damage merges into the
    /// oldest surviving frame — a coalesced frame carries every change the
    /// coalescing swallowed, or the presenter would skip pixels that changed.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::NonMonotonicPts`] when the stream's time runs backwards.
    pub fn push(&mut self, mut frame: Frame<P>) -> Result<PushOutcome, SurfaceError> {
        if let Some(last) = self.last_pts
            && frame.pts.same_scale_cmp(&last) == Some(core::cmp::Ordering::Less)
        {
            return Err(SurfaceError::NonMonotonicPts {
                sequence: frame.sequence,
            });
        }
        self.last_pts = Some(frame.pts);
        if self.frames.len() == self.capacity
            && let Some(mut oldest) = self.frames.pop_front()
        {
            self.dropped += 1;
            if let Some(survivor) = self.frames.front_mut() {
                let mut merged = std::mem::take(&mut survivor.damage);
                for rect in &oldest.damage {
                    if !merged.iter().any(|r| r == rect) {
                        merged.push(*rect);
                    }
                }
                survivor.damage = merged;
            }
            oldest.damage.clear();
            self.frames.push_back(frame);
            return Ok(PushOutcome::AcceptedDroppedOldest {
                total_dropped: self.dropped,
            });
        }
        frame.damage.shrink_to_fit();
        self.frames.push_back(frame);
        Ok(PushOutcome::Accepted)
    }

    /// Drains toward the newest frame: everything older than the newest is
    /// consumed and dropped on the spot, with its damage merged forward, because
    /// the presenter never wants the past. Returns the newest unconsumed frame.
    pub fn drain_to_newest(&mut self) -> Option<Frame<P>> {
        while self.frames.len() > 1 {
            let dropped = self.frames.pop_front()?;
            self.dropped += 1;
            if let Some(survivor) = self.frames.front_mut() {
                for rect in &dropped.damage {
                    if !survivor.damage.iter().any(|r| r == rect) {
                        survivor.damage.push(*rect);
                    }
                }
            }
        }
        self.frames.pop_front()
    }

    /// The oldest unconsumed frame, for a presenter in lockstep (scrub/step)
    /// rather than free-running mode.
    pub fn pop_oldest(&mut self) -> Option<Frame<P>> {
        self.frames.pop_front()
    }

    /// Frames dropped for any reason — the metric the acceptance criteria read.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Unconsumed depth right now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// The surface lifecycle: live, then torn down exactly once.
///
/// Teardown is idempotent because the triggering paths are plural — the page
/// navigates, the element unmounts, the window closes — and which one lands
/// first is a race the model should not care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// Accepting frames and intents.
    Live,
    /// Torn down: frames and intents are refused, and the presenter may drain
    /// what is already queued before the platform surface is destroyed.
    TornDown,
}

/// One surface: spec, lifecycle, playback state, and its ring.
///
/// The presenter and producer share this object through whatever concurrency
/// the runtime wires — the model is deliberately thread-agnostic, because the
/// locking strategy is a host decision, not a semantic one.
impl<P> core::fmt::Debug for Surface<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Surface")
            .field("id", &self.id)
            .field("spec", &self.spec)
            .field("lifecycle", &self.lifecycle)
            .field("playback", &self.playback)
            .finish_non_exhaustive()
    }
}

pub struct Surface<P> {
    id: SurfaceId,
    spec: SurfaceSpec,
    lifecycle: Lifecycle,
    playback: Playback,
    ring: FrameRing<P>,
}

impl<P> Surface<P> {
    /// Builds a live surface.
    ///
    /// # Errors
    ///
    /// The spec and ring validations', unchanged.
    pub fn new(id: SurfaceId, spec: SurfaceSpec, ring_depth: usize) -> Result<Self, SurfaceError> {
        Ok(Self {
            id,
            spec: spec.validated()?,
            lifecycle: Lifecycle::Live,
            playback: Playback::Playing,
            ring: FrameRing::new(ring_depth)?,
        })
    }

    /// The surface's identity.
    #[must_use]
    pub fn id(&self) -> SurfaceId {
        self.id
    }

    /// The validated spec.
    #[must_use]
    pub fn spec(&self) -> SurfaceSpec {
        self.spec
    }

    /// Current playback state.
    #[must_use]
    pub fn playback(&self) -> Playback {
        self.playback
    }

    /// The ring, for producers pushing and presenters draining.
    pub fn ring(&mut self) -> &mut FrameRing<P> {
        &mut self.ring
    }

    /// How many frames the ring has dropped in total — the read-only inspection
    /// a metrics surface needs, distinct from `ring()` because looking must not
    /// require the borrow that draining does.
    pub fn dropped_frames(&self) -> u64 {
        self.ring.dropped()
    }

    /// How many frames are queued right now.
    pub fn queued_frames(&self) -> usize {
        self.ring.len()
    }

    /// Applies a control intent, updating the playback state the presenter reads.
    ///
    /// The intent is returned to the caller for forwarding to the producer —
    /// the surface records, the producer acts, and neither confuses the other's
    /// job. Seeking and stepping both imply scrub-time semantics; resume returns
    /// to rate playback.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::TornDown`] after teardown.
    pub fn control(&mut self, intent: ControlIntent) -> Result<ControlIntent, SurfaceError> {
        if self.lifecycle == Lifecycle::TornDown {
            return Err(SurfaceError::TornDown);
        }
        self.playback = match intent {
            ControlIntent::Pause => Playback::Paused,
            ControlIntent::Resume => Playback::Playing,
            ControlIntent::SeekTo(_) | ControlIntent::Step(_) => Playback::Scrubbing,
        };
        Ok(intent)
    }

    /// Pushes a frame unless the surface is torn down.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::TornDown`], or the ring's own refusals.
    pub fn push(&mut self, frame: Frame<P>) -> Result<PushOutcome, SurfaceError> {
        if self.lifecycle == Lifecycle::TornDown {
            return Err(SurfaceError::TornDown);
        }
        self.ring.push(frame)
    }

    /// Tears the surface down, idempotently: the second call is a no-op rather
    /// than an error, because the racing teardown paths are each correct.
    pub fn teardown(&mut self) {
        self.lifecycle = Lifecycle::TornDown;
    }

    /// Whether the surface is still accepting.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.lifecycle == Lifecycle::Live
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
mod tests {
    use super::*;

    fn pts(value: u64) -> FramePts {
        FramePts {
            value,
            timescale: 90_000,
        }
    }

    fn frame(sequence: u64, pts_value: u64) -> Frame<Vec<u8>> {
        Frame {
            sequence,
            pts: pts(pts_value),
            damage: Vec::new(),
            payload: vec![sequence as u8],
        }
    }

    fn spec() -> SurfaceSpec {
        SurfaceSpec {
            width: 1920,
            height: 1080,
        }
    }

    #[test]
    fn a_surface_refuses_zero_and_oversized_dimensions() {
        let zero = SurfaceSpec {
            width: 0,
            height: 1080,
        };
        assert!(
            zero.validated().is_err(),
            "zero width is a layout bug, not a clamp"
        );
        let zero_h = SurfaceSpec {
            width: 1920,
            height: 0,
        };
        assert!(zero_h.validated().is_err());
        let huge = SurfaceSpec {
            width: 7681,
            height: 4320,
        };
        assert!(huge.validated().is_err(), "past the pixel bound is refused");
        assert!(spec().validated().is_ok(), "1080p is a normal surface");
    }

    #[test]
    fn the_ring_drops_oldest_on_overrun_and_counts_every_drop() {
        let mut ring = FrameRing::new(2).expect("depth two is honest");
        assert!(matches!(
            ring.push(frame(1, 100)).expect("push"),
            PushOutcome::Accepted
        ));
        ring.push(frame(2, 200)).expect("push");
        let outcome = ring.push(frame(3, 300)).expect("push");
        assert_eq!(
            outcome,
            PushOutcome::AcceptedDroppedOldest { total_dropped: 1 },
            "the third frame evicts the first"
        );
        ring.push(frame(4, 400)).expect("push");
        ring.push(frame(5, 500)).expect("push");
        assert_eq!(ring.dropped(), 3, "every overrun is counted");
        // The survivors are the newest two — the ring never keeps the past at
        // the expense of the present.
        let newest = ring.drain_to_newest().expect("a frame waits");
        assert_eq!(newest.sequence, 5);
    }

    #[test]
    fn drain_to_newest_skips_the_middle_and_preserves_order() {
        let mut ring = FrameRing::new(4).expect("ring");
        for i in 1..=4 {
            ring.push(frame(i, i * 100)).expect("push");
        }
        let newest = ring.drain_to_newest().expect("frame");
        assert_eq!(newest.sequence, 4, "only the present is worth presenting");
        assert_eq!(
            ring.dropped(),
            3,
            "the skipped frames are counted as dropped"
        );
        assert!(ring.is_empty());
    }

    #[test]
    fn damage_merges_forward_when_frames_coalesce() {
        let mut ring = FrameRing::new(2).expect("ring");
        let mut first = frame(1, 100);
        first.damage.push(Damage {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        });
        let mut second = frame(2, 200);
        second.damage.push(Damage {
            x: 50,
            y: 50,
            width: 10,
            height: 10,
        });
        ring.push(first).expect("push");
        ring.push(second).expect("push");
        // The first frame is dropped unconsumed; its damage must survive in the
        // second, or the presenter would skip a changed region.
        let third = frame(3, 300);
        ring.push(third).expect("push");
        let survivor = ring.pop_oldest().expect("the second frame survives");
        assert_eq!(survivor.sequence, 2);
        assert!(
            survivor.damage.iter().any(|r| r.x == 0 && r.y == 0),
            "the dropped frame's damage merged forward: {survivor:?}"
        );
        assert!(
            survivor.damage.iter().any(|r| r.x == 50 && r.y == 50),
            "the survivor's own damage is intact"
        );
    }

    #[test]
    fn a_pts_that_runs_backwards_is_refused() {
        let mut ring = FrameRing::new(2).expect("ring");
        ring.push(frame(1, 500)).expect("push");
        let error = ring
            .push(frame(2, 400))
            .expect_err("backwards time is two streams, not one");
        assert!(matches!(
            error,
            SurfaceError::NonMonotonicPts { sequence: 2 }
        ));
    }

    #[test]
    fn mismatched_timescales_are_not_ordered_silently() {
        let media = pts(100);
        let millis = FramePts {
            value: 100,
            timescale: 1_000,
        };
        assert_eq!(media.same_scale_cmp(&millis), None, "no silent conversion");
    }

    #[test]
    fn teardown_refuses_frames_and_intents_and_is_idempotent() {
        let mut surface = Surface::new(SurfaceId(1), spec(), 2).expect("spec is valid");
        surface.push(frame(1, 100)).expect("live surfaces accept");
        surface.teardown();
        surface.teardown();
        let pushed = surface.push(frame(2, 200));
        assert!(
            matches!(pushed, Err(SurfaceError::TornDown)),
            "no frames after teardown"
        );
        let controlled = surface.control(ControlIntent::Resume);
        assert!(matches!(controlled, Err(SurfaceError::TornDown)));
        assert!(!surface.is_live());
    }

    #[test]
    fn control_intents_update_the_playback_state_the_presenter_reads() {
        let mut surface = Surface::<Vec<u8>>::new(SurfaceId(1), spec(), 2).expect("valid");
        assert_eq!(surface.playback(), Playback::Playing);
        surface.control(ControlIntent::Pause).expect("live");
        assert_eq!(surface.playback(), Playback::Paused);
        surface
            .control(ControlIntent::SeekTo(pts(9000)))
            .expect("live");
        assert_eq!(
            surface.playback(),
            Playback::Scrubbing,
            "a seek is scrub-time"
        );
        surface.control(ControlIntent::Step(1)).expect("live");
        assert_eq!(surface.playback(), Playback::Scrubbing);
        surface.control(ControlIntent::Resume).expect("live");
        assert_eq!(surface.playback(), Playback::Playing);
    }

    #[test]
    fn the_ring_depth_bound_is_enforced() {
        assert!(
            FrameRing::<Vec<u8>>::new(0).is_err(),
            "zero depth is no surface"
        );
        assert!(FrameRing::<Vec<u8>>::new(MAX_RING_DEPTH).is_ok());
        assert!(
            FrameRing::<Vec<u8>>::new(MAX_RING_DEPTH + 1).is_err(),
            "deeper than the bound is latency, not smoothness"
        );
    }

    #[test]
    fn damage_union_covers_both_rects() {
        let a = Damage {
            x: 10,
            y: 10,
            width: 10,
            height: 10,
        };
        let b = Damage {
            x: 50,
            y: 5,
            width: 10,
            height: 10,
        };
        let union = a.union(&b);
        // a covers x 10..20 / y 10..20; b covers x 50..60 / y 5..15 — the union
        // spans x 10..60, y 5..20.
        assert_eq!(union.x, 10, "origin x is the leftmost edge");
        assert_eq!(union.y, 5, "origin y is the topmost edge");
        assert_eq!(union.width, 50, "covers to the furthest edge");
        assert_eq!(union.height, 15);
    }
}

/// The presenter contract: what platform code implements behind the model.
///
/// A presenter is the surface's body on one platform — the child `HWND` a
/// Windows runtime layers over the preview rect, or the test double that
/// proves the model without a window. The contract is small on purpose:
/// everything the model can prove stays in the model, and what remains here
/// is exactly what only platform code can do.
///
/// Implementations must be honest about failure: a present that fails is
/// reported, not swallowed, because a preview that silently freezes is the
/// one failure an editor operator cannot distinguish from a hang.
pub trait Presenter<P> {
    /// The platform error type — presenters fail in platform dialects.
    type Error;

    /// Presents one frame. The frame's damage is advisory; a presenter may
    /// always present full-frame, but it may never present *less* than the
    /// damage covers.
    ///
    /// # Errors
    ///
    /// The platform's own — a lost device, a destroyed window. The caller's
    /// correct response is teardown, not retry: the surface is gone.
    fn present(&mut self, frame: &Frame<P>) -> Result<(), Self::Error>;

    /// Moves and resizes the presented region, in the host window's physical
    /// pixels. The placement machinery converts; the presenter positions.
    ///
    /// # Errors
    ///
    /// The platform's own — positioning a destroyed surface.
    fn place(&mut self, rect: PlacementRect) -> Result<(), Self::Error>;

    /// Tears the platform surface down. After this call the presenter is
    /// dead weight and the runtime drops it.
    fn teardown(&mut self);
}

/// A rectangle in physical pixels relative to the host window's client area.
///
/// This is the presenter's coordinate truth: the page thinks in logical CSS
/// pixels relative to the document, the placement machinery converts through
/// scroll offset, device-pixel ratio and client origin, and what lands here
/// is final — the presenter does no conversion of its own, so a positioning
/// bug has exactly one home.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacementRect {
    /// Physical pixels from the client area's left edge.
    pub x: i32,
    /// Physical pixels from the client area's top edge.
    pub y: i32,
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
}

/// The placement conversion: document logical rect to host physical rect.
///
/// The inputs are everything that moves the mapping — scroll position,
/// device-pixel ratio, and the document rect itself — and the output is the
/// physical placement, or `None` when the rect is entirely clipped away.
/// Clipping is honest: a surface scrolled out of view is hidden, not
/// presented-at-negative-coordinates, because a child surface cannot be
/// clipped by the DOM and must not overdraw chrome it would cover.
///
/// The `clip` rect is the visible region the surface may occupy — typically
/// the window client area or a containing scroll viewport — in the same
/// document-logical space as `rect`.
#[must_use]
pub fn placement_physical(
    rect: (f32, f32, f32, f32),
    scroll: (f32, f32),
    device_pixel_ratio: f32,
    clip: (f32, f32, f32, f32),
) -> Option<PlacementRect> {
    let (rx, ry, rw, rh) = rect;
    let (cx, cy, cw, ch) = clip;
    // Clip in document space first: the part of the surface inside the viewport.
    let x0 = rx.max(cx);
    let y0 = ry.max(cy);
    let x1 = (rx + rw).min(cx + cw);
    let y1 = (ry + rh).min(cy + ch);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    // Then convert: unscrolled, scaled, rounded outward so no presented pixel
    // is ever missing — a hairline of overdraw is cosmetic, a missing edge is a lie.
    let dpr = device_pixel_ratio.max(f32::EPSILON);
    let phys_x0 = ((x0 - scroll.0) * dpr).floor() as i32;
    let phys_y0 = ((y0 - scroll.1) * dpr).floor() as i32;
    let phys_x1 = ((x1 - scroll.0) * dpr).ceil() as i32;
    let phys_y1 = ((y1 - scroll.1) * dpr).ceil() as i32;
    Some(PlacementRect {
        x: phys_x0,
        y: phys_y0,
        width: (phys_x1 - phys_x0).max(1) as u32,
        height: (phys_y1 - phys_y0).max(1) as u32,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp,
    reason = "test assertions must abort loudly on failure; placement geometry is exact on purpose"
)]
mod placement_tests {
    use super::*;

    #[test]
    fn a_rect_at_rest_converts_through_dpr_alone() {
        let placed = placement_physical(
            (100.0, 50.0, 640.0, 360.0),
            (0.0, 0.0),
            2.0,
            (0.0, 0.0, 4000.0, 4000.0),
        );
        let placed = placed.expect("fully visible");
        assert_eq!((placed.x, placed.y), (200, 100), "dpr doubles");
        assert_eq!((placed.width, placed.height), (1280, 720));
    }

    #[test]
    fn scroll_moves_the_physical_position_not_the_size() {
        let placed = placement_physical(
            (100.0, 100.0, 640.0, 360.0),
            (0.0, 60.0),
            1.0,
            (0.0, 0.0, 4000.0, 4000.0),
        )
        .expect("visible");
        assert_eq!(placed.y, 40, "scrolled up by 60 logical px");
        assert_eq!(placed.height, 360, "size is scroll-invariant");
    }

    #[test]
    fn a_rect_clipped_by_the_viewport_shrinks_to_the_visible_part() {
        // The viewport clip starts at document y=200; the surface starts at 100
        // and is 360 tall, so its first 100 logical px are clipped away.
        let placed = placement_physical(
            (100.0, 100.0, 640.0, 360.0),
            (0.0, 0.0),
            1.0,
            (0.0, 200.0, 4000.0, 2000.0),
        )
        .expect("partly visible");
        assert_eq!(placed.y, 200, "the visible part starts at the clip edge");
        assert_eq!(placed.height, 260, "only the visible part is sized");
    }

    #[test]
    fn a_fully_clipped_surface_is_hidden_not_presented_offscreen() {
        let placed = placement_physical(
            (100.0, 100.0, 640.0, 360.0),
            (0.0, 0.0),
            1.0,
            (0.0, 2000.0, 4000.0, 2000.0),
        );
        assert_eq!(placed, None, "no overlap means no surface");
    }

    #[test]
    fn fractional_logical_edges_round_outward_never_inward() {
        let placed = placement_physical(
            (10.3, 10.3, 100.4, 100.4),
            (0.0, 0.0),
            1.0,
            (0.0, 0.0, 4000.0, 4000.0),
        )
        .expect("visible");
        // floor/ceil outward rounding: no presented pixel missing.
        assert_eq!(placed.x, 10, "floor on the origin");
        assert!(
            placed.width >= 101,
            "ceil on the far edge: got {}",
            placed.width
        );
    }
}
