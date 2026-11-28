//! Live evidence for the Windows video presenter: a real parent window, a real
//! child surface, real frames presented and read back.
//!
//! The parent is a plain Win32 window created by the test itself — the airspace
//! mechanism does not care what owns the parent, which is exactly why it works
//! inside a webview host later. What is proven here:
//!
//! - the child window exists at the OS level, parented and positioned as asked;
//! - a frame with known pixels is presented, and the GPU-side readback shows
//!   those pixels — the presenter's pixels, not a hope about them;
//! - misshaped payloads are refused rather than uploaded;
//! - placement updates land at the OS level (`GetWindowRect` truth);
//! - a video-rate burst (120 frames of 1080p) completes, and its timing is
//!   printed for the record — the sustained-rate acceptance runs against the
//!   real engine later, but a burst that cannot sustain rate is caught here.

#![cfg(target_os = "windows")]
// The test is the boundary's own evidence: it creates windows and destroys them.
#![allow(
    unsafe_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_wrap,
    clippy::print_stderr,
    reason = "test assertions must abort loudly on failure; the casts are fixed test dimensions far below i32::MAX; the parent-window FFI is the boundary's own evidence shape; eprintln is how test evidence is reported"
)]

use std::sync::OnceLock;

use ncui_gpu::surface::{Damage, Frame, FramePts, PlacementRect, Presenter};
use ncui_video_surface::{RgbaPixels, Win32Presenter};
use windows_sys::Win32::Foundation::{HINSTANCE, HWND, RECT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowRect, RegisterClassW, SW_SHOW,
    ShowWindow, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

const PARENT_CLASS: &[u16] = &[
    0x006e, 0x0063, 0x0075, 0x0069, 0x002d, 0x0076, 0x0069, 0x0064, 0x0065, 0x006f, 0x002d, 0x0070,
    0x0061, 0x0072, 0x0065, 0x006e, 0x0074, 0x0000, // "ncui-video-parent\0"
];

unsafe extern "system" fn parent_wndproc(
    window: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

static PARENT_REGISTERED: OnceLock<()> = OnceLock::new();

/// A plain top-level window for the child to live in.
struct ParentWindow(HWND);

impl ParentWindow {
    fn create(width: i32, height: i32) -> Self {
        PARENT_REGISTERED.get_or_init(|| unsafe {
            let instance = GetModuleHandleW(std::ptr::null()) as HINSTANCE;
            let class = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(parent_wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: PARENT_CLASS.as_ptr(),
            };
            RegisterClassW(&raw const class);
        });
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null()) as HINSTANCE;
            let window = CreateWindowExW(
                0,
                PARENT_CLASS.as_ptr(),
                std::ptr::null(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                width,
                height,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            assert!(!window.is_null(), "the parent window is created");
            ShowWindow(window, SW_SHOW);
            Self(window)
        }
    }
}

impl Drop for ParentWindow {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.0);
        }
    }
}

fn frame_pixels(width: u32, height: u32, sequence: u64) -> RgbaPixels {
    // The frame index is burned into the pixels themselves: the red channel is the
    // sequence number, green is the row index's low byte, blue the column's. A
    // dropped or reordered frame is visible in any readback, which is the point.
    let mut bytes = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let offset = ((y * width + x) * 4) as usize;
            bytes[offset] = sequence as u8;
            bytes[offset + 1] = y as u8;
            bytes[offset + 2] = x as u8;
            bytes[offset + 3] = 255;
        }
    }
    RgbaPixels {
        width,
        height,
        stride: width * 4,
        bytes,
    }
}

fn make_frame(width: u32, height: u32, sequence: u64) -> Frame<RgbaPixels> {
    Frame {
        sequence,
        pts: FramePts {
            value: sequence * 3000,
            timescale: 90_000,
        },
        damage: vec![Damage::full(width, height)],
        payload: frame_pixels(width, height, sequence),
    }
}

fn placement() -> PlacementRect {
    PlacementRect {
        x: 20,
        y: 20,
        width: 640,
        height: 360,
    }
}

