//! Text on a real GPU: shaped, packed, drawn, read back as pixels.
//!
//! The pure tests prove the plan; this proves the join — that coverage rasterised on the
//! CPU arrives in the texture the fragment shader samples, and that what lands on the
//! target is the glyph's shape and not a rectangle where a letter should be. A rectangle
//! would pass a test that only checked "some pixels lit in the run's box", which is why
//! the assertions here use the shape of the letters themselves.
//!
//! These tests need a graphics adapter; on a machine without one they skip rather than
//! fail, the same contract `island_pixels.rs` states.

#![cfg(feature = "wgpu-device")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    reason = "test assertions must abort loudly on failure; GPU-less-machine skips report on stderr"
)]

use std::future::Future;
use std::task::{Context, Poll};

use ncui_gpu::display::DisplayList;
use ncui_gpu::geometry::{LogicalPoint, Rect};
use ncui_gpu_renderer::atlas::GlyphCache;
use ncui_gpu_renderer::device::{DeviceFactory, WgpuDevice, WgpuDeviceFactory};
use ncui_gpu_renderer::frame::{CustomIsland, Frame};
use ncui_gpu_renderer::render::RenderPlan;
use ncui_gpu_renderer::render::wgpu_pass::QuadPipeline;
use ncui_gpu_renderer::surface::SurfaceSize;
use ncui_gpu_renderer::surface::wgpu_target::OffscreenTarget;
use ncui_gpu_renderer::text::default_font;
use ncui_gpu_renderer::text_layout::ContentAtlas;

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

fn device() -> Option<WgpuDevice> {
    match block_on(WgpuDeviceFactory.create()) {
        Ok(device) => Some(device),
        Err(error) => {
            assert!(
                std::env::var_os("NCUI_REQUIRE_GPU").is_none(),
                "GPU evidence was required, but no graphics adapter is available: {error}"
            );
            eprintln!("no GPU on this machine; skipping");
            None
        }
    }
}

/// The pixel at `(x, y)` of a readback, as rgba.
fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * width + x) * 4) as usize;
    [rgba[at], rgba[at + 1], rgba[at + 2], rgba[at + 3]]
}

/// Renders one frame with text support and reads the pixels back.
fn render(frame: &Frame, size: SurfaceSize) -> Option<Vec<u8>> {
    render_with_store(frame, size, None)
}

/// Renders one frame with full content support and reads the pixels back.
fn render_with_store(
    frame: &Frame,
    size: SurfaceSize,
    store: Option<&ncui_gpu_renderer::bitmap::BitmapStore>,
) -> Option<Vec<u8>> {
    let device = device()?;
    let cache = GlyphCache::new(default_font().expect("a system font"));
    let mut atlas = ContentAtlas::new();
    let plan = RenderPlan::from_frame_content(frame, size, &cache, &mut atlas, store)
        .expect("the plan expands");
    let target = OffscreenTarget::new(device.wgpu_device(), size);
    let pipeline = QuadPipeline::new(
        device.wgpu_device(),
        device.queue(),
        wgpu::TextureFormat::Rgba8Unorm,
    );
    pipeline.sync_atlas(device.queue(), &mut atlas);
    pipeline.draw(device.wgpu_device(), device.queue(), target.view(), &plan);
    target.read_back(device.wgpu_device(), device.queue()).ok()
}

fn text_frame(text: &str, origin: (f32, f32), colour: [u8; 4], size_px: f32) -> Frame {
    let list = DisplayList::builder()
        .text(
            LogicalPoint {
                x: origin.0,
                y: origin.1,
            },
            text,
            size_px,
            colour,
        )
        .finish()
        .expect("valid");
    Frame {
        clear_colour: [0, 0, 0, 0],
        islands: Vec::new(),
        custom_islands: vec![CustomIsland {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 100.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            list,
        }],
    }
}

