//! An island's frame as the pixels a page can composite.
//!
//! ADR-002's composition boundary has two halves, and they are built in different places. The
//! renderer's half is that a region is drawn on the GPU; the page's half is that the result reaches
//! the DOM, which is what makes it an island *in a page* rather than a picture beside one. This
//! module is the join: it draws a frame into an offscreen target, reads the pixels back, and hands
//! them over as a value that carries the size they were drawn at. A page takes those bytes, wraps
//! them in an `ImageData`, and draws them into a `canvas` — and from there the browser composites
//! the island with everything around it: z-order, clipping, scrolling and opacity become the DOM's
//! business rather than something this renderer has to reimplement.
//!
//! # What this is not
//!
//! It is not a zero-copy path. The pixels cross from the GPU to the CPU and then to the page, so a
//! 640×480 island costs a megabyte and a bit per frame. `wry`'s public API has no shared-texture or
//! child-surface seam, so a zero-copy path is not expressible through the port this runtime draws
//! through; `docs/known-gaps.md` records that, and the readback is the honest alternative rather
//! than a stand-in for it. What it does deliver is the part that was missing: the pixels are
//! produced on the GPU, and they end up composited by the DOM.
//!
//! # Why the clear colour is transparent
//!
//! An island is a region *of* a page, so the pixels outside it belong to the page. Clearing to
//! transparent is what lets the page show through the parts of the frame the island does not cover,
//! which is the difference between an island and a rectangle pasted over the page.

#[cfg(feature = "wgpu-device")]
pub use wgpu_join::{IslandError, IslandPixels, MAX_ISLAND_PIXELS, render_to_pixels};

#[cfg(feature = "wgpu-device")]
mod wgpu_join {
    use core::fmt;

    use crate::device::WgpuDevice;
    use crate::frame::Frame;
    use crate::render::{RenderError, RenderPlan, wgpu_pass::QuadPipeline};
    use crate::surface::SurfaceSize;
    use crate::surface::wgpu_target::{OffscreenTarget, ReadbackError};

    /// The largest island this will render, in pixels.
    ///
    /// A bound because the frame crosses to the page as bytes: an island of a hundred million pixels
    /// is four hundred megabytes of transfer per frame, which is not a size a page can composite at
    /// any rate a person would call animation. The bound is generous for a region of a window and
    /// refuses the sizes that are certainly a mistake.
    pub const MAX_ISLAND_PIXELS: u32 = 4_194_304;

    /// An island's pixels, with the size they were drawn at.
    ///
    /// The size is part of the value rather than something the caller remembers: pixels without
    /// their dimensions cannot be put in a canvas, and a page that guessed the width would draw a
    /// skewed island.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct IslandPixels {
        /// The width, in physical pixels.
        pub width: u32,
        /// The height, in physical pixels.
        pub height: u32,
        /// The pixels: red, green, blue and alpha bytes, row by row from the top.
        pub rgba: Vec<u8>,
    }

    impl IslandPixels {
        /// What these bytes are, for a transport that has to label them.
        pub const CONTENT_TYPE: &'static str = "application/octet-stream";

        /// Whether the pixels are the size they say they are.
        ///
        /// Checked by whoever is about to hand them to a page, because a short frame is a frame the
        /// browser would refuse or draw wrongly, and refusing it here names the reason.
        #[must_use]
        pub fn is_whole(&self) -> bool {
            self.rgba.len() == SurfaceSize::new(self.width, self.height).byte_len()
        }
    }

    /// Why an island's frame could not be turned into pixels.
    #[derive(Debug)]
    pub enum IslandError {
        /// The size is not one this will render.
        Size {
            /// The width that was asked for.
            width: u32,
            /// The height that was asked for.
            height: u32,
        },
        /// The frame is not one the renderer can draw.
        Plan(RenderError),
        /// The pixels could not be read back.
        Readback(ReadbackError),
    }

    impl fmt::Display for IslandError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Size { width, height } => write!(
                    f,
                    "an island of {width}x{height} is not one this renders: each side must be \
                     positive and the area at most {MAX_ISLAND_PIXELS} pixels"
                ),
                Self::Plan(error) => write!(f, "{error}"),
                Self::Readback(error) => write!(f, "{error}"),
            }
        }
    }

    impl std::error::Error for IslandError {}

    /// Draws `frame` at `size` and reads the pixels back.
    ///
    /// The device is borrowed rather than created here: a device is expensive to bring up and is
    /// what makes a sequence of frames cheap, so an island renderer that made one per frame would be
    /// paying the whole cost of the GPU for each one.
    ///
    /// # Errors
    ///
    /// Returns [`IslandError::Size`] when the size is not renderable, [`IslandError::Plan`] when the
    /// frame holds a rectangle that is not one, and [`IslandError::Readback`] when the pixels could
    /// not be read back.
    pub fn render_to_pixels(
        device: &WgpuDevice,
        size: SurfaceSize,
        frame: &Frame,
    ) -> Result<IslandPixels, IslandError> {
        let area = u64::from(size.width) * u64::from(size.height);
        if !size.is_drawable() || area > u64::from(MAX_ISLAND_PIXELS) {
            return Err(IslandError::Size {
                width: size.width,
                height: size.height,
            });
        }
        let plan = RenderPlan::from_frame(frame, size).map_err(IslandError::Plan)?;
        let target = OffscreenTarget::new(device.wgpu_device(), size);
        let pipeline = QuadPipeline::new(device.wgpu_device(), wgpu::TextureFormat::Rgba8Unorm);
        pipeline.draw(device.wgpu_device(), device.queue(), target.view(), &plan);
        let rgba = target
            .read_back(device.wgpu_device(), device.queue())
            .map_err(IslandError::Readback)?;
        Ok(IslandPixels {
            width: size.width,
            height: size.height,
            rgba,
        })
    }
}
