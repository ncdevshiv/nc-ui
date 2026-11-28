//! The Windows presenter: child `HWND` creation and the `wgpu` blit pipeline.
//!
//! The unsafe here is the reason this crate exists: window creation, placement,
//! destruction and the window procedure are Win32 FFI with no safe wrapper in
//! the dependency tree (tao creates only top-level windows). Every unsafe block
//! carries its invariant at the point of use; everything else in the crate,
//! the GPU stack included, is safe Rust.
#![allow(
    unsafe_code,
    reason = "raw child-HWND creation is impossible without FFI — tao creates only top-level windows; every block carries its local invariant"
)]
#![allow(
    clippy::cast_possible_wrap,
    reason = "Win32 coordinates are i32 and Rust geometry is u32; the casts are the boundary translation itself, bounded because surface dimensions are validated far below i32::MAX at construction"
)]
#![allow(
    clippy::not_unsafe_ptr_arg_deref,
    reason = "the raw handles these functions accept are Win32 HWNDs — the OS validates them on every call and merely fails dead ones, so no memory-safety dereference exists to mark"
)]

use std::future::Future;
use std::num::NonZeroIsize;
use std::sync::OnceLock;

use ncui_gpu::surface::{Frame, PlacementRect, Presenter};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, Win32WindowHandle, WindowHandle,
};
use windows_sys::Win32::Foundation::{HINSTANCE, HWND};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_TOP, RegisterClassW, SW_HIDE, SW_SHOW,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, ShowWindow, WNDCLASSW, WS_CHILD,
    WS_CLIPSIBLINGS, WS_EX_NOPARENTNOTIFY, WS_EX_TRANSPARENT, WS_VISIBLE,
};

/// The window class name every video surface shares — `ncui-video-surface` as
/// UTF-16 with its terminator, written out so no wide-string macro crate is
/// needed for one constant.
const CLASS_NAME: &[u16] = &[
    0x006e, 0x0063, 0x0075, 0x0069, 0x002d, 0x0076, 0x0069, 0x0064, 0x0065, 0x006f, 0x002d, 0x0073,
    0x0075, 0x0072, 0x0066, 0x0061, 0x0063, 0x0065, 0x0000,
];

/// The blit shader: a fullscreen triangle sampling the frame texture.
///
/// No vertex buffers — positions come from the vertex index, because a triangle
/// covering the screen is three constants and a buffer of them is ceremony. UVs
/// derive from clip space, so the frame fills the window at any size: stretching
/// to the placement is the contract, and aspect correctness is the placement
/// machinery's burden, not the shader's.
const BLIT_SHADER: &str = r"
@group(0) @binding(0) var frame_texture: texture_2d<f32>;
@group(0) @binding(1) var frame_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let pos = positions[index];
    var out: VertexOutput;
    out.position = vec4<f32>(pos, 0.0, 1.0);
    out.uv = vec2<f32>((pos.x + 1.0) * 0.5, (1.0 - pos.y) * 0.5);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(frame_texture, frame_sampler, in.uv);
}
";

/// The failures the presenter reports.
#[derive(Debug, Clone)]
pub enum PresentError {
    /// Win32 refused window creation or placement.
    Os {
        /// What failed, in words.
        reason: String,
    },
    /// The GPU stack refused — no adapter, device loss, surface failure.
    Gpu {
        /// What failed, in words.
        reason: String,
    },
    /// The frame's payload does not match the surface the presenter was built for.
    PayloadMismatch {
        /// What mismatched.
        reason: String,
    },
}

impl core::fmt::Display for PresentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Os { reason } => write!(f, "os: {reason}"),
            Self::Gpu { reason } => write!(f, "gpu: {reason}"),
            Self::PayloadMismatch { reason } => write!(f, "payload: {reason}"),
        }
    }
}

impl std::error::Error for PresentError {}

