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
//! Two `f32` positions and four `u8` colour channels: twelve bytes per vertex. Small
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
}

impl Vertex {
    /// The size of one vertex on the wire.
    pub const WIRE_BYTES: usize = 12;
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
}

impl core::fmt::Display for RenderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidRectangle { index, reason } => {
                write!(f, "island {index} is not a rectangle: {reason}")
            }
        }
    }
}

impl std::error::Error for RenderError {}

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
            let base = (vertices.len() / 4) as u32 * 4;
            // The frame's coordinates are in physical pixels with the origin at the top
            // left. The clip space the GPU expects has its origin at the centre and its y
            // axis pointing up, so the conversion is a flip and a scale.
            let to_clip = |x: f32, y: f32| -> (f32, f32) {
                ((x / width) * 2.0 - 1.0, 1.0 - (y / height) * 2.0)
            };
            let (left, top) = to_clip(island.x, island.y);
            let (right, bottom) = to_clip(island.x + island.width, island.y + island.height);
            let colour = island.colour;
            vertices.push(Vertex {
                x: left,
                y: top,
                colour,
            });
            vertices.push(Vertex {
                x: right,
                y: top,
                colour,
            });
            vertices.push(Vertex {
                x: right,
                y: bottom,
                colour,
            });
            vertices.push(Vertex {
                x: left,
                y: bottom,
                colour,
            });
            indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
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
        #[must_use]
        pub fn new(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Self {
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("ncui-gpu-renderer-quad"),
                source: wgpu::ShaderSource::Wgsl(
                    r"
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) colour: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) colour: vec4<f32>,
}

@vertex
fn vertex_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    // The positions arrive in clip space: the conversion from physical pixels is done on
    // the CPU, where the size of the target is known.
    out.position = vec4<f32>(input.position, 0.0, 1.0);
    out.colour = input.colour;
    return out;
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return input.colour;
}
"
                    .into(),
                ),
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("ncui-gpu-renderer-layout"),
                bind_group_layouts: &[],
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
            Self { pipeline, layout }
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
    reason = "test assertions must abort loudly on failure"
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
            clear_colour: [0, 0, 0, 255],
            islands: vec![island(0.0, 0.0, 10.0, 20.0)],
        };
        let plan = RenderPlan::from_frame(&frame, surface()).expect("a plan");
        assert_eq!(plan.vertices.len(), 4);
        assert_eq!(plan.indices.len(), 6);
        assert_eq!(plan.vertex_bytes(), 48);
    }

    #[test]
    fn a_quads_corners_are_the_rectangles_corners() {
        // The top-left of the surface is the top-left in clip space: x is -1 and y is +1.
        let frame = Frame {
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
            clear_colour: [1, 2, 3, 4],
            islands: Vec::new(),
        };
        let plan = RenderPlan::from_frame(&frame, surface()).expect("a plan");
        assert_eq!(plan.clear_colour, [1, 2, 3, 4]);
    }
}
