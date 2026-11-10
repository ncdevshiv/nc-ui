//! Where a frame is drawn: a window surface, or an offscreen target.
//!
//! The two are the same thing with different destinations, and keeping them in one type
//! is what lets the same render pass serve both. An offscreen target is what makes the
//! renderer testable: a test can draw a frame and read the pixels back, with no window
//! station and no display.
//!
//! The surface is a *surface* and not a texture because a window's pixels are presented
//! by the driver, not by the application. What the application owns is the surface's
//! configuration — its size, its scale factor, and whether it is currently able to be
//! drawn into.

use core::fmt;

/// The size of a drawing target, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceSize {
    /// The width, in physical pixels.
    pub width: u32,
    /// The height, in physical pixels.
    pub height: u32,
}

impl SurfaceSize {
    /// A size.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Whether the size can hold a pixel at all.
    #[must_use]
    pub const fn is_drawable(&self) -> bool {
        self.width > 0 && self.height > 0
    }

    /// The number of bytes one frame of this size occupies at four bytes per pixel.
    #[must_use]
    pub const fn byte_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

/// Where a surface is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    /// A window the user can see.
    Window,
    /// A texture the application reads back.
    Offscreen,
}

impl fmt::Display for SurfaceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Window => f.write_str("window"),
            Self::Offscreen => f.write_str("offscreen"),
        }
    }
}

/// A drawing target.
///
/// The policy half of a surface: what kind it is, how big it is, and whether it is
/// currently able to be drawn into. The `wgpu` half — the actual `Surface` or `Texture` —
/// lives in the `wgpu-device` build and is reached through the same fields, so the
/// renderer does not branch on which kind of surface it has.
#[derive(Debug, Clone, PartialEq)]
pub struct Surface {
    kind: SurfaceKind,
    size: SurfaceSize,
    /// The scale factor between logical and physical pixels.
    scale_factor: f32,
    /// Whether the surface can be drawn into right now.
    ///
    /// A window that is occluded, minimised, or between frames is not drawable, and a
    /// renderer that drew into it anyway would either fail in the driver or present a
    /// stale frame as if it were current.
    drawable: bool,
}

impl Surface {
    /// A surface of the given kind and size.
    #[must_use]
    pub fn new(kind: SurfaceKind, size: SurfaceSize, scale_factor: f32) -> Self {
        Self {
            kind,
            size,
            scale_factor: if scale_factor > 0.0 {
                scale_factor
            } else {
                1.0
            },
            drawable: size.is_drawable(),
        }
    }

    /// What kind of surface this is.
    #[must_use]
    pub const fn kind(&self) -> SurfaceKind {
        self.kind
    }

    /// The surface's size, in physical pixels.
    #[must_use]
    pub const fn size(&self) -> SurfaceSize {
        self.size
    }

    /// The scale factor between logical and physical pixels.
    #[must_use]
    pub fn scale_factor(&self) -> f32 {
        self.scale_factor
    }

    /// Whether the surface can be drawn into.
    #[must_use]
    pub const fn is_drawable(&self) -> bool {
        self.drawable
    }

    /// Resizes the surface.
    ///
    /// A resize to a size that cannot hold a pixel leaves the surface not drawable, which
    /// is the honest answer: a zero-sized window is not a window that can be drawn into.
    pub fn resize(&mut self, size: SurfaceSize) {
        self.size = size;
        self.drawable = size.is_drawable();
    }

    /// Notes that the surface cannot currently be drawn into.
    pub fn mark_unavailable(&mut self) {
        self.drawable = false;
    }

    /// Notes that the surface can be drawn into again.
    pub fn mark_available(&mut self) {
        self.drawable = self.size.is_drawable();
    }
}

#[cfg(feature = "wgpu-device")]
pub mod wgpu_target {
    use super::SurfaceSize;

    /// A texture the renderer draws into and the application reads back.
    ///
    /// This is what makes the renderer verifiable: a test can draw a frame and read the
    /// pixels, with no window station, no display, and no compositor in the way. What is
    /// asserted is the *output*, not the absence of a crash.
    #[derive(Debug)]
    pub struct OffscreenTarget {
        texture: wgpu::Texture,
        view: wgpu::TextureView,
        buffer: wgpu::Buffer,
        size: SurfaceSize,
        /// The number of bytes one row occupies in the readback buffer.
        ///
        /// A readback row is padded to a 256-byte boundary, because that is what a
        /// texture-to-buffer copy requires. The padding is not part of the image, so it
        /// is stripped when the pixels are read.
        padded_row_bytes: u64,
    }