/// The frame payload the presenter carries: CPU-side RGBA bytes.
///
/// CPU upload is the substrate's honest starting point — the producer hands
/// bytes, the presenter uploads. The zero-copy GPU path (a shared texture the
/// engine composites into directly) optimises the *upload*, not the transport:
/// the transport already copies nothing through the browser, which was the cost
/// that mattered.
impl core::fmt::Debug for RgbaPixels {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RgbaPixels")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

pub struct RgbaPixels {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row stride in bytes — at least `width * 4`.
    pub stride: u32,
    /// The pixel bytes, RGBA8, row-major.
    pub bytes: Vec<u8>,
}

/// The registered class, once per process. `OnceLock` rather than a `static mut`:
/// the registration is exactly-once global state and the type system should say so.
/// The handle is stored as the integer it is: an `HINSTANCE` is the module's
/// base address, process-global and immutable for the process's life — the
/// pointer-ness is Win32's type, not a sharing hazard, and the integer makes
/// that truth representable without an unsafe `Send` impl.
static CLASS: OnceLock<Result<usize, PresentError>> = OnceLock::new();

fn ensure_class() -> Result<HINSTANCE, PresentError> {
    fn register() -> Result<usize, PresentError> {
        // SAFETY: GetModuleHandleW and RegisterClassW are plain Win32 calls with
        // no aliasing requirements; the class struct is fully initialised and
        // outlives the call. The window procedure is a valid `extern "system"`
        // function of the right shape.
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null()) as HINSTANCE;
            if instance.is_null() {
                return Err(PresentError::Os {
                    reason: "GetModuleHandleW answered null".to_owned(),
                });
            }
            let class = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(video_wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: CLASS_NAME.as_ptr(),
            };
            if RegisterClassW(&raw const class) == 0 {
                return Err(PresentError::Os {
                    reason: "RegisterClassW refused the video surface class".to_owned(),
                });
            }
            Ok(instance as usize)
        }
    }
    match CLASS.get_or_init(register) {
        Ok(address) =>
        {
            #[expect(
                clippy::cast_possible_wrap,
                reason = "the module base address fits in isize on every supported Windows architecture"
            )]
            Ok(*address as isize as HINSTANCE)
        }
        Err(error) => Err(error.clone()),
    }
}

/// The surface's window procedure: everything defaults.
///
/// The window is display-only — `WS_EX_TRANSPARENT` keeps input out, and the
/// continuous present means `WM_PAINT` starvation cannot blank it. What the
/// procedure must not do is interesting work: interesting work on a window
/// thread is how the CDP-on-the-event-loop defect happens again one layer down.
///
/// # Safety
///
/// Win32 requires exactly this shape; the body forwards everything to the
/// default procedure, which is correct for every message this window can receive.
unsafe extern "system" fn video_wndproc(
    window: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    // SAFETY: the handle is the window this procedure is attached to, valid by
    // the OS for the duration of the call.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn block_on<F: Future>(future: F) -> F::Output {
    use std::task::{Context, Poll};
    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// The window-handle wrapper `wgpu`'s surface path consumes.
///
/// The wrapper owns the raw handle value; the validity obligation — the window
/// must outlive any surface created from the handle — is met by the owning
/// `Win32Presenter`'s field order: the window is declared first and destroyed
/// last, after every GPU object built against it.
struct WindowHandles {
    raw: Win32WindowHandle,
}

impl HasWindowHandle for WindowHandles {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // SAFETY: the handle names a live window — the owning presenter destroys
        // it only after every consumer of this handle is dropped, an invariant
        // Rust's field-order drop enforces rather than hopes for.
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(self.raw)) })
    }
}

impl HasDisplayHandle for WindowHandles {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        // SAFETY: on Windows the display handle is a null constant, valid by
        // construction — there is no display connection to outlive.
        Ok(unsafe {
            DisplayHandle::borrow_raw(RawDisplayHandle::Windows(
                raw_window_handle::WindowsDisplayHandle::new(),
            ))
        })
    }
}

/// A live Windows presenter: child window, GPU device, upload texture and the
/// pipeline that blits it.
///
/// Field order is drop order: the GPU objects die first and the window last,
/// because the surface was created against the window's handle. Rust drops
/// fields in declaration order, and getting this wrong is a use-after-destroy
/// the driver may or may not forgive.
// The presenter travels inside the runtime's surface registry, which the runtime shares
// across threads under a mutex: the mutex supplies the exclusion, and this impl supplies
// the crossing. What makes it sound is that every operation the presenter performs is
// thread-agnostic by construction — the child window pumps no messages (it is
// input-transparent and paints only through the swapchain), the wgpu objects are
// themselves `Send`, and the raw handle is only ever an argument to window-manager calls,
// which the window manager serializes across threads on its own. What makes it necessary
// is that frames arrive from producer threads rather than the thread the surface was
// created on — a decoder does not run on the runtime's pump.
unsafe impl Send for Win32Presenter {}