#[test]
fn a_drawn_letter_is_its_shape_not_a_rectangle() {
    let size = SurfaceSize::new(200, 100);
    let Some(rgba) = render(
        &text_frame("H", (20.0, 80.0), [255, 255, 255, 255], 64.0),
        size,
    ) else {
        return;
    };
    // Scan the run's box and count lit pixels column by column. An H is two vertical
    // strokes joined by a crossbar: the columns of the strokes are lit most of their
    // height, and the columns between them are lit only where the crossbar runs — so a
    // column histogram is two plateaus with a valley between them. A rectangle has no
    // valley.
    let mut lit_rows_per_column: Vec<u32> = vec![0; 200];
    for x in 0..200u32 {
        for y in 0..100u32 {
            if pixel(&rgba, 200, x, y)[3] > 40 {
                lit_rows_per_column[x as usize] += 1;
            }
        }
    }
    let lit_columns: Vec<usize> = (0..200).filter(|x| lit_rows_per_column[*x] > 0).collect();
    assert!(
        lit_columns.len() > 10,
        "an H at 64px is a wide letter: {} lit columns",
        lit_columns.len()
    );
    let first = *lit_columns.first().unwrap();
    let last = *lit_columns.last().unwrap();
    let span = last - first;
    let third = span / 3;
    let stroke_max = lit_rows_per_column[first..first + third]
        .iter()
        .max()
        .copied()
        .unwrap_or(0);
    let middle_min = lit_rows_per_column[first + third..last - third]
        .iter()
        .min()
        .copied()
        .unwrap_or(0);
    assert!(
        stroke_max > middle_min + middle_min.max(1),
        "the strokes are taller than the gap between them: stroke {stroke_max}, middle {middle_min} — a rectangle would not be"
    );
}

#[test]
fn the_interior_of_a_closed_letter_stays_dark() {
    let size = SurfaceSize::new(200, 100);
    let Some(rgba) = render(
        &text_frame("O", (20.0, 80.0), [255, 255, 255, 255], 64.0),
        size,
    ) else {
        return;
    };
    // The counter — the hole — of a closed letter is the one place inside the letter's
    // box the coverage never reaches. Find it by scanning the middle rows for a dark
    // run between lit runs, which a solid rectangle cannot produce.
    let mut found_counter = false;
    for y in 30..70u32 {
        let mut runs: Vec<bool> = Vec::new();
        for x in 20..160u32 {
            runs.push(pixel(&rgba, 200, x, y)[3] > 40);
        }
        // Compress the row into lit/dark transitions and look for lit-dark-lit.
        let mut pattern = String::new();
        let mut last = false;
        for lit in runs {
            if lit != last {
                pattern.push(if lit { '#' } else { '.' });
                last = lit;
            }
        }
        if pattern.contains("#.#") {
            found_counter = true;
            break;
        }
    }
    assert!(
        found_counter,
        "some row through the letter's middle shows the dark counter between the strokes"
    );
}

#[test]
fn text_clipped_by_the_viewport_draws_nothing_outside_it() {
    // The island covers only the right half of the target; the run starts far left of
    // it. Nothing may be lit in the left half — the same clipping the rects get, proven
    // through the texture path.
    let size = SurfaceSize::new(200, 100);
    let list = DisplayList::builder()
        .text(
            LogicalPoint { x: -60.0, y: 80.0 },
            "HHHH",
            64.0,
            [255, 255, 255, 255],
        )
        .finish()
        .expect("valid");
    let frame = Frame {
        clear_colour: [0, 0, 0, 0],
        islands: Vec::new(),
        custom_islands: vec![CustomIsland {
            x: 100.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            list,
        }],
    };
    let Some(rgba) = render(&frame, size) else {
        return;
    };
    for x in 0..100u32 {
        for y in 0..100u32 {
            assert!(
                pixel(&rgba, 200, x, y)[3] <= 40,
                "pixel ({x},{y}) is outside the island but lit: {:?}",
                pixel(&rgba, 200, x, y)
            );
        }
    }
    // And something must be lit inside it: the clip intersected the run rather than
    // swallowing it whole.
    let mut lit = 0u32;
    for x in 100..200u32 {
        for y in 0..100u32 {
            if pixel(&rgba, 200, x, y)[3] > 40 {
                lit += 1;
            }
        }
    }
    assert!(
        lit > 0,
        "the surviving part of the run draws inside the viewport"
    );
}

