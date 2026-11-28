//! The render pass: turning a frame into something the GPU can draw.
//!
//! The pass is split in two, and the split is the point:
//!
//! * [`RenderPlan`] — the *pure* part. It turns a [`Frame`] into vertices and indices,
//!   and it can be tested on a machine with no GPU at all. A renderer whose only test is
//!   "it did not crash on a machine with a display" has tested almost nothing.
//! * the `wgpu` binding — it uploads the plan and submits it.
//!
//! # The vertex format
//!
//! Two `f32` positions, four `u8` colour channels and two `f32` texture coordinates:
//! twenty bytes per vertex. Small
//! enough that a frame with ten thousand islands is a hundred and twenty kilobytes, and
//! fixed-layout, so the shader reads offsets rather than a description of a description.
//!
//! # Where the coordinates are converted
//!
//! The plan holds positions in **clip space** — the `-1..=1` space the GPU's vertex stage
//! expects — not in pixels. The conversion needs the size of the target, and the size of
//! the target is not known when a frame is built: a frame is a description of what to
//! draw, and it may be drawn into more than one surface. Converting on the CPU, with the
//! surface size in hand, keeps the shader to a pass-through and keeps a hardcoded
//! resolution out of the code — a hardcoded resolution is a bug that only shows up on
//! targets that are not the one it was written for.

use crate::frame::Frame;
use crate::surface::SurfaceSize;

/// One vertex of an island quad.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    /// The x coordinate, in clip space.
    pub x: f32,
    /// The y coordinate, in clip space.
    pub y: f32,
    /// The colour, as red, green, blue and alpha in `0..=255`.
    pub colour: [u8; 4],
    /// Where the vertex samples the glyph atlas, as u and v.
    ///
    /// Every vertex samples: the pipeline is one textured path for everything, and a
    /// solid rectangle is a quad over the atlas's white texel. The uniformity is the
    /// point — a shader with a branch is two shaders, and two shaders is two pipelines.
    pub uv: [f32; 2],
}

impl Vertex {
    /// The size of one vertex on the wire.
    pub const WIRE_BYTES: usize = 20;
}

/// Why a frame could not be turned into a render plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// An island's rectangle is not a rectangle.
    InvalidRectangle {
        /// Which island, by index.
        index: usize,
        /// Why it was refused.
        reason: String,
    },
    /// A custom island carries content this pass cannot draw yet.
    ///
    /// Rects expand exactly — a solid axis-aligned rectangle clipped to an axis-aligned
    /// clip is still a rectangle. Text runs and bitmap blits are not: they need the
    /// glyph atlas and the texture binding, and until those land the honest answer is a
    /// refusal. Drawing nothing for them would put a frame on the screen that is not
    /// the frame the producer built, and a missing playhead label is exactly the kind
    /// of lie a pixel gate exists to catch.
    UnrenderableContent {
        /// Which custom island, by index.
        island: usize,
        /// What kind of command cannot be drawn yet.
        kind: &'static str,
    },
    /// A text run could not be laid out — shaping failed, or the glyph atlas is full.
    TextLayout {
        /// Which custom island, by index.
        island: usize,
        /// What the layout said.
        reason: String,
    },
}

impl core::fmt::Display for RenderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidRectangle { index, reason } => {
                write!(f, "island {index} is not a rectangle: {reason}")
            }
            Self::UnrenderableContent { island, kind } => {
                write!(
                    f,
                    "custom island {island} carries a {kind}, which this pass cannot draw yet"
                )
            }
            Self::TextLayout { island, reason } => {
                write!(
                    f,
                    "custom island {island}'s text could not be laid out: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for RenderError {}

/// Emits one axis-aligned quad: four vertices, two triangles.
///
/// The positions arrive in clip space already converted; the texture coordinates arrive
/// interpolated for whatever clipping the quad went through, so what is drawn samples
/// exactly the part of the coverage that survived the clip.
#[allow(
    clippy::too_many_arguments,
    reason = "a quad is eight pieces of one shape; grouping them into a struct would invent a type used nowhere else"
)]
fn push_quad(
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    colour: [u8; 4],
    uv: [f32; 4],
) {
    let base = vertices.len() as u32;
    vertices.push(Vertex {
        x: left,
        y: top,
        colour,
        uv: [uv[0], uv[1]],
    });
    vertices.push(Vertex {
        x: right,
        y: top,
        colour,
        uv: [uv[2], uv[1]],
    });
    vertices.push(Vertex {
        x: right,
        y: bottom,
        colour,
        uv: [uv[2], uv[3]],
    });
    vertices.push(Vertex {
        x: left,
        y: bottom,
        colour,
        uv: [uv[0], uv[3]],
    });
    indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

/// The uv every solid quad carries: the atlas's white texel, sampled at all four corners.
fn solid_uv() -> [f32; 4] {
    let [u, v] = crate::text_layout::ContentAtlas::white_uv();
    [u, v, u, v]
}

/// Clips a textured quad to a rectangle, interpolating the texture coordinates.
///
/// A textured quad is not a rectangle the way a solid one is: cutting its left edge off
/// without moving the left texture coordinate would stretch the coverage, drawing pixels
/// the clip was meant to hide. The interpolation is linear because the sampling is.
fn clip_textured(
    quad: crate::text_layout::PositionedGlyph,
    clip: &ncui_gpu::geometry::Rect,
) -> Option<(f32, f32, f32, f32, [f32; 4])> {
    let bounds = ncui_gpu::geometry::Rect::new(quad.x, quad.y, quad.width, quad.height);
    let clipped = clip.intersection(&bounds)?;
    if clipped.width <= 0.0 || clipped.height <= 0.0 || quad.width <= 0.0 || quad.height <= 0.0 {
        return None;
    }
    let [u0, v0, u1, v1] = quad.uv;
    let du = u1 - u0;
    let dv = v1 - v0;
    let uv = [
        u0 + (clipped.x - quad.x) / quad.width * du,
        v0 + (clipped.y - quad.y) / quad.height * dv,
        u0 + (clipped.x + clipped.width - quad.x) / quad.width * du,
        v0 + (clipped.y + clipped.height - quad.y) / quad.height * dv,
    ];
    Some((clipped.x, clipped.y, clipped.width, clipped.height, uv))
}

/// The custom-content support an expansion carries: the cache coverage comes from, the
/// atlas everything packs into, and the store blits resolve against. The cache and the
/// atlas travel together because a glyph positioned from one font and packed from
/// another is two fonts' idea of one letter; the store is optional because a frame
/// without blits should not have to carry one.
pub struct ContentSupport<'a> {
    /// The glyph cache — and through it, the font — runs are shaped and rasterised with.
    pub cache: &'a crate::atlas::GlyphCache,
    /// The atlas coverage and bitmap pixels pack into.
    pub atlas: &'a mut crate::text_layout::ContentAtlas,
    /// The bitmap store blit keys resolve against, when the frame carries blits.
    pub bitmaps: Option<&'a crate::bitmap::BitmapStore>,
}