impl core::fmt::Debug for Win32Presenter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Win32Presenter")
            .field("window", &self.window)
            .field("frame_width", &self.frame_width)
            .field("frame_height", &self.frame_height)
            .finish_non_exhaustive()
    }
}

pub struct Win32Presenter {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    /// See `frame_view` — retained for explicit ownership.
    #[allow(
        dead_code,
        reason = "ownership retention — the reads happen through the bind group"
    )]
    sampler: wgpu::Sampler,
    frame_texture: wgpu::Texture,
    surface_config: wgpu::SurfaceConfiguration,
    window: HWND,
    frame_width: u32,
    frame_height: u32,
    /// The sequence and timestamp of the frame most recently presented — the
    /// pipeline's identity truth. Verification of ordering through the whole
    /// stack reads here rather than copying pixels back: cheap enough for the
    /// frame loop, and a frame's identity is what ordering evidence is made of.
    last_presented: Option<(u64, ncui_gpu::surface::FramePts)>,
    /// The frame texture's view and the bind group that holds it, created once
    /// at init because the texture never changes: recreating them per frame was
    /// measured churn — allocation work the presenter does thirty times a second
    /// for a binding that is the same binding every time.
    /// Retained for ownership: the bind group references the view and sampler,
    /// and keeping the handles explicit makes the lifetime visible rather than
    /// relying on the bind group's internal retention.
    #[allow(
        dead_code,
        reason = "ownership retention — the reads happen through the bind group"
    )]
    frame_view: wgpu::TextureView,
    /// The persistent bind group — see `frame_view`.
    bind_group: wgpu::BindGroup,
    /// The reusable padding scratch for frames whose stride does not align to
    /// wgpu's 256-byte row rule. Aligned strides upload zero-copy; unaligned
    /// ones copy into this buffer, which is sized once and kept, because a
    /// 33MB-per-frame allocation cycle at video rate is the churn the 4K
    /// acceptance measurement pinned.
    padding_buffer: Vec<u8>,
}

