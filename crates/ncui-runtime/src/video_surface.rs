//! The runtime's video surface registry: the wiring between the page's placement
//! truth, the producer's frames, and the platform presenter.
//!
//! The architecture this implements, in one paragraph: the page owns geometry —
//! only the document knows where the video element sits, how far it is scrolled
//! and what the device-pixel ratio is — so the page reports placement in
//! document-logical terms and the registry converts once, here, into the physical
//! placement the presenter speaks. The producer owns time — frames arrive when
//! the engine has them, never on a clock the registry invented — so `push_frame`
//! presents synchronously on the producer's own call. The registry owns
//! everything in between: the frame ring's drop policy, the control intents
//! travelling back to the producer, and the lifecycle that ties every surface to
//! its window.
//!
//! What this module deliberately does not do is decode. Pixels arrive as
//! [`RgbaPixels`] — a CPU transport — because the first honest transport is the
//! one that works everywhere; the zero-copy texture path is an optimization the
//! producer seam already anticipates through the ring's generic payload.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use ncui_core::WindowId;
use ncui_gpu::surface::{
    ControlIntent, Frame, PlacementRect, Presenter, PushOutcome, Surface, SurfaceSpec,
    placement_physical,
};
use ncui_video_surface::{RgbaPixels, Win32Presenter};

/// One surface the runtime is wiring: the host-agnostic model, the platform
/// presenter, and the control intents the producer has not yet drained.
struct VideoSurfaceEntry {
    /// The ring and playback truth — ordering, dropping, control state.
    surface: Surface<RgbaPixels>,
    /// The platform presenter, live whenever the surface is placed inside its clip.
    presenter: Win32Presenter,
    /// Control intents not yet drained by the producer, in arrival order.
    pending_control: Vec<ControlIntent>,
}

impl core::fmt::Debug for VideoSurfaceEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VideoSurfaceEntry")
            .field("surface", &self.surface)
            .field("pending_control", &self.pending_control.len())
            .finish_non_exhaustive()
    }
}

/// Why a video surface operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoSurfaceError {
    /// The window named is not open.
    NoSuchWindow(WindowId),
    /// The surface named does not exist on the window.
    NoSuchSurface(WindowId, u64),
    /// The window's backend has no native handle a presenter can child into.
    NoNativeHandle(WindowId),
    /// The platform presenter refused creation or placement.
    Presenter(String),
    /// The surface's own model refused — a teardown re-use, a malformed spec.
    Model(String),
}

impl core::fmt::Display for VideoSurfaceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSuchWindow(window) => write!(f, "window {window} is not open"),
            Self::NoSuchSurface(window, surface) => {
                write!(f, "window {window} has no video surface {surface}")
            }
            Self::NoNativeHandle(window) => write!(
                f,
                "window {window}'s engine has no native handle a video surface can child into"
            ),
            Self::Presenter(reason) => write!(f, "the presenter refused: {reason}"),
            Self::Model(reason) => write!(f, "the surface model refused: {reason}"),
        }
    }
}

/// The registry itself: surfaces keyed by window and id, with the id supply.
///
/// Behind one mutex because no operation here is hot enough to justify finer
/// grain — placement updates are user-gesture rate and frame pushes are bounded
/// by the ring — and because presenter operations must never interleave with
/// their own teardown, which only a single exclusion honestly expresses.
#[derive(Debug, Default)]
pub(crate) struct VideoSurfaces {
    entries: Mutex<HashMap<(WindowId, u64), VideoSurfaceEntry>>,
    next_id: AtomicU64,
}

/// The producer's own handle onto the registry — cloneable, shareable, and
/// independent of the runtime's lock.
///
/// Frames are the hottest path in the system: a 1080p30 stream pushes thirty
/// times a second, and every push presents synchronously inside it. If that
/// path had to win the same exclusion the page pump holds, frame cost would
/// include pump time — measured in the acceptance harness as a p99 double the
/// frame budget, the classic convoy signature. The registry synchronizes
/// itself; this handle is how a producer reaches it directly, and the only
/// exclusion a frame ever waits on is the one that serializes presentation
/// against teardown — a correctness requirement, not contention.
#[derive(Debug)]
pub struct VideoSurfaceProducer {
    registry: std::sync::Arc<VideoSurfaces>,
}