    impl OffscreenTarget {
        /// Creates a target of the given size.
        #[must_use]
        pub fn new(device: &wgpu::Device, size: SurfaceSize) -> Self {
            let width = size.width.max(1);
            let height = size.height.max(1);
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ncui-gpu-renderer-target"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bytes_per_row = width as usize * 4;
            let padded_row_bytes = bytes_per_row.div_ceil(256) * 256;
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ncui-gpu-renderer-readback"),
                size: (padded_row_bytes * height as usize) as u64,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            Self {
                texture,
                view,
                buffer,
                size,
                padded_row_bytes: padded_row_bytes as u64,
            }
        }

        /// The target's texture view, to draw into.
        #[must_use]
        pub fn view(&self) -> &wgpu::TextureView {
            &self.view
        }

        /// The target's size.
        #[must_use]
        pub const fn size(&self) -> SurfaceSize {
            self.size
        }

        /// Reads the target's pixels back.
        ///
        /// Returns the pixels as red, green, blue and alpha bytes, row by row from the
        /// top. The copy is staged through a mapped buffer, so the result is the frame
        /// that was actually drawn rather than the frame that was submitted.
        ///
        /// # Errors
        ///
        /// Returns [`ReadbackError`] when the copy could not be completed or the buffer
        /// could not be mapped. A readback that fails is reported rather than turned into
        /// an empty frame, because an empty frame is indistinguishable from a black one.
        pub fn read_back(
            &self,
            device: &wgpu::Device,
            queue: &wgpu::Queue,
        ) -> Result<Vec<u8>, ReadbackError> {
            let width = self.size.width.max(1);
            let height = self.size.height.max(1);
            let bytes_per_row = width as usize * 4;
            let padded = self.padded_row_bytes as usize;
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ncui-gpu-renderer-readback-encoder"),
            });
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &self.buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded as u32),
                        rows_per_image: Some(height),
                    },
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            queue.submit(Some(encoder.finish()));
            let slice = self.buffer.slice(..);
            let (sender, receiver) = std::sync::mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .map_err(|error| ReadbackError::PollFailed(error.to_string()))?;
            receiver
                .recv()
                .map_err(|_| ReadbackError::NoResult)?
                .map_err(|error| ReadbackError::MapFailed(error.to_string()))?;
            let mapped = slice.get_mapped_range();
            let mut pixels = Vec::with_capacity(bytes_per_row * height as usize);
            for row in 0..height as usize {
                let start = row * padded;
                pixels.extend_from_slice(&mapped[start..start + bytes_per_row]);
            }
            drop(mapped);
            self.buffer.unmap();
            Ok(pixels)
        }
    }

    /// Why a readback failed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ReadbackError {
        /// The GPU did not finish the copy.
        PollFailed(String),
        /// The buffer could not be mapped.
        MapFailed(String),
        /// The readback reported no result at all.
        NoResult,
    }

    impl core::fmt::Display for ReadbackError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Self::PollFailed(reason) => write!(f, "the readback did not complete: {reason}"),
                Self::MapFailed(reason) => {
                    write!(f, "the readback buffer could not be mapped: {reason}")
                }
                Self::NoResult => f.write_str("the readback reported no result"),
            }
        }
    }

    /// An error presenting a frame to a window surface.
    #[derive(Debug)]
    pub enum PresentError {
        /// The surface could not be configured for the requested size.
        Configure(String),
        /// The surface had no current texture to draw into.
        NoCurrentTexture,
        /// Presenting the frame failed.
        Present(String),
    }

    impl core::fmt::Display for PresentError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Self::Configure(reason) => {
                    write!(f, "the surface could not be configured: {reason}")
                }
                Self::NoCurrentTexture => f.write_str("the surface had no current texture"),
                Self::Present(reason) => write!(f, "presenting the frame failed: {reason}"),
            }
        }
    }

    impl std::error::Error for PresentError {}

    /// A window surface the renderer draws into and the compositor presents.
    ///
    /// This is the *window* half of the composition boundary ADR-002 requires: a real
    /// surface backed by a real window, rather than the offscreen texture the tests use.
    /// The surface itself is created by the caller — the adapter that owns the window —
    /// because creating it needs a window handle and the workspace forbids the `unsafe`
    /// that reaching for one directly would require. What this type owns is the
    /// configuration and the draw: size, format, and the render pass that paints a
    /// [`crate::render::RenderPlan`] into it.
    #[derive(Debug)]
    pub struct WindowSurface {
        size: SurfaceSize,
        format: wgpu::TextureFormat,
        configured: bool,
    }

    impl WindowSurface {
        /// A surface of the given size and format, not yet configured.
        #[must_use]
        pub fn new(size: SurfaceSize, format: wgpu::TextureFormat) -> Self {
            Self {
                size,
                format,
                configured: false,
            }
        }

        /// The surface's size.
        #[must_use]
        pub const fn size(&self) -> SurfaceSize {
            self.size
        }

        /// The surface's format.
        #[must_use]
        pub const fn format(&self) -> wgpu::TextureFormat {
            self.format
        }

        /// Whether the surface has been configured and can be drawn into.
        #[must_use]
        pub const fn is_configured(&self) -> bool {
            self.configured
        }

        /// Configures the surface for drawing at the current size.
        ///
        /// # Errors
        ///
        /// Returns [`PresentError::Configure`] when the surface rejects the configuration.
        pub fn configure(
            &mut self,
            device: &wgpu::Device,
            surface: &wgpu::Surface<'_>,
        ) -> Result<(), PresentError> {
            if !self.size.is_drawable() {
                return Err(PresentError::Configure(
                    "the surface has no drawable area".to_owned(),
                ));
            }
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: self.format,
                width: self.size.width,
                height: self.size.height,
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode: wgpu::CompositeAlphaMode::Auto,
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            };
            surface.configure(device, &config);
            self.configured = true;
            Ok(())
        }

        /// Draws a plan into the surface and presents it.
        ///
        /// # Errors
        ///
        /// Returns [`PresentError::NoCurrentTexture`] when the surface has no frame to draw
        /// into, and [`PresentError::Present`] when presenting fails.
        pub fn render(
            &self,
            device: &wgpu::Device,
            queue: &wgpu::Queue,
            surface: &wgpu::Surface<'_>,
            pipeline: &crate::render::wgpu_pass::QuadPipeline,
            plan: &crate::render::RenderPlan,
        ) -> Result<(), PresentError> {
            if !self.configured {
                return Err(PresentError::Configure(
                    "the surface has not been configured".to_owned(),
                ));
            }
            let frame = match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(texture)
                | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
                other => {
                    return Err(PresentError::Present(format!(
                        "the surface produced no drawable texture: {other:?}"
                    )));
                }
            };
            let view = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            pipeline.draw(device, queue, &view, plan);
            frame.present();
            Ok(())
        }

        /// Records a resize: the surface must be reconfigured before it is drawn into again.
        pub fn resize(&mut self, size: SurfaceSize) {
            self.size = size;
            self.configured = false;
        }
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_surface_with_no_area_is_not_drawable() {
        let surface = Surface::new(SurfaceKind::Window, SurfaceSize::new(0, 100), 1.0);
        assert!(!surface.is_drawable());
        let surface = Surface::new(SurfaceKind::Window, SurfaceSize::new(100, 0), 1.0);
        assert!(!surface.is_drawable());
    }

    #[test]
    fn a_surface_with_area_is_drawable() {
        let surface = Surface::new(SurfaceKind::Window, SurfaceSize::new(100, 50), 1.0);
        assert!(surface.is_drawable());
    }

    #[test]
    fn a_non_positive_scale_factor_is_treated_as_one() {
        // A scale factor of zero would make every logical pixel vanish, and a negative one
        // would mirror the surface. Neither is a thing a caller can mean.
        let surface = Surface::new(SurfaceKind::Window, SurfaceSize::new(10, 10), 0.0);
        assert!((surface.scale_factor() - 1.0).abs() < f32::EPSILON);
        let surface = Surface::new(SurfaceKind::Window, SurfaceSize::new(10, 10), -2.0);
        assert!((surface.scale_factor() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn resizing_to_nothing_makes_the_surface_not_drawable() {
        let mut surface = Surface::new(SurfaceKind::Window, SurfaceSize::new(100, 50), 1.0);
        assert!(surface.is_drawable());
        surface.resize(SurfaceSize::new(0, 0));
        assert!(!surface.is_drawable());
    }

    #[test]
    fn an_unavailable_surface_becomes_drawable_again_when_it_is_available() {
        let mut surface = Surface::new(SurfaceKind::Window, SurfaceSize::new(100, 50), 1.0);
        surface.mark_unavailable();
        assert!(!surface.is_drawable());
        surface.mark_available();
        assert!(surface.is_drawable());
    }

    #[test]
    fn a_surface_reports_its_kind() {
        let surface = Surface::new(SurfaceKind::Offscreen, SurfaceSize::new(4, 4), 1.0);
        assert_eq!(surface.kind().to_string(), "offscreen");
        let surface = Surface::new(SurfaceKind::Window, SurfaceSize::new(4, 4), 1.0);
        assert_eq!(surface.kind().to_string(), "window");
    }

    #[test]
    fn a_size_reports_the_bytes_a_frame_of_it_would_occupy() {
        assert_eq!(SurfaceSize::new(4, 4).byte_len(), 64);
    }
}
