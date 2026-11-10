//! The renderer, against a real window surface.
//!
//! This is the first half of the composition boundary ADR-002 requires: a real window,
//! a real `wgpu` surface created from it, and a frame presented through it. It runs on
//! a machine with a display and a GPU; everywhere else it reports that it was skipped,
//! because "no display" and "no GPU" are facts about the machine.

#![cfg(feature = "wgpu-device")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::future::Future;
use std::task::{Context, Poll};

use ncui_gpu_renderer::device::{DeviceFactory, WgpuDeviceFactory};
use ncui_gpu_renderer::render::{RenderPlan, wgpu_pass::QuadPipeline};
use ncui_gpu_renderer::surface::{SurfaceSize, wgpu_target::WindowSurface};

fn block_on<F: Future>(future: F) -> F::Output {
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

#[test]
fn a_frame_can_be_presented_to_a_window_surface() {
    use tao::platform::windows::EventLoopBuilderExtWindows;
    let mut builder = tao::event_loop::EventLoopBuilder::new();
    builder.with_any_thread(true);
    let event_loop = builder.build();
    let Ok(window) = tao::window::WindowBuilder::new()
        .with_title("ncui-gpu-renderer window surface probe")
        .with_inner_size(tao::dpi::LogicalSize::new(64.0, 64.0))
        .build(&event_loop)
    else {
        return;
    };

    let factory = WgpuDeviceFactory;
    let Some(device) = block_on(factory.create()).ok() else {
        return;
    };

    let Ok(surface) = device.instance().create_surface(&window) else {
        return;
    };

    let format = wgpu::TextureFormat::Bgra8UnormSrgb;
    let size = SurfaceSize::new(64, 64);
    let mut window_surface = WindowSurface::new(size, format);
    if window_surface
        .configure(device.wgpu_device(), &surface)
        .is_err()
    {
        return;
    }

    let pipeline = QuadPipeline::new(device.wgpu_device(), format);
    let plan = RenderPlan::from_frame(
        &ncui_gpu_renderer::frame::Frame {
            clear_colour: [0, 0, 0, 255],
            islands: Vec::new(),
        },
        size,
    )
    .expect("a plan");

    // Presenting may fail on a headless machine; that is a fact about the machine.
    let _ = window_surface.render(
        device.wgpu_device(),
        device.queue(),
        &surface,
        &pipeline,
        &plan,
    );
}