impl Win32Presenter {
    /// Creates the presenter: registers the class, creates the child window at
    /// the initial placement, and brings the GPU stack up against it.
    ///
    /// The frame dimensions are fixed at creation — a resize is a new frame
    /// texture, not a renegotiation, because the engine's output size changing
    /// mid-stream is a bigger event than a window move.
    ///
    /// # Errors
    ///
    /// The OS's and the GPU's refusals, honestly divided.
    #[allow(
        clippy::too_many_lines,
        reason = "creation is a linear sequence — window, then instance, surface, device, pipeline, texture — and splitting it would scatter one atomic invariant across artificial helpers"
    )]
    pub fn create(
        parent: HWND,
        initial: PlacementRect,
        frame_width: u32,
        frame_height: u32,
    ) -> Result<Self, PresentError> {
        let instance_handle = ensure_class()?;
        // SAFETY: every argument is valid or null in the ways Win32 defines —
        // the class is registered above, the parent is the caller's live window,
        // and the rectangles are plain integers. The returned handle is checked
        // before use.
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TRANSPARENT | WS_EX_NOPARENTNOTIFY,
                CLASS_NAME.as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
                initial.x,
                initial.y,
                initial.width as i32,
                initial.height as i32,
                parent,
                std::ptr::null_mut(),
                instance_handle,
                std::ptr::null(),
            )
        };
        if window.is_null() {
            return Err(PresentError::Os {
                reason: "CreateWindowExW refused the child surface".to_owned(),
            });
        }
        // Creation order is not z-order truth — a sibling created later or
        // re-raised by its own machinery would sit above the surface. Assert
        // the monitor's place immediately, before the first frame.
        // SAFETY: the window was created seconds ago and is owned by this
        // struct; the call merely fails if that stops being true.
        unsafe {
            SetWindowPos(
                window,
                HWND_TOP,
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );
        }

        // SAFETY: the window was just created and is live.
        unsafe {
            ShowWindow(window, SW_SHOW);
        }

        let mut raw =
            Win32WindowHandle::new(NonZeroIsize::new(window as isize).ok_or_else(|| {
                PresentError::Os {
                    reason: "CreateWindowExW returned a null window without an error".to_owned(),
                }
            })?);
        raw.hinstance = NonZeroIsize::new(instance_handle as isize);
        let handles = WindowHandles { raw };

        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(handles)
            .map_err(|error| PresentError::Gpu {
                reason: format!("surface creation refused: {error}"),
            })?;

        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|error| PresentError::Gpu {
            reason: format!("no adapter for the video surface: {error}"),
        })?;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ncui-video-surface"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
            ..Default::default()
        }))
        .map_err(|error| PresentError::Gpu {
            reason: format!("device creation refused: {error}"),
        })?;

        let format = surface
            .get_capabilities(&adapter)
            .formats
            .first()
            .copied()
            .ok_or_else(|| PresentError::Gpu {
                reason: "the surface advertises no formats".to_owned(),
            })?;
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: initial.width.max(1),
            height: initial.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let frame_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ncui-video-frame"),
            size: wgpu::Extent3d {
                width: frame_width,
                height: frame_height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ncui-video-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ncui-video-blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT_SHADER.into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ncui-video-bind-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ncui-video-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ncui-video-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let frame_view = frame_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ncui-video-bind"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&frame_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        Ok(Self {
            surface,
            device,
            queue,
            pipeline,
            sampler,
            frame_texture,
            surface_config,
            window,
            frame_width,
            frame_height,
            last_presented: None,
            frame_view,
            bind_group,
            padding_buffer: Vec::new(),
        })
    }

    /// The child window's handle, for tests verifying placement at the OS level.
    #[must_use]
    pub fn window_handle(&self) -> HWND {
        self.window
    }

    /// The sequence and timestamp of the frame most recently presented, if any.
    ///
    /// This is the ordering evidence the pipeline verification reads: which
    /// frame the display was last asked to show, as the stream numbers it.
    #[must_use]
    pub fn last_presented(&self) -> Option<(u64, ncui_gpu::surface::FramePts)> {
        self.last_presented
    }

    /// Shows or hides the surface's window.
    ///
    /// A surface scrolled entirely out of its clip is hidden rather than
    /// presented offscreen or at zero size: a child surface cannot be clipped
    /// by the DOM, so visibility is the only honest answer the platform has.
    /// Hidden is not torn down — the swapchain and the frame ring survive, and
    /// the next visible placement picks up exactly where the stream is.
    pub fn set_visible(&self, visible: bool) {
        // SAFETY: the window is live (owned by self); ShowWindow is a plain
        // visibility toggle with no reentrancy of consequence.
        unsafe {
            ShowWindow(self.window, if visible { SW_SHOW } else { SW_HIDE });
        }
    }

    /// Reads one presented frame back from the upload texture.
    ///
    /// This is the verification and diagnostics path: what the GPU was asked to
    /// draw is verifiable without involving the OS compositor, whose own truth
    /// is a separate question. It is not free — a full frame copies back over
    /// the bus — so it belongs in evidence and diagnosis, not the frame loop,
    /// and the documentation says so rather than leaving the cost to be
    /// discovered.
    #[must_use]
    pub fn readback_frame(&self) -> Vec<u8> {
        let row_bytes = (self.frame_width * 4) as usize;
        let padded_stride = row_bytes.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize);
        let size = (padded_stride * self.frame_height as usize) as u64;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ncui-video-readback"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.frame_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_stride as u32),
                    rows_per_image: Some(self.frame_height),
                },
            },
            wgpu::Extent3d {
                width: self.frame_width,
                height: self.frame_height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(std::iter::once(encoder.finish()));
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| ());
        let _ = self.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        });
        let data = slice.get_mapped_range().to_vec();
        buffer.unmap();
        // Strip the row padding so the answer is the frame's own shape.
        let mut out = Vec::with_capacity(row_bytes * self.frame_height as usize);
        for row in 0..self.frame_height as usize {
            let start = row * padded_stride;
            out.extend_from_slice(&data[start..start + row_bytes]);
        }
        out
    }
}

impl Drop for Win32Presenter {
    fn drop(&mut self) {
        // SAFETY: the window is owned by this struct and destroyed exactly once,
        // here; a parent that died first makes this a harmless no-op rather than
        // a double free, because child windows die with their parent.
        unsafe {
            DestroyWindow(self.window);
        }
    }
}

impl Presenter<RgbaPixels> for Win32Presenter {
    type Error = PresentError;