impl VideoSurfaceProducer {
    pub(crate) fn new(registry: std::sync::Arc<VideoSurfaces>) -> Self {
        Self { registry }
    }

    /// Pushes one frame: the ring keeps it newest-first and the presenter shows
    /// it inside the call, because the producer owns time.
    ///
    /// # Errors
    ///
    /// The registry's typed refusal when the window or surface does not exist,
    /// the surface is torn down, or the presenter refuses the frame.
    #[cfg(target_os = "windows")]
    pub fn push_frame(
        &self,
        window: WindowId,
        surface: u64,
        frame: Frame<RgbaPixels>,
    ) -> Result<PushOutcome, VideoSurfaceError> {
        self.registry.push_frame(window, surface, frame)
    }

    /// Drains the playback control intents not yet seen — pull-shaped because
    /// the decode loop's cadence is the engine's own.
    #[cfg(target_os = "windows")]
    #[must_use]
    pub fn drain_control(&self, window: WindowId, surface: u64) -> Vec<ControlIntent> {
        self.registry.drain_control(window, surface)
    }
}

/// The placement a page reports, in the document's own logical terms.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PagePlacement {
    /// The surface element's document rect: x, y, width, height in CSS pixels.
    pub rect: (f32, f32, f32, f32),
    /// The document's scroll offset in CSS pixels.
    pub scroll: (f32, f32),
    /// The window's device-pixel ratio as the page measures it.
    pub device_pixel_ratio: f32,
    /// The visible clip region in document space — the viewport or a scroller.
    pub clip: (f32, f32, f32, f32),
}