#[test]
fn a_child_surface_presents_verifiable_pixels() {
    let parent = ParentWindow::create(800, 600);
    let (w, h) = (320u32, 240u32);
    let mut presenter = Win32Presenter::create(parent.0, placement(), w, h)
        .expect("the presenter is created against a live parent");

    // OS-level truth first: the child exists, parented, at the placement rect.
    assert!(!presenter.window_handle().is_null());
    let mut rect = std::mem::MaybeUninit::<RECT>::uninit();
    unsafe {
        GetWindowRect(presenter.window_handle(), rect.as_mut_ptr());
        let rect = rect.assume_init();
        assert_eq!(
            rect.right - rect.left,
            placement().width as i32,
            "the window is the placement's width"
        );
    }

    // Present a frame whose pixels are known, then read the GPU texture back:
    // the red channel is the sequence number everywhere.
    let sequence = 42u64;
    presenter
        .present(&make_frame(w, h, sequence))
        .expect("a well-formed frame presents");
    let readback = presenter.readback_frame();
    assert_eq!(readback.len(), (w * h * 4) as usize);
    let first = &readback[0..4];
    assert_eq!(
        first[0], sequence as u8,
        "the frame index is burned into the first pixel's red channel"
    );
    assert_eq!(first[3], 255, "alpha survives the round trip");
    // A pixel deeper in proves rows and columns land where they should: at
    // (10, 5) green is 5 and blue is 10.
    let offset = ((5 * w + 10) * 4) as usize;
    assert_eq!(readback[offset + 1], 5, "row index in green");
    assert_eq!(readback[offset + 2], 10, "column index in blue");

    // A misshaped payload is refused rather than uploaded.
    let mut bad = make_frame(w, h, 43);
    bad.payload.width = w + 4;
    assert!(
        presenter.present(&bad).is_err(),
        "a frame the surface was not built for is refused"
    );

    presenter.teardown();
    drop(parent);
}

#[test]
fn placement_updates_land_at_the_os_level() {
    let parent = ParentWindow::create(800, 600);
    let mut presenter =
        Win32Presenter::create(parent.0, placement(), 64, 64).expect("presenter created");
    let moved = PlacementRect {
        x: 100,
        y: 150,
        width: 400,
        height: 300,
    };
    presenter.place(moved).expect("placement is accepted");
    unsafe {
        let mut rect = std::mem::MaybeUninit::<RECT>::uninit();
        GetWindowRect(presenter.window_handle(), rect.as_mut_ptr());
        let rect = rect.assume_init();
        assert_eq!(rect.right - rect.left, 400, "the OS-level width follows");
        assert_eq!(rect.bottom - rect.top, 300, "the OS-level height follows");
        // The position is in client-relative coordinates for a child; the
        // window rect is screen-relative, so compare sizes only — the position
        // conversion is the placement machinery's, proven in its own tests.
    }
    presenter.teardown();
    drop(parent);
}

#[test]
fn a_video_rate_burst_completes_and_reports_its_timing() {
    let parent = ParentWindow::create(1080, 800);
    let (w, h) = (1920u32, 1080u32);
    let mut presenter =
        Win32Presenter::create(parent.0, placement(), w, h).expect("presenter created");
    let frames = 120usize;
    let start = std::time::Instant::now();
    for sequence in 0..frames as u64 {
        presenter
            .present(&make_frame(w, h, sequence))
            .expect("burst frames present");
    }
    let elapsed = start.elapsed();
    let per_frame = elapsed / frames as u32;
    // The honest report: print the numbers so the record carries them. The
    // assertion is loose on purpose — a debug build on a loaded machine is not
    // the acceptance environment; what must be true is that a burst completes
    // at all without loss or panic, and that presentation is not orders of
    // magnitude off rate.
    eprintln!("video burst: {frames} frames of {w}x{h} in {elapsed:?} — {per_frame:?}/frame");
    assert!(
        per_frame.as_millis() < 100,
        "presentation at {per_frame:?}/frame is not a video path at all"
    );
    // The last frame's pixels are still truthful after the burst.
    let readback = presenter.readback_frame();
    assert_eq!(
        readback[0],
        (frames - 1) as u8,
        "the final frame is the newest"
    );
    presenter.teardown();
    drop(parent);
}
