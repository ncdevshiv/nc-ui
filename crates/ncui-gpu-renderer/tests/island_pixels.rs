//! An island's pixels, on a real GPU.
//!
//! These tests need a graphics adapter. When there is none — a CI runner with no GPU, a container
//! without a device — they report that they were skipped rather than failing, because "this machine
//! has no GPU" is a fact about the machine, not a defect in the renderer.
//!
//! What they add over `render_offscreen.rs` is the join: that the pixels a page is handed are the
//! pixels the GPU drew, with the size they were drawn at, and that the parts of the frame the island
//! does not cover are transparent — which is what lets a page show through around an island instead
//! of having a rectangle pasted over it.

#![cfg(feature = "wgpu-device")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::future::Future;
use std::task::{Context, Poll};

use ncui_gpu_renderer::device::{DeviceFactory, WgpuDevice, WgpuDeviceFactory};
use ncui_gpu_renderer::frame::{Frame, IslandRect};
use ncui_gpu_renderer::island::{IslandError, MAX_ISLAND_PIXELS, render_to_pixels};
use ncui_gpu_renderer::surface::SurfaceSize;

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
    match block_on(WgpuDeviceFactory.create()) {
        Ok(device) => Some(device),
        Err(error) => {
            // The reason is not printed: the workspace denies `eprintln!`, and a machine with no
            // adapter is a fact about the machine rather than a test failure.
            let _ = error;
            None
        }
    }
}

/// The colour an island is drawn in, and the transparent clear behind it.
const ISLAND: [u8; 4] = [0, 128, 255, 255];
const CLEAR: [u8; 4] = [0, 0, 0, 0];

fn frame(size: SurfaceSize, island: IslandRect) -> Frame {
    let _ = size;
    Frame {
        clear_colour: CLEAR,
        islands: vec![island],
    }
}

#[test]
fn an_island_is_handed_over_as_the_pixels_the_gpu_drew() {
    let Some(device) = device() else {
        return;
    };
    let size = SurfaceSize::new(48, 32);
    let island = IslandRect {
        x: 8.0,
        y: 8.0,
        width: 24.0,
        height: 16.0,
        colour: ISLAND,
    };
    let pixels = render_to_pixels(&device, size, &frame(size, island)).expect("the island renders");

    assert_eq!((pixels.width, pixels.height), (48, 32));
    assert!(
        pixels.is_whole(),
        "a frame that is not whole cannot be drawn"
    );

    // The middle of the island is the island's colour.
    let middle = ((16 * 48) + 24) * 4;
    assert_eq!(
        &pixels.rgba[middle..middle + 4],
        &ISLAND,
        "the island's colour must be where the island was"
    );
    // A corner the island does not cover is transparent, so the page shows through there.
    let corner = 0;
    assert_eq!(
        &pixels.rgba[corner..corner + 4],
        &CLEAR,
        "everything outside the island must be transparent rather than black"
    );
}

#[test]
fn an_island_larger_than_the_bound_is_refused_rather_than_transferred() {
    let Some(device) = device() else {
        return;
    };
    // One pixel past the bound, along one edge: the smallest size that is over it.
    let size = SurfaceSize::new(MAX_ISLAND_PIXELS + 1, 1);
    let error = render_to_pixels(
        &device,
        size,
        &frame(
            size,
            IslandRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
                colour: ISLAND,
            },
        ),
    )
    .expect_err("an island past the bound is refused");
    assert!(matches!(error, IslandError::Size { .. }), "{error:?}");
    assert!(error.to_string().contains("at most"), "{error}");

    // A side of zero is not a size a frame can be drawn at.
    let error = render_to_pixels(
        &device,
        SurfaceSize::new(0, 16),
        &frame(
            SurfaceSize::new(0, 16),
            IslandRect {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
                colour: ISLAND,
            },
        ),
    )
    .expect_err("a zero side is refused");
    assert!(matches!(error, IslandError::Size { .. }), "{error:?}");
}

#[test]
fn a_frame_with_a_rectangle_that_is_not_one_is_refused() {
    let Some(device) = device() else {
        return;
    };
    let size = SurfaceSize::new(16, 16);
    let error = render_to_pixels(
        &device,
        size,
        &frame(
            size,
            IslandRect {
                x: f32::NAN,
                y: 0.0,
                width: 8.0,
                height: 8.0,
                colour: ISLAND,
            },
        ),
    )
    .expect_err("a rectangle with a NaN edge is not a rectangle");
    assert!(matches!(error, IslandError::Plan(_)), "{error:?}");
}