impl VideoSurfaces {
    /// Creates a surface on `window` and its presenter, returning the id the page
    /// will name it by.
    ///
    /// The presenter is created even before the first placement: a surface that
    /// exists but cannot present is a half-truth the first frame would discover
    /// too late. The initial placement is applied immediately for the same reason.
    #[cfg(target_os = "windows")]
    pub(crate) fn create(
        &self,
        window: WindowId,
        native_handle: Option<isize>,
        spec: SurfaceSpec,
        placement: PagePlacement,
    ) -> Result<u64, VideoSurfaceError> {
        // Depth three, deliberately shallow: a preview monitor never wants the
        // past, and ring depth past the pipeline's own latency is queued latency,
        // not smoothness.
        const RING_DEPTH: usize = 3;
        let handle = native_handle.ok_or(VideoSurfaceError::NoNativeHandle(window))?;
        let physical = placement_physical(
            placement.rect,
            placement.scroll,
            placement.device_pixel_ratio,
            placement.clip,
        );
        // A surface created fully clipped starts hidden at a minimal placement
        // rather than failing to exist — visibility follows the clip as it moves.
        let initial = physical.unwrap_or(PlacementRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        });
        let spec_validated = spec
            .validated()
            .map_err(|error| VideoSurfaceError::Model(error.to_string()))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let presenter = Win32Presenter::create(
            handle as *mut core::ffi::c_void,
            initial,
            spec_validated.width,
            spec_validated.height,
        )
        .map_err(|error| VideoSurfaceError::Presenter(error.to_string()))?;
        if physical.is_none() {
            presenter.set_visible(false);
        }
        let surface = Surface::new(ncui_gpu::surface::SurfaceId(id), spec_validated, RING_DEPTH)
            .map_err(|error| VideoSurfaceError::Model(error.to_string()))?;
        let entry = VideoSurfaceEntry {
            surface,
            presenter,
            pending_control: Vec::new(),
        };
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((window, id), entry);
        Ok(id)
    }

    /// Applies a page-reported placement: convert once, place the presenter, and
    /// hide or show as the clip dictates.
    #[cfg(target_os = "windows")]
    pub(crate) fn place(
        &self,
        window: WindowId,
        surface: u64,
        placement: PagePlacement,
    ) -> Result<(), VideoSurfaceError> {
        let physical = placement_physical(
            placement.rect,
            placement.scroll,
            placement.device_pixel_ratio,
            placement.clip,
        );
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries
            .get_mut(&(window, surface))
            .ok_or(VideoSurfaceError::NoSuchSurface(window, surface))?;
        match physical {
            Some(rect) => {
                entry
                    .presenter
                    .place(rect)
                    .map_err(|error| VideoSurfaceError::Presenter(error.to_string()))?;
                entry.presenter.set_visible(true);
            }
            None => entry.presenter.set_visible(false),
        }
        Ok(())
    }

    /// Records a control intent in the model and queues it for the producer.
    ///
    /// Both halves matter: the model's playback state is the truth a seek's frame
    /// accuracy is measured against, and the queue is how the producer learns —
    /// a control the producer never sees is a gesture swallowed.
    #[cfg(target_os = "windows")]
    pub(crate) fn control(
        &self,
        window: WindowId,
        surface: u64,
        intent: ControlIntent,
    ) -> Result<(), VideoSurfaceError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries
            .get_mut(&(window, surface))
            .ok_or(VideoSurfaceError::NoSuchSurface(window, surface))?;
        entry
            .surface
            .control(intent)
            .map_err(|error| VideoSurfaceError::Model(error.to_string()))?;
        entry.pending_control.push(intent);
        Ok(())
    }

    /// Drains the control intents a producer has not yet seen.
    ///
    /// Pull-shaped deliberately: a producer drains when its own loop is ready —
    /// the decode thread's cadence is the engine's business, and a push-shaped
    /// callback would be the runtime reaching into a thread it does not own.
    #[cfg(target_os = "windows")]
    pub(crate) fn drain_control(&self, window: WindowId, surface: u64) -> Vec<ControlIntent> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries
            .get_mut(&(window, surface))
            .map_or_else(Vec::new, |entry| std::mem::take(&mut entry.pending_control))
    }

    /// Pushes a producer frame into the ring and presents the newest truth.
    ///
    /// Presentation happens here, on the producer's call, because the producer
    /// owns time: a frame pushed is a frame ready, and the ring's drop-oldest
    /// policy — not a presentation clock — decides what the producer's rate
    /// means. The answer says what the ring did, so the producer can slow down
    /// when it outruns the display.
    #[cfg(target_os = "windows")]
    pub(crate) fn push_frame(
        &self,
        window: WindowId,
        surface: u64,
        frame: Frame<RgbaPixels>,
    ) -> Result<PushOutcome, VideoSurfaceError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries
            .get_mut(&(window, surface))
            .ok_or(VideoSurfaceError::NoSuchSurface(window, surface))?;
        let outcome = entry
            .surface
            .push(frame)
            .map_err(|error| VideoSurfaceError::Model(error.to_string()))?;
        if let Some(newest) = entry.surface.ring().drain_to_newest() {
            entry
                .presenter
                .present(&newest)
                .map_err(|error| VideoSurfaceError::Presenter(error.to_string()))?;
        }
        Ok(outcome)
    }

    /// Tears one surface down: the presenter dies with its window child, and the
    /// ring's accounting closes.
    #[cfg(target_os = "windows")]
    pub(crate) fn teardown(&self, window: WindowId, surface: u64) -> Result<(), VideoSurfaceError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut entry = entries
            .remove(&(window, surface))
            .ok_or(VideoSurfaceError::NoSuchSurface(window, surface))?;
        entry.surface.teardown();
        entry.presenter.teardown();
        Ok(())
    }

    /// Tears down every surface a window owns — the window-close path, where no
    /// surface may outlive the window its presenter is childed into.
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
            if let Some(mut entry) = entries.remove(&key) {
                entry.surface.teardown();
                entry.presenter.teardown();
            }
        }
    }

    /// How many surfaces a window currently owns — the introspection answer.
    #[cfg(target_os = "windows")]
    #[allow(
        dead_code,
        reason = "the count answers the inspection question tests and diagnostics ask; kept for the seam surface it belongs to"
    )]
    pub(crate) fn count(&self, window: WindowId) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .filter(|(owner, _)| *owner == window)
            .count()
    }

    /// A surface's playback and ring truth, for the introspection surface.
    #[cfg(target_os = "windows")]
    pub(crate) fn inspect(
        &self,
        window: WindowId,
        surface: u64,
    ) -> Option<(u64, usize, Option<u64>)> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.get(&(window, surface)).map(|entry| {
            (
                entry.surface.dropped_frames(),
                entry.surface.queued_frames(),
                entry
                    .presenter
                    .last_presented()
                    .map(|(sequence, _)| sequence),
            )
        })
    }
}