impl core::fmt::Debug for ContentSupport<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ContentSupport")
            .field("cache", &self.cache)
            .field("atlas", &self.atlas)
            .field(
                "bitmaps",
                &self.bitmaps.map(crate::bitmap::BitmapStore::len),
            )
            .finish()
    }
}

/// Expands one custom island's display list into quads, appended to the plan's buffers.
///
/// The walk applies three coordinate truths at once:
///
/// * content coordinates become window coordinates by adding the island's position and
///   subtracting its scroll — the same arithmetic the input host applies in reverse when
///   it routes a pointer, which is what keeps the paint under the cursor the cursor
///   believes it is over;
/// * the island's viewport is the outermost clip — content never draws outside its
///   island, exactly as the row-based layout clips rows;
/// * the list's own clip stack narrows from there, and — because a solid axis-aligned
///   rectangle clipped to an axis-aligned rectangle is still a rectangle — the clipping
///   is done geometrically, in the plan, with no scissor state in the pass at all.
///
/// Text runs draw through the glyph atlas when the expansion carries [`TextSupport`]:
/// the run is shaped, its coverage packed, and each glyph emitted as a textured quad
/// whose texture coordinates track whatever the clip stack leaves of it. Without text
/// support a text run still refuses loudly — a missing label is exactly the kind of lie
/// a pixel gate exists to catch. Bitmap blits refuse either way: they need the host's
/// bitmap store, which is a texture path of its own.
#[allow(
    clippy::too_many_lines,
    reason = "custom islands expand several content kinds under one clipping stack; splitting mid-walk would hide the shared clip bookkeeping"
)]
fn expand_custom(
    custom: &crate::frame::CustomIsland,
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
    width: f32,
    height: f32,
    custom_index: usize,
    text: Option<&mut ContentSupport<'_>>,
) -> Result<(), RenderError> {
    use ncui_gpu::display::Command;
    use ncui_gpu::geometry::Rect;

    let viewport = Rect::new(custom.x, custom.y, custom.width, custom.height);
    let origin_x = custom.x - custom.scroll_x;
    let origin_y = custom.y - custom.scroll_y;
    let to_clip =
        |x: f32, y: f32| -> (f32, f32) { ((x / width) * 2.0 - 1.0, 1.0 - (y / height) * 2.0) };
    // The clip stack, in window coordinates, starting at the viewport. `None` means
    // "clips everything", mirroring the display list's own semantics for a clip pushed
    // entirely outside its parent.
    let mut clips: Vec<Option<Rect>> = vec![Some(viewport)];
    let mut text = text;
    for command in custom.list.commands() {
        match command {
            Command::ClipPush { bounds } => {
                let windowed = Rect::new(
                    bounds.x + origin_x,
                    bounds.y + origin_y,
                    bounds.width,
                    bounds.height,
                );
                let effective = match clips.last() {
                    None => Some(windowed),
                    Some(enclosing) => enclosing.and_then(|clip| clip.intersection(&windowed)),
                };
                clips.push(effective);
            }
            Command::ClipPop => {
                clips.pop();
                // A list is validated at construction, so a pop cannot empty the stack
                // below the viewport — but defending the invariant costs one line and a
                // corrupt frame is never worth it.
                if clips.is_empty() {
                    clips.push(Some(viewport));
                }
            }
            Command::Rect { bounds, colour } => {
                let windowed = Rect::new(
                    bounds.x + origin_x,
                    bounds.y + origin_y,
                    bounds.width,
                    bounds.height,
                );
                let clipped = match clips.last() {
                    None => Some(windowed),
                    Some(enclosing) => enclosing.and_then(|clip| clip.intersection(&windowed)),
                };
                if let Some(draw) = clipped
                    && draw.width > 0.0
                    && draw.height > 0.0
                {
                    let (left, top) = to_clip(draw.x, draw.y);
                    let (right, bottom) = to_clip(draw.x + draw.width, draw.y + draw.height);
                    push_quad(
                        vertices,
                        indices,
                        left,
                        top,
                        right,
                        bottom,
                        *colour,
                        solid_uv(),
                    );
                }
            }
            Command::TextRun {
                origin,
                text: run_text,
                size,
                colour,
            } => {
                let Some(support) = text.as_deref_mut() else {
                    return Err(RenderError::UnrenderableContent {
                        island: custom_index,
                        kind: "text run",
                    });
                };
                let shaped = crate::text_layout::layout_run(
                    support.cache,
                    support.atlas,
                    (origin.x + origin_x, origin.y + origin_y),
                    run_text,
                    *size,
                )
                .map_err(|error| RenderError::TextLayout {
                    island: custom_index,
                    reason: error.to_string(),
                })?;
                let clip = clips.last().copied().flatten();
                for glyph in shaped {
                    let Some(clip_rect) = clip else {
                        break;
                    };
                    if let Some((x, y, w, h, uv)) = clip_textured(glyph, &clip_rect) {
                        let (left, top) = to_clip(x, y);
                        let (right, bottom) = to_clip(x + w, y + h);
                        push_quad(vertices, indices, left, top, right, bottom, *colour, uv);
                    }
                }
            }
            Command::Blit { bounds, source } => {
                let Some(support) = text.as_deref_mut() else {
                    return Err(RenderError::UnrenderableContent {
                        island: custom_index,
                        kind: "bitmap blit",
                    });
                };
                // A missing key draws nothing — the transparent well is the truth of
                // content that has not arrived, per the store's documented semantics.
                // A frame that carries blits without a store at all is a different
                // thing: a defect in the wiring, refused loudly.
                let Some(store) = support.bitmaps else {
                    return Err(RenderError::UnrenderableContent {
                        island: custom_index,
                        kind: "bitmap blit without a bitmap store",
                    });
                };
                if let Some(bitmap) = store.get(source) {
                    let packing_key =
                        store
                            .packing_key(source)
                            .ok_or(RenderError::UnrenderableContent {
                                island: custom_index,
                                kind: "a live bitmap key without a generation",
                            })?;
                    let entry = support
                        .atlas
                        .pack_bitmap(&packing_key, bitmap.width, bitmap.height, &bitmap.rgba)
                        .map_err(|error| RenderError::TextLayout {
                            island: custom_index,
                            reason: error.to_string(),
                        })?;
                    let windowed = Rect::new(
                        bounds.x + origin_x,
                        bounds.y + origin_y,
                        bounds.width,
                        bounds.height,
                    );
                    let clipped = match clips.last() {
                        None => Some(windowed),
                        Some(enclosing) => enclosing.and_then(|clip| clip.intersection(&windowed)),
                    };
                    if let Some(draw) = clipped
                        && draw.width > 0.0
                        && draw.height > 0.0
                        && bounds.width > 0.0
                        && bounds.height > 0.0
                    {
                        // The blit scales the bitmap to its bounds, so the uv span is
                        // the coverage's whole span, clipped proportionally — the same
                        // interpolation a glyph gets, at bitmap rather than glyph scale.
                        let uv_full = entry.uv();
                        let glyph_shaped = crate::text_layout::PositionedGlyph {
                            x: windowed.x,
                            y: windowed.y,
                            width: bounds.width,
                            height: bounds.height,
                            uv: uv_full,
                        };
                        let clip_rect = clips.last().copied().flatten();
                        if let Some(clip_rect) = clip_rect
                            && let Some((x, y, w, h, uv)) = clip_textured(glyph_shaped, &clip_rect)
                        {
                            let (left, top) = to_clip(x, y);
                            let (right, bottom) = to_clip(x + w, y + h);
                            // White vertex colour: the bitmap's own pixels are the
                            // colour, and the multiply must pass them through.
                            push_quad(
                                vertices,
                                indices,
                                left,
                                top,
                                right,
                                bottom,
                                [255, 255, 255, 255],
                                uv,
                            );
                        }
                    }
                }
            }
            // Hit regions are input and accessibility geometry — they draw nothing, and
            // drawing nothing for them is correct rather than a gap.
            Command::HitRegion { .. } => {}
        }
    }
    Ok(())
}