#[test]
fn the_text_colour_is_the_commands_colour() {
    let size = SurfaceSize::new(200, 100);
    let Some(rgba) = render(&text_frame("H", (20.0, 80.0), [255, 0, 0, 255], 64.0), size) else {
        return;
    };
    // Alpha blending over the black clear makes a pixel's red channel its coverage —
    // src.rgb x src.alpha — so the honest invariant is the ratio: red tracks alpha
    // exactly, and green stays zero, because coverage modulates alpha and nothing else.
    let mut checked = false;
    for x in 0..200u32 {
        for y in 0..100u32 {
            let px = pixel(&rgba, 200, x, y);
            if px[3] > 40 {
                let drift = (i32::from(px[0]) - i32::from(px[3])).abs();
                assert!(
                    drift <= 2,
                    "red {px0} tracks alpha {px3} at ({x},{y})",
                    px0 = px[0],
                    px3 = px[3]
                );
                assert_eq!(px[1], 0, "green is untouched by coverage at ({x},{y})");
                checked = true;
            }
        }
    }
    assert!(checked, "some pixel of the letter is lit");
}

#[test]
fn a_blit_draws_the_bitmaps_own_pixels() {
    // A bitmap split vertically: red on the left, green on the right. If what lands is
    // the bitmap's content rather than a solid rectangle, the two halves read back in
    // their own colours — and if the blit scales honestly, a 4-pixel source fills the
    // whole 100-pixel bounds.
    let mut store = ncui_gpu_renderer::bitmap::BitmapStore::new();
    let mut rgba = Vec::new();
    for _y in 0..4 {
        for x in 0..4 {
            if x < 2 {
                rgba.extend_from_slice(&[255, 0, 0, 255]);
            } else {
                rgba.extend_from_slice(&[0, 255, 0, 255]);
            }
        }
    }
    assert!(store.insert("thumb:split", 4, 4, rgba));
    let list = DisplayList::builder()
        .blit(Rect::new(0.0, 0.0, 100.0, 50.0), "thumb:split")
        .finish()
        .expect("valid");
    let frame = Frame {
        clear_colour: [0, 0, 0, 0],
        islands: Vec::new(),
        custom_islands: vec![CustomIsland {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 100.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            list,
        }],
    };
    let size = SurfaceSize::new(200, 100);
    let Some(pixels) = render_with_store(&frame, size, Some(&store)) else {
        return;
    };
    // x=25 is deep in the red half, x=75 deep in the green — both inside the blit's
    // 100x50 bounds at the island's origin.
    let red = pixel(&pixels, 200, 25, 25);
    let green = pixel(&pixels, 200, 75, 25);
    assert!(red[0] > 150 && red[1] < 60, "the left half is red: {red:?}");
    assert!(
        green[1] > 150 && green[0] < 60,
        "the right half is green: {green:?}"
    );
    // And outside the blit's bounds — below y=50 — nothing draws.
    let below = pixel(&pixels, 200, 25, 75);
    assert_eq!(below, [0, 0, 0, 0], "below the blit stays transparent");
}

#[test]
fn a_blit_replacement_draws_the_new_pixels() {
    // The store's generational keys exist so a replacement never samples the old
    // packing: insert red, replace with blue, and what draws is blue.
    let mut store = ncui_gpu_renderer::bitmap::BitmapStore::new();
    assert!(store.insert(
        "thumb:swap",
        2,
        2,
        vec![
            255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255
        ]
    ));
    assert!(store.insert(
        "thumb:swap",
        2,
        2,
        vec![
            0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255
        ]
    ));
    let list = DisplayList::builder()
        .blit(Rect::new(0.0, 0.0, 40.0, 40.0), "thumb:swap")
        .finish()
        .expect("valid");
    let frame = Frame {
        clear_colour: [0, 0, 0, 0],
        islands: Vec::new(),
        custom_islands: vec![CustomIsland {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 100.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            list,
        }],
    };
    let size = SurfaceSize::new(200, 100);
    let Some(pixels) = render_with_store(&frame, size, Some(&store)) else {
        return;
    };
    let center = pixel(&pixels, 200, 20, 20);
    assert!(
        center[2] > 150 && center[0] < 60,
        "the replacement draws: {center:?}"
    );
}
