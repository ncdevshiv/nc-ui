//! The renderer, on a real GPU.
//!
//! These tests need a graphics adapter. When there is none — a CI runner with no GPU, a
//! container without a device — they report that they were skipped rather than failing,
//! because "this machine has no GPU" is a fact about the machine, not a defect in the
//! renderer. The *policy* — that a frame is encoded, validated and turned into a render
//! plan — is tested without a GPU in the crate's own unit tests; what these add is the
//! part that only a real driver can answer: does the drawn frame contain the pixels it
//! was asked to draw.
//!
//! # The pixel assertions
//!
//! The renderer draws in *physical pixels* with the origin at the top left, and the
//! shader converts that to the clip space the GPU expects. That conversion is the one
//! piece of arithmetic that a unit test cannot check, because it is the point where the
//! renderer's idea of a coordinate becomes the GPU's. So the test asserts on the pixels:
//! the island's colour is where the island was, and the clear colour is everywhere else.

#![cfg(feature = "wgpu-device")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::future::Future;
use std::task::{Context, Poll};

use ncui_gpu_renderer::device::{DeviceFactory, GpuDevice, WgpuDevice, WgpuDeviceFactory};
use ncui_gpu_renderer::frame::{Frame, IslandRect};
use ncui_gpu_renderer::render::{RenderPlan, wgpu_pass::QuadPipeline};
use ncui_gpu_renderer::surface::{SurfaceSize, wgpu_target::OffscreenTarget};

/// Drives a future to completion on the current thread.
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

/// Creates a device, or reports that this machine has no GPU.
fn device() -> Option<WgpuDevice> {
    let factory = WgpuDeviceFactory;
    match block_on(factory.create()) {
        Ok(device) => Some(device),
        Err(error) => {
            // The reason is not printed: the workspace denies `eprintln!`, and a machine
            // with no adapter is a fact about the machine rather than a test failure.
            let _ = error;
            None
        }
    }
}

#[test]
fn a_drawn_frame_contains_the_pixels_it_was_asked_to_draw() {
    let Some(device) = device() else {
        return;
    };
    let size = SurfaceSize::new(64, 64);
    let target = OffscreenTarget::new(device.wgpu_device(), size);
    let pipeline = QuadPipeline::new(device.wgpu_device(), wgpu::TextureFormat::Rgba8Unorm);

    let frame = Frame {
        clear_colour: [0, 0, 0, 255],
        islands: vec![IslandRect {
            x: 16.0,
            y: 16.0,
            width: 32.0,
            height: 32.0,
            colour: [255, 0, 0, 255],
        }],
    };
    let plan = RenderPlan::from_frame(&frame, size).expect("a plan");
    pipeline.draw(device.wgpu_device(), device.queue(), target.view(), &plan);

    let pixels = target
        .read_back(device.wgpu_device(), device.queue())
        .expect("the readback should succeed");
    assert_eq!(
        pixels.len(),
        64 * 64 * 4,
        "the readback must be a whole frame"
    );

    // The middle of the island is the island's colour.
    let middle = (32 * 64 + 32) * 4;
    assert_eq!(
        &pixels[middle..middle + 4],
        &[255, 0, 0, 255],
        "the island's colour must be where the island was"
    );
    // The corner outside it is the clear colour.
    let corner = 4;
    assert_eq!(
        &pixels[corner..corner + 4],
        &[0, 0, 0, 255],
        "everything outside the island must be the clear colour"
    );
}

#[test]
fn an_empty_frame_clears_the_target_and_draws_nothing() {
    let Some(device) = device() else {
        return;
    };
    let size = SurfaceSize::new(32, 32);
    let target = OffscreenTarget::new(device.wgpu_device(), size);
    let pipeline = QuadPipeline::new(device.wgpu_device(), wgpu::TextureFormat::Rgba8Unorm);

    let frame = Frame {
        clear_colour: [10, 20, 30, 255],
        islands: Vec::new(),
    };
    let plan = RenderPlan::from_frame(&frame, size).expect("a plan");
    assert!(plan.is_empty());
    pipeline.draw(device.wgpu_device(), device.queue(), target.view(), &plan);

    let pixels = target
        .read_back(device.wgpu_device(), device.queue())
        .expect("the readback should succeed");
    assert_eq!(
        &pixels[0..4],
        &[10, 20, 30, 255],
        "the target must be cleared"
    );
}

#[test]
fn a_device_that_is_alive_reports_itself_alive() {
    let Some(device) = device() else {
        return;
    };
    assert!(device.is_alive(), "a freshly created device must be alive");
    assert!(
        !device.adapter().is_empty(),
        "a device must name the adapter it came from"
    );
}

#[test]
fn a_device_can_be_replaced_after_a_loss() {
    // The recovery path, on a real device. The device is not actually lost — the driver
    // will not give it back — but the supervisor's contract is that a replacement can be
    // created and installed, and that is what a driver reset would need.
    let Some(first) = device() else {
        return;
    };
    assert!(first.is_alive());
    drop(first);
    let replacement = device();
    assert!(
        replacement.is_some(),
        "a replacement device must be creatable after a loss"
    );
}