/// The vertices and indices one frame turns into.
///
/// A plan is the whole of what the render pass needs to know. It holds no device state,
/// so it can be built, inspected and asserted on without a GPU.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderPlan {
    /// The colour the target is cleared with.
    pub clear_colour: [u8; 4],
    /// The vertices, four per island.
    pub vertices: Vec<Vertex>,
    /// The indices, six per island.
    pub indices: Vec<u32>,
    /// How many islands the plan covers.
    pub island_count: usize,
}

impl RenderPlan {
    /// Builds the plan for a frame, drawn into a surface of the given size.
    ///
    /// The size is the target's, in physical pixels. The positions it produces are in clip
    /// space, which is what the vertex stage expects.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::InvalidRectangle`] for an island whose rectangle has a
    /// negative size, a non-finite edge, or a colour channel out of range. A rectangle
    /// that is not a rectangle is a defect in the producer, and drawing it anyway would
    /// put a number on the screen where a shape should be.
    pub fn from_frame(frame: &Frame, surface: SurfaceSize) -> Result<Self, RenderError> {
        Self::from_frame_inner(frame, surface, None)
    }

    /// Builds the plan for a frame whose custom islands may carry text.
    ///
    /// The cache shapes and rasterises; the atlas packs. Both are borrowed rather than
    /// owned because they outlive the frame — the atlas is the same one the draw call
    /// uploads from, and a plan that owned its own copy would draw with coverage the
    /// texture never received.
    ///
    /// # Errors
    ///
    /// As [`RenderPlan::from_frame`], plus [`RenderError::TextLayout`] when a run cannot
    /// be shaped or the atlas fills.
    pub fn from_frame_text(
        frame: &Frame,
        surface: SurfaceSize,
        cache: &crate::atlas::GlyphCache,
        atlas: &mut crate::text_layout::ContentAtlas,
    ) -> Result<Self, RenderError> {
        Self::from_frame_content(frame, surface, cache, atlas, None)
    }

    /// Builds the plan for a frame whose custom islands may carry text and blits.
    ///
    /// The store is optional because a frame without blits should not have to carry
    /// one — but a frame that *has* blits and no store fails loudly rather than
    /// drawing nothing: empty wells are for bitmaps that have not arrived, not for
    /// wiring that forgot the store.
    ///
    /// # Errors
    ///
    /// As [`RenderPlan::from_frame`], plus [`RenderError::TextLayout`] when a run cannot
    /// be shaped or the atlas fills.
    pub fn from_frame_content(
        frame: &Frame,
        surface: SurfaceSize,
        cache: &crate::atlas::GlyphCache,
        atlas: &mut crate::text_layout::ContentAtlas,
        bitmaps: Option<&crate::bitmap::BitmapStore>,
    ) -> Result<Self, RenderError> {
        let mut support = ContentSupport {
            cache,
            atlas,
            bitmaps,
        };
        Self::from_frame_inner(frame, surface, Some(&mut support))
    }