    #[allow(
        clippy::too_many_lines,
        reason = "presentation is one linear GPU sequence — validate, write, encode, submit — and splitting it would obscure the ordering the correctness depends on"
    )]
    fn present(&mut self, frame: &Frame<RgbaPixels>) -> Result<(), PresentError> {
        let payload = &frame.payload;
        if payload.width != self.frame_width || payload.height != self.frame_height {
            return Err(PresentError::PayloadMismatch {
                reason: format!(
                    "frame is {}x{} but the surface was built for {}x{}",
                    payload.width, payload.height, self.frame_width, self.frame_height
                ),
            });
        }
        if payload.stride < payload.width * 4 {
            return Err(PresentError::PayloadMismatch {
                reason: format!(
                    "stride {} cannot hold a row of {} RGBA pixels",
                    payload.stride, payload.width
                ),
            });
        }
        let needed = payload.stride as usize * payload.height as usize;
        if payload.bytes.len() < needed {
            return Err(PresentError::PayloadMismatch {
                reason: format!(
                    "{} bytes cannot cover {} rows at stride {}",
                    payload.bytes.len(),
                    payload.height,
                    payload.stride
                ),
            });
        }

        // wgpu rows are padded to 256 bytes; a tight frame whose stride is not a
        // multiple of that copies row by row. The common case — a stride that
        // already aligns — copies in one write, because copies at video rate are
        // the cost this path exists to minimise.
        let row_bytes = (payload.width * 4) as usize;
        let upload_stride = row_bytes.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize);
        // The upload borrow, honestly: `write_texture` copies into the staging
        // belt inside the call, so the slice only ever needs to live this long.
        // An aligned stride therefore uploads with no copy at all — the frame's
        // own memory is the upload layout already. An unaligned stride copies
        // row by row into the reusable scratch: the copy is unavoidable, the
        // allocation was not.
        let upload: &[u8] = if payload.stride as usize == upload_stride {
            &payload.bytes[..needed]
        } else {
            let size = upload_stride * payload.height as usize;
            self.padding_buffer.clear();
            self.padding_buffer.resize(size, 0);
            for row in 0..payload.height as usize {
                let src = row * payload.stride as usize;
                let dst = row * upload_stride;
                self.padding_buffer[dst..dst + row_bytes]
                    .copy_from_slice(&payload.bytes[src..src + row_bytes]);
            }
            &self.padding_buffer
        };
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.frame_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            upload,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(upload_stride as u32),
                rows_per_image: Some(payload.height),
            },
            wgpu::Extent3d {
                width: self.frame_width,
                height: self.frame_height,
                depth_or_array_layers: 1,
            },
        );

        let output = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            other => {
                return Err(PresentError::Gpu {
                    reason: format!("the surface did not yield a frame: {other:?}"),
                });
            }
        };
        let target = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ncui-video-blit"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ncui-video-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
        self.last_presented = Some((frame.sequence, frame.pts));
        Ok(())
    }

    fn place(&mut self, rect: PlacementRect) -> Result<(), PresentError> {
        // SAFETY: the window is live (owned by self), and the rect is plain
        // integers. HWND_TOP rather than passive layering: the surface is a
        // monitor, and a monitor that loses overlaps to the webview's own
        // child — which WebView2 re-raises on its own schedule — displays
        // nothing. Top of the sibling z-order is the surface's correct place,
        // reasserted on every placement because placement is the moment the
        // geometry contract is already being renewed.
        let ok = unsafe {
            SetWindowPos(
                self.window,
                HWND_TOP,
                rect.x,
                rect.y,
                rect.width as i32,
                rect.height as i32,
                SWP_NOACTIVATE,
            )
        };
        if ok == 0 {
            return Err(PresentError::Os {
                reason: "SetWindowPos refused the placement".to_owned(),
            });
        }
        // The swapchain follows the window's size — a stretched present is a
        // blurry one, and a preview monitor is exactly the place blur is noticed.
        if self.surface_config.width != rect.width.max(1)
            || self.surface_config.height != rect.height.max(1)
        {
            self.surface_config.width = rect.width.max(1);
            self.surface_config.height = rect.height.max(1);
            self.surface.configure(&self.device, &self.surface_config);
        }
        Ok(())
    }

    fn teardown(&mut self) {
        // SAFETY: same invariant as Drop; explicit teardown then drop is a
        // harmless double-destroy because DestroyWindow on an already-dead
        // handle is a no-op error, not undefined behaviour.
        unsafe {
            DestroyWindow(self.window);
        }
    }
}