    /// The shared expansion. The `text` parameter is the whole difference between the
    /// two public entry points: present, text runs draw; absent, they refuse.
    fn from_frame_inner(
        frame: &Frame,
        surface: SurfaceSize,
        mut text: Option<&mut ContentSupport<'_>>,
    ) -> Result<Self, RenderError> {
        let width = surface.width.max(1) as f32;
        let height = surface.height.max(1) as f32;
        let mut vertices = Vec::with_capacity(frame.islands.len() * 4);
        let mut indices = Vec::with_capacity(frame.islands.len() * 6);
        for (index, island) in frame.islands.iter().enumerate() {
            if !island.is_valid() {
                return Err(RenderError::InvalidRectangle {
                    index,
                    reason: "the rectangle has a negative size or a non-finite edge".to_owned(),
                });
            }
            // The frame's coordinates are in physical pixels with the origin at the top
            // left. The clip space the GPU expects has its origin at the centre and its y
            // axis pointing up, so the conversion is a flip and a scale.
            let to_clip = |x: f32, y: f32| -> (f32, f32) {
                ((x / width) * 2.0 - 1.0, 1.0 - (y / height) * 2.0)
            };
            let (left, top) = to_clip(island.x, island.y);
            let (right, bottom) = to_clip(island.x + island.width, island.y + island.height);
            let colour = island.colour;
            push_quad(
                &mut vertices,
                &mut indices,
                left,
                top,
                right,
                bottom,
                colour,
                solid_uv(),
            );
        }
        for (custom_index, custom) in frame.custom_islands.iter().enumerate() {
            if !custom.is_valid() {
                return Err(RenderError::InvalidRectangle {
                    index: custom_index,
                    reason: "the custom island's viewport has a negative size or a non-finite edge"
                        .to_owned(),
                });
            }
            expand_custom(
                custom,
                &mut vertices,
                &mut indices,
                width,
                height,
                custom_index,
                text.as_deref_mut(),
            )?;
        }
        Ok(Self {
            clear_colour: frame.clear_colour,
            vertices,
            indices,
            island_count: frame.islands.len(),
        })
    }

    /// The number of bytes the vertex buffer occupies.
    #[must_use]
    pub fn vertex_bytes(&self) -> usize {
        self.vertices.len() * Vertex::WIRE_BYTES
    }

    /// Whether the plan would draw nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty()
    }
}

/// Draws a plan into a target.
///
/// The `wgpu` binding. It is behind the `wgpu-device` feature so that the crate — and
/// the whole workspace — compiles on a machine with no graphics stack.
#[cfg(feature = "wgpu-device")]
pub mod wgpu_pass {
    use super::{RenderPlan, Vertex};
    use wgpu::util::DeviceExt;

    /// A pipeline that draws coloured quads.
    ///
    /// Built once per device: creating a pipeline is expensive, and rebuilding it every
    /// frame would make the frame cost more than the drawing does.
    #[derive(Debug)]
    pub struct QuadPipeline {
        pipeline: wgpu::RenderPipeline,
        layout: wgpu::PipelineLayout,
        /// The GPU mirror of the glyph atlas. The texture is created once — its size
        /// never changes — and the coverage bytes arrive through
        /// [`QuadPipeline::sync_atlas`].
        atlas_texture: wgpu::Texture,
        /// The bind group the pass draws with: the atlas view and its sampler, bound
        /// once because neither ever changes.
        atlas_bind_group: wgpu::BindGroup,
    }

    impl QuadPipeline {
        /// Builds the pipeline for a device.
        ///
        /// `wgpu` 29 creates resources infallibly and reports validation failures through
        /// its error handler rather than through a `Result`. That is why this returns the
        /// pipeline and not a `Result`: there is no error to return, and inventing one
        /// would mean promising a guarantee `wgpu` does not make. A caller that wants the
        /// validation failures can install an error handler on the device, and the
        /// renderer does exactly that.
        ///
        /// The queue is taken because the atlas's white texel — what every solid quad
        /// samples — is seeded at birth: a pipeline that only ever draws rectangles
        /// must still read coverage of one, and a texture never written is zeros.
        #[must_use]
        #[allow(
            clippy::too_many_lines,
            reason = "pipeline construction is one linear sequence of wgpu objects; splitting it would rename the same steps, not reduce them"
        )]
        pub fn new(
            device: &wgpu::Device,
            queue: &wgpu::Queue,
            surface_format: wgpu::TextureFormat,
        ) -> Self {
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("ncui-gpu-renderer-quad"),
                source: wgpu::ShaderSource::Wgsl(
                    r"
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) colour: vec4<f32>,
    @location(2) uv: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) colour: vec4<f32>,
    @location(1) uv: vec2<f32>,
}

// The glyph atlas: one R8 texture every quad samples. A solid rectangle samples the
// white texel at the origin, so its coverage is uniformly one and what comes out is its
// colour unchanged; a glyph samples its own coverage and the byte modulates the alpha.
// One path for both is the design — a branch here is two pipelines, and two pipelines
// is a draw-call split in the middle of every line of text.
@group(0) @binding(0) var atlas: texture_2d<f32>;
@group(0) @binding(1) var atlas_sampler: sampler;

@vertex
fn vertex_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    // The positions arrive in clip space: the conversion from physical pixels is done on
    // the CPU, where the size of the target is known.
    out.position = vec4<f32>(input.position, 0.0, 1.0);
    out.colour = input.colour;
    out.uv = input.uv;
    return out;
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    // One multiply for everything: a glyph's texel is white with its coverage in
    // alpha, so the run's colour comes through untouched and the coverage modulates
    // alpha; a bitmap's texel is its own colour, sampled through a white vertex
    // colour. The two cases are the same multiply — that is why the atlas is RGBA.
    let texel = textureSample(atlas, atlas_sampler, input.uv);
    return input.colour * texel;
}
"
                    .into(),
                ),
            });
            let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("ncui-gpu-renderer-atlas-layout"),
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
                label: Some("ncui-gpu-renderer-layout"),
                bind_group_layouts: &[Some(&atlas_layout)],
                immediate_size: 0,
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("ncui-gpu-renderer-pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vertex_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: Vertex::WIRE_BYTES as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &[
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x2,
                                offset: 0,
                                shader_location: 0,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Unorm8x4,
                                offset: 8,
                                shader_location: 1,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x2,
                                offset: 12,
                                shader_location: 2,
                            },
                        ],
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fragment_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: surface_format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
            let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ncui-gpu-renderer-atlas"),
                size: wgpu::Extent3d {
                    width: crate::text_layout::ATLAS_SIZE,
                    height: crate::text_layout::ATLAS_SIZE,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &atlas_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y: 0, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &[255u8, 255, 255, 255],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4),
                    rows_per_image: Some(1),
                },
                wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
            let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("ncui-gpu-renderer-sampler"),
                // Nearest, not linear: coverage is per-pixel and the quads are sized one
                // texel to one pixel, so filtering would only blur the edges the
                // rasteriser placed exactly.
                mag_filter: wgpu::FilterMode::Nearest,
                min_filter: wgpu::FilterMode::Nearest,
                ..Default::default()
            });
            let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ncui-gpu-renderer-atlas-bind-group"),
                layout: &atlas_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&atlas_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            });
            Self {
                pipeline,
                layout,
                atlas_texture,
                atlas_bind_group,
            }
        }

        /// Uploads whatever the atlas changed since its last sync.
        ///
        /// The CPU mirror is the truth — a device loss rebuilds from it — and the dirty
        /// list is what the upload copies, because diffing a megabyte per frame would
        /// cost more than the drawing does. Region-by-region writes rather than one
        /// whole-texture write: a frame that adds one glyph pays for one glyph.
        pub fn sync_atlas(
            &self,
            queue: &wgpu::Queue,
            atlas: &mut crate::text_layout::ContentAtlas,
        ) {
            for region in atlas.dirty() {
                let bpp = crate::text_layout::ATLAS_BPP;
                let mut bytes = Vec::with_capacity((region.width * region.height * bpp) as usize);
                for row in 0..region.height {
                    let start = (((region.y + row) * crate::text_layout::ATLAS_SIZE + region.x)
                        * bpp) as usize;
                    bytes.extend_from_slice(
                        &atlas.bytes()[start..start + (region.width * bpp) as usize],
                    );
                }
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.atlas_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: region.x,
                            y: region.y,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &bytes,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(region.width * crate::text_layout::ATLAS_BPP),
                        rows_per_image: Some(region.height),
                    },
                    wgpu::Extent3d {
                        width: region.width,
                        height: region.height,
                        depth_or_array_layers: 1,
                    },
                );
            }
            atlas.take_dirty();
        }

        /// Draws a plan into a texture view.
        ///
        /// Every `wgpu` 29 operation here is infallible, so this returns nothing: a lost
        /// device is detected by the health check the renderer performs after the frame, not
        /// by the draw call, and that is the path ADR-002 asks to be tested.
        pub fn draw(
            &self,
            device: &wgpu::Device,
            queue: &wgpu::Queue,
            view: &wgpu::TextureView,
            plan: &RenderPlan,
        ) {
            // The vertex data is assembled as plain bytes rather than through a `bytemuck`
            // cast: the layout is twelve bytes of a known shape, and writing it out keeps the
            // crate free of a dependency whose only job would be to reinterpret a slice.
            let mut vertex_bytes = Vec::with_capacity(plan.vertices.len() * Vertex::WIRE_BYTES);
            for vertex in &plan.vertices {
                vertex_bytes.extend_from_slice(&vertex.x.to_le_bytes());
                vertex_bytes.extend_from_slice(&vertex.y.to_le_bytes());
                vertex_bytes.extend_from_slice(&vertex.colour);
                vertex_bytes.extend_from_slice(&vertex.uv[0].to_le_bytes());
                vertex_bytes.extend_from_slice(&vertex.uv[1].to_le_bytes());
            }
            let mut index_bytes = Vec::with_capacity(plan.indices.len() * 4);
            for index in &plan.indices {
                index_bytes.extend_from_slice(&index.to_le_bytes());
            }
            let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("ncui-gpu-renderer-vertices"),
                contents: &vertex_bytes,
                usage: wgpu::BufferUsages::VERTEX,
            });
            let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("ncui-gpu-renderer-indices"),
                contents: &index_bytes,
                usage: wgpu::BufferUsages::INDEX,
            });

            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ncui-gpu-renderer-encoder"),
            });
            {
                let clear = wgpu::Color {
                    r: f64::from(plan.clear_colour[0]) / 255.0,
                    g: f64::from(plan.clear_colour[1]) / 255.0,
                    b: f64::from(plan.clear_colour[2]) / 255.0,
                    a: f64::from(plan.clear_colour[3]) / 255.0,
                };
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("ncui-gpu-renderer-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(clear),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.atlas_bind_group, &[]);
                if !plan.indices.is_empty() {
                    pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                    pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..plan.indices.len() as u32, 0, 0..1);
                }
            }
            queue.submit(Some(encoder.finish()));
        }

        /// The pipeline's layout, so a caller can check it is the one it expects.
        #[must_use]
        pub fn layout(&self) -> &wgpu::PipelineLayout {
            &self.layout
        }
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::float_cmp,
    reason = "test assertions must abort loudly on failure; skips and evidence report on stderr; pixel comparisons compare exact bits"
)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::IslandRect;
    use crate::surface::SurfaceSize;

    /// The surface the tests draw into: 100 by 100 physical pixels.
    fn surface() -> SurfaceSize {
        SurfaceSize::new(100, 100)
    }

    fn island(x: f32, y: f32, w: f32, h: f32) -> IslandRect {
        IslandRect {
            x,
            y,
            width: w,
            height: h,
            colour: [10, 20, 30, 255],
        }
    }

    #[test]
    fn an_empty_frame_produces_an_empty_plan() {
        let frame = Frame {
            custom_islands: Vec::new(),
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        let plan = RenderPlan::from_frame(&frame, surface()).expect("a plan");
        assert!(plan.is_empty());
        assert_eq!(plan.island_count, 0);
        assert_eq!(plan.vertex_bytes(), 0);
    }

    #[test]
    fn one_island_produces_four_vertices_and_six_indices() {
        let frame = Frame {
            custom_islands: Vec::new(),
            clear_colour: [0, 0, 0, 255],
            islands: vec![island(0.0, 0.0, 10.0, 20.0)],
        };
        let plan = RenderPlan::from_frame(&frame, surface()).expect("a plan");
        assert_eq!(plan.vertices.len(), 4);
        assert_eq!(plan.indices.len(), 6);
        assert_eq!(plan.vertex_bytes(), 80);
    }

    #[test]
    fn a_quads_corners_are_the_rectangles_corners() {
        // The top-left of the surface is the top-left in clip space: x is -1 and y is +1.
        let frame = Frame {
            custom_islands: Vec::new(),
            clear_colour: [0, 0, 0, 255],
            islands: vec![island(0.0, 0.0, 50.0, 50.0)],
        };
        let plan = RenderPlan::from_frame(&frame, surface()).expect("a plan");
        let positions: Vec<(f32, f32)> = plan
            .vertices
            .iter()
            .map(|vertex| (vertex.x, vertex.y))
            .collect();
        assert_eq!(
            positions,
            vec![(-1.0, 1.0), (0.0, 1.0), (0.0, 0.0), (-1.0, 0.0)]
        );
    }

    #[test]
    fn a_rectangle_in_the_bottom_right_lands_in_the_bottom_right() {
        // The origin is the top left and the y axis points down, so the bottom right of
        // the surface is the origin of clip space.
        let frame = Frame {
            custom_islands: Vec::new(),
            clear_colour: [0, 0, 0, 255],
            islands: vec![island(50.0, 50.0, 50.0, 50.0)],
        };
        let plan = RenderPlan::from_frame(&frame, surface()).expect("a plan");
        let first = plan.vertices[0];
        assert!((first.x - 0.0).abs() < 1e-6, "x was {}", first.x);
        assert!((first.y - 0.0).abs() < 1e-6, "y was {}", first.y);
    }

    #[test]
    fn each_island_gets_its_own_indices() {
        let frame = Frame {
            custom_islands: Vec::new(),
            clear_colour: [0, 0, 0, 255],
            islands: vec![island(0.0, 0.0, 1.0, 1.0), island(2.0, 2.0, 1.0, 1.0)],
        };
        let plan = RenderPlan::from_frame(&frame, surface()).expect("a plan");
        assert_eq!(plan.indices[0..6], [0, 1, 2, 0, 2, 3]);
        assert_eq!(plan.indices[6..12], [4, 5, 6, 4, 6, 7]);
    }

    #[test]
    fn a_rectangle_that_is_not_a_rectangle_is_refused() {
        let frame = Frame {
            custom_islands: Vec::new(),
            clear_colour: [0, 0, 0, 255],
            islands: vec![island(0.0, 0.0, -1.0, 1.0)],
        };
        let error = RenderPlan::from_frame(&frame, surface()).expect_err("must be refused");
        assert!(
            matches!(error, RenderError::InvalidRectangle { index: 0, .. }),
            "got {error}"
        );
    }

    #[test]
    fn the_plan_carries_the_clears_colour() {
        let frame = Frame {
            custom_islands: Vec::new(),
            clear_colour: [1, 2, 3, 4],
            islands: Vec::new(),
        };
        let plan = RenderPlan::from_frame(&frame, surface()).expect("a plan");
        assert_eq!(plan.clear_colour, [1, 2, 3, 4]);
    }

    use crate::frame::CustomIsland;
    use ncui_gpu::display::{DisplayList, RegionRole};
    use ncui_gpu::geometry::{LogicalPoint, Rect};

    /// A timeline-flavoured custom island: a track, a clip body, waveform bars clipped
    /// to the clip, and a playhead — the same shape the display-list tests use, so the
    /// pixel and the hit-test gates assert against one truth.
    fn custom_island(list: DisplayList) -> CustomIsland {
        CustomIsland {
            x: 100.0,
            y: 100.0,
            width: 200.0,
            height: 48.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            list,
        }
    }

    /// The window-space rect a quad covers, back-converted from clip space.
    fn window_rects(plan: &RenderPlan, surface: SurfaceSize) -> Vec<(f32, f32, f32, f32)> {
        plan.vertices
            .chunks(4)
            .map(|quad| {
                let xs: Vec<f32> = quad
                    .iter()
                    .map(|v| v.x.midpoint(1.0) * surface.width as f32)
                    .collect();
                let ys: Vec<f32> = quad
                    .iter()
                    .map(|v| (1.0 - v.y) / 2.0 * surface.height as f32)
                    .collect();
                let left = xs.iter().copied().fold(f32::INFINITY, f32::min);
                let right = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                let top = ys.iter().copied().fold(f32::INFINITY, f32::min);
                let bottom = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                (left, top, right - left, bottom - top)
            })
            .collect()
    }

    #[test]
    fn a_custom_islands_rect_lands_at_its_window_position() {
        let list = DisplayList::builder()
            .rect(Rect::new(10.0, 4.0, 50.0, 40.0), [60, 90, 200, 255])
            .finish()
            .expect("valid");
        let frame = Frame {
            custom_islands: vec![custom_island(list)],
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        let size = surface();
        let plan = RenderPlan::from_frame(&frame, size).expect("a plan");
        let rects = window_rects(&plan, size);
        assert_eq!(rects.len(), 1);
        // Island at (100,100), content rect at content (10,4): window (110,104).
        let (x, y, w, h) = rects[0];
        assert!((x - 110.0).abs() < 0.01, "x: {x}");
        assert!((y - 104.0).abs() < 0.01, "y: {y}");
        assert!((w - 50.0).abs() < 0.01, "w: {w}");
        assert!((h - 40.0).abs() < 0.01, "h: {h}");
    }

    #[test]
    fn scroll_offsets_the_content_exactly_as_the_host_offsets_the_pointer() {
        let list = DisplayList::builder()
            .rect(Rect::new(50.0, 20.0, 50.0, 20.0), [60, 90, 200, 255])
            .finish()
            .expect("valid");
        let mut island = custom_island(list);
        island.scroll_x = 30.0;
        island.scroll_y = 12.0;
        let frame = Frame {
            custom_islands: vec![island],
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        let size = surface();
        let plan = RenderPlan::from_frame(&frame, size).expect("a plan");
        let (x, y, _, _) = window_rects(&plan, size)[0];
        // Content x=50 scrolled right by 30 lands at window 100 + 50 - 30 = 120, inside
        // the viewport, so the clipping the narrower cases exercise does not apply.
        assert!((x - 120.0).abs() < 0.01, "x: {x}");
        // Content y=20 scrolled down by 12 lands at window 100 + 20 - 12 = 108.
        assert!((y - 108.0).abs() < 0.01, "y: {y}");
    }

    #[test]
    fn a_scrolled_rect_clips_at_the_viewport_edge() {
        let list = DisplayList::builder()
            .rect(Rect::new(10.0, 4.0, 50.0, 40.0), [60, 90, 200, 255])
            .finish()
            .expect("valid");
        let mut island = custom_island(list);
        island.scroll_x = 30.0;
        let frame = Frame {
            custom_islands: vec![island],
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        let size = surface();
        let plan = RenderPlan::from_frame(&frame, size).expect("a plan");
        let (x, _, w, _) = window_rects(&plan, size)[0];
        // The rect's window position is 100 + 10 - 30 = 80 — twenty pixels past the
        // viewport's left edge. What survives is the part inside: starting at 100,
        // thirty wide of the original fifty.
        assert!((x - 100.0).abs() < 0.01, "x: {x}");
        assert!((w - 30.0).abs() < 0.01, "w: {w}");
    }

    #[test]
    fn content_is_clipped_to_the_viewport_even_without_a_clip_command() {
        let list = DisplayList::builder()
            // The rect starts inside but extends past the island's right edge.
            .rect(Rect::new(150.0, 0.0, 200.0, 48.0), [60, 90, 200, 255])
            .finish()
            .expect("valid");
        let frame = Frame {
            custom_islands: vec![custom_island(list)],
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        let size = surface();
        let plan = RenderPlan::from_frame(&frame, size).expect("a plan");
        let (x, _, w, _) = window_rects(&plan, size)[0];
        // Window x = 100 + 150 = 250; the island ends at 300, so 50 of the 200 survives.
        assert!((x - 250.0).abs() < 0.01, "x: {x}");
        assert!((w - 50.0).abs() < 0.01, "w: {w}");
    }

    #[test]
    fn content_fully_outside_the_viewport_draws_nothing() {
        let list = DisplayList::builder()
            .rect(Rect::new(500.0, 0.0, 100.0, 48.0), [60, 90, 200, 255])
            .finish()
            .expect("valid");
        let frame = Frame {
            custom_islands: vec![custom_island(list)],
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        let plan = RenderPlan::from_frame(&frame, surface()).expect("a plan");
        assert!(plan.is_empty(), "nothing visible means nothing drawn");
    }

    #[test]
    fn the_lists_own_clips_narrow_the_viewport_further() {
        let list = DisplayList::builder()
            .clip_push(Rect::new(0.0, 0.0, 80.0, 48.0))
            .rect(Rect::new(10.0, 0.0, 200.0, 48.0), [60, 90, 200, 255])
            .clip_pop()
            .finish()
            .expect("valid");
        let frame = Frame {
            custom_islands: vec![custom_island(list)],
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        let size = surface();
        let plan = RenderPlan::from_frame(&frame, size).expect("a plan");
        let (x, _, w, _) = window_rects(&plan, size)[0];
        // Window x = 110; the clip ends at content 80 -> window 180, so width 70.
        assert!((x - 110.0).abs() < 0.01, "x: {x}");
        assert!((w - 70.0).abs() < 0.01, "w: {w}");
    }

    #[test]
    fn hit_regions_draw_nothing() {
        let list = DisplayList::builder()
            .hit_region(
                "playhead",
                Rect::new(0.0, 0.0, 10.0, 48.0),
                "playhead",
                RegionRole::Handle,
            )
            .finish()
            .expect("valid");
        let frame = Frame {
            custom_islands: vec![custom_island(list)],
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        let plan = RenderPlan::from_frame(&frame, surface()).expect("a plan");
        assert!(plan.is_empty(), "a hit region is input geometry, not paint");
    }

    #[test]
    fn text_and_blits_are_refused_loudly_until_their_passes_land() {
        let text_list = DisplayList::builder()
            .text(
                LogicalPoint::new(0.0, 12.0),
                "clip name",
                11.0,
                [255, 255, 255, 255],
            )
            .finish()
            .expect("valid");
        let frame = Frame {
            custom_islands: vec![custom_island(text_list)],
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        let error = RenderPlan::from_frame(&frame, surface()).expect_err("text refuses");
        assert_eq!(
            error,
            RenderError::UnrenderableContent {
                island: 0,
                kind: "text run"
            }
        );

        let blit_list = DisplayList::builder()
            .blit(Rect::new(0.0, 0.0, 64.0, 36.0), "thumb:1")
            .finish()
            .expect("valid");
        let frame = Frame {
            custom_islands: vec![custom_island(blit_list)],
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        let error = RenderPlan::from_frame(&frame, surface()).expect_err("blits refuse");
        assert_eq!(
            error,
            RenderError::UnrenderableContent {
                island: 0,
                kind: "bitmap blit"
            }
        );
    }

    #[test]
    fn a_blit_with_support_but_no_store_refuses_loudly() {
        let Some(cache) = font_cache() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut atlas = crate::text_layout::ContentAtlas::new();
        let blit_list = DisplayList::builder()
            .blit(Rect::new(0.0, 0.0, 64.0, 36.0), "thumb:1")
            .finish()
            .expect("valid");
        let frame = Frame {
            custom_islands: vec![custom_island(blit_list)],
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        // Text support but no store: the wiring defect is named, not silent.
        let error = RenderPlan::from_frame_text(&frame, surface(), &cache, &mut atlas)
            .expect_err("a blit with no store refuses");
        assert_eq!(
            error,
            RenderError::UnrenderableContent {
                island: 0,
                kind: "bitmap blit without a bitmap store"
            }
        );
    }

    #[test]
    fn a_blit_whose_key_is_missing_draws_nothing_but_refuses_nothing() {
        let Some(cache) = font_cache() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut atlas = crate::text_layout::ContentAtlas::new();
        let store = crate::bitmap::BitmapStore::new();
        let blit_list = DisplayList::builder()
            .blit(Rect::new(0.0, 0.0, 64.0, 36.0), "thumb:not-decoded-yet")
            .finish()
            .expect("valid");
        let frame = Frame {
            custom_islands: vec![custom_island(blit_list)],
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        let plan =
            RenderPlan::from_frame_content(&frame, surface(), &cache, &mut atlas, Some(&store))
                .expect("a missing key is the transparent well, not an error");
        assert!(
            plan.vertices.is_empty(),
            "the well draws nothing: {} vertices",
            plan.vertices.len()
        );
    }

    #[test]
    fn a_present_blit_expands_to_one_textured_quad() {
        let Some(cache) = font_cache() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut atlas = crate::text_layout::ContentAtlas::new();
        let mut store = crate::bitmap::BitmapStore::new();
        assert!(store.insert("thumb:1", 4, 4, vec![200u8; 64]));
        let blit_list = DisplayList::builder()
            .blit(Rect::new(10.0, 10.0, 32.0, 18.0), "thumb:1")
            .finish()
            .expect("valid");
        let frame = Frame {
            custom_islands: vec![custom_island(blit_list)],
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        };
        let plan =
            RenderPlan::from_frame_content(&frame, surface(), &cache, &mut atlas, Some(&store))
                .expect("the blit expands");
        assert_eq!(plan.vertices.len(), 4, "one blit is one quad");
        assert_eq!(plan.indices.len(), 6);
        // The vertex colour is white: the bitmap's pixels are the colour.
        for vertex in &plan.vertices {
            assert_eq!(vertex.colour, [255, 255, 255, 255]);
        }
        // And the quad spans a uv rectangle — the bitmap's packing — not a point.
        let spread = (plan.vertices[1].uv[0] - plan.vertices[0].uv[0]).abs();
        assert!(spread > 0.0, "the quad samples a span of the atlas");
    }
    /// A cache over the system font, or `None` on a machine without one — the tests
    /// that need it skip rather than fail, because a fontless CI box is a machine
    /// truth, not a defect.
    fn font_cache() -> Option<crate::atlas::GlyphCache> {
        crate::text::default_font()
            .ok()
            .map(crate::atlas::GlyphCache::new)
    }

    fn text_frame(text: &str, origin: (f32, f32)) -> Frame {
        let list = DisplayList::builder()
            .text(
                LogicalPoint {
                    x: origin.0,
                    y: origin.1,
                },
                text,
                14.0,
                [230, 230, 235, 255],
            )
            .finish()
            .expect("valid");
        Frame {
            custom_islands: vec![custom_island(list)],
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        }
    }

    #[test]
    fn text_runs_expand_to_textured_quads() {
        let Some(cache) = font_cache() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut atlas = crate::text_layout::ContentAtlas::new();
        let frame = text_frame("HH", (4.0, 24.0));
        let plan = RenderPlan::from_frame_text(&frame, surface(), &cache, &mut atlas)
            .expect("text expands");
        assert!(
            plan.vertices.len() >= 8,
            "two glyphs are two quads, got {} vertices",
            plan.vertices.len()
        );
        let white = crate::text_layout::ContentAtlas::white_uv();
        let mut textured = 0;
        for quad in plan.vertices.chunks(4) {
            // A glyph quad's corners span a uv rectangle rather than sitting on one texel.
            let spread =
                (quad[2].uv[0] - quad[0].uv[0]).abs() + (quad[2].uv[1] - quad[0].uv[1]).abs();
            if spread > 0.0 {
                textured += 1;
            }
            for vertex in quad {
                assert_ne!(
                    vertex.uv,
                    [f32::NAN; 2],
                    "no vertex may carry a non-finite uv"
                );
            }
        }
        let _ = white;
        assert!(
            textured >= 2,
            "the glyph quads are textured, got {textured}"
        );
        assert!(atlas.packed() >= 1, "expansion packed the glyphs it placed");
    }

    #[test]
    fn text_positions_follow_the_island_transform() {
        let Some(cache) = font_cache() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut atlas = crate::text_layout::ContentAtlas::new();
        let frame = text_frame("H", (4.0, 24.0));
        let plan = RenderPlan::from_frame_text(&frame, surface(), &cache, &mut atlas)
            .expect("text expands");
        let rects = window_rects(&plan, surface());
        assert!(!rects.is_empty(), "one glyph is at least one quad");
        let (x, y, _w, _h) = rects[0];
        // The island sits at (100,100) with no scroll; the run origin is content (4,24),
        // so the first glyph's quad must start at or after window x = 104.
        assert!(
            x >= 104.0,
            "the glyph x {x} is window-space, not content-space"
        );
        assert!(y >= 100.0, "the glyph y {y} is inside the island's band");
    }

    #[test]
    fn clipped_text_interpolates_its_uvs() {
        let Some(cache) = font_cache() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut atlas = crate::text_layout::ContentAtlas::new();
        // A run whose origin is outside the viewport's left edge: the island's viewport
        // clips the first glyphs, and the survivors' uvs must be the *interior* of the
        // coverage, not its edge — clipping without uv interpolation would stretch it.
        let frame = text_frame("HHHH", (-20.0, 24.0));
        let plan = RenderPlan::from_frame_text(&frame, surface(), &cache, &mut atlas)
            .expect("text expands");
        let rects = window_rects(&plan, surface());
        assert!(!rects.is_empty(), "some of the run survives the clip");
        for (x, _y, w, _h) in &rects {
            assert!(
                *x >= 100.0 - 0.01,
                "nothing draws left of the viewport: x={x}"
            );
            assert!(*w > 0.0);
        }
        // The first surviving quad is a partial glyph: its left uv is strictly inside
        // the coverage's uv span. Full-glyph quads start at their entry's left edge;
        // a clipped one starts further right.
        let first_uvs: Vec<[f32; 2]> = plan
            .vertices
            .chunks(4)
            .next()
            .unwrap()
            .iter()
            .map(|v| v.uv)
            .collect();
        let left_u = first_uvs[0][0];
        let right_u = first_uvs[1][0];
        assert!(
            right_u > left_u,
            "a partially-clipped quad still samples a positive span: {first_uvs:?}"
        );
    }

    #[test]
    fn text_fully_clipped_away_draws_nothing() {
        let Some(cache) = font_cache() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut atlas = crate::text_layout::ContentAtlas::new();
        // The whole run sits left of the viewport: no quad may be emitted for it.
        let frame = text_frame("HHHH", (-500.0, 24.0));
        let plan = RenderPlan::from_frame_text(&frame, surface(), &cache, &mut atlas)
            .expect("text expands");
        assert!(
            plan.vertices.is_empty(),
            "a run outside the viewport emits no vertices, got {}",
            plan.vertices.len()
        );
    }
}
