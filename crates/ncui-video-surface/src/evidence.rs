//! Verification and diagnostics utilities: the OS-level reading of window and
//! compositor truth.
//!
//! These exist because the airspace architecture's central claim — "the surface
//! composites over the webview's pixels" — cannot be proven from inside the
//! process's own objects. The proof is what the compositor shows, and the
//! compositor's truth is the screen. The same utilities serve live evidence
//! tests and runtime diagnosis alike; they are safe wrappers over the small
//! piece of Win32 that reads window geometry and pixels, and the unsafe inside
//! is the crate's documented boundary at work.
//!
//! Nothing here is fast, and nothing here needs to be: this is the evidence
//! path, not the frame path.

#![allow(
    unsafe_code,
    reason = "window-manager evidence is OS FFI by definition — screen capture, z-order walking, window discovery have no safe API; this module is why the crate is the documented boundary"
)]
#![allow(
    clippy::cast_possible_wrap,
    reason = "the cast sites are fixed 256-element buffer lengths and small window dimensions — the wrap the lint fears is not constructible at these call sites"
)]

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, GetPixel, ReleaseDC, SRCCOPY,
    SelectObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_CHILD, GW_HWNDNEXT, GetClassNameW, GetSystemMetrics, GetWindow, GetWindowRect,
    GetWindowTextW, HWND_TOPMOST, IsWindowVisible, PostMessageW, SM_CXSCREEN, SM_CXVIRTUALSCREEN,
    SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos, WM_COMMAND,
};

/// A rectangle in screen coordinates, as the compositor sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect {
    /// Left edge in screen pixels.
    pub left: i32,
    /// Top edge in screen pixels.
    pub top: i32,
    /// Right edge in screen pixels.
    pub right: i32,
    /// Bottom edge in screen pixels.
    pub bottom: i32,
}

impl ScreenRect {
    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    /// Whether a screen point lies inside the rect.
    #[must_use]
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

/// A composited pixel as read back from the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenPixel {
    /// Red channel.
    pub red: u8,
    /// Green channel.
    pub green: u8,
    /// Blue channel.
    pub blue: u8,
}

/// Reads the screen rectangle of a window in the compositor's own coordinates.
///
/// Answers `None` when the handle is not a live window — a dead surface's
/// geometry is not evidence of anything, and inventing a rect for it would be.
#[must_use]
pub fn window_screen_rect(window: isize) -> Option<ScreenRect> {
    // SAFETY: GetWindowRect accepts any window handle and writes a plain struct;
    // a dangling handle simply fails the call, which is the None branch.
    unsafe {
        let mut rect = std::mem::MaybeUninit::<RECT>::uninit();
        if GetWindowRect(window as HWND, rect.as_mut_ptr()) == 0 {
            return None;
        }
        let rect = rect.assume_init();
        Some(ScreenRect {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        })
    }
}

struct WindowSearch {
    needle: Vec<u16>,
    found: HWND,
}

unsafe extern "system" fn enum_window_proc(window: HWND, param: LPARAM) -> BOOL {
    // SAFETY: `param` is the search struct pointer EnumWindows was given in
    // `find_toplevel_window`, live for the enumeration's duration because the
    // enumeration is synchronous on this thread.
    unsafe {
        let search = &mut *(param as *mut WindowSearch);
        if IsWindowVisible(window) == 0 {
            return 1;
        }
        let mut title = [0u16; 256];
        let len = GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32) as usize;
        let title = &title[..len.min(255)];
        if title
            .windows(search.needle.len())
            .any(|part| part == search.needle.as_slice())
        {
            search.found = window;
            return 0;
        }
        1
    }
}

/// Finds a visible top-level window whose title contains `title_part`, answered
/// as the raw handle integer.
///
/// This is how evidence connects to a window the test did not itself create —
/// the host window belongs to the engine, and the OS is the honest broker of
/// which window is which. Answers `None` when no visible window matches.
#[must_use]
pub fn find_toplevel_window(title_part: &str) -> Option<isize> {
    let needle: Vec<u16> = title_part.encode_utf16().collect();
    let mut search = WindowSearch {
        needle,
        found: std::ptr::null_mut(),
    };
    // SAFETY: the search struct outlives the synchronous enumeration; the
    // callback writes only through the pointer it was given.
    unsafe {
        EnumWindows(Some(enum_window_proc), &raw mut search as isize);
    }
    if search.found.is_null() {
        None
    } else {
        Some(search.found as isize)
    }
}

/// Posts `WM_COMMAND` to a window — how one of a dialog's own buttons is answered:
/// the button's control id carried as the command, which the dialog box procedure
/// treats exactly like the click. Live evidence uses it to dismiss a consent prompt
/// no automation can click, so the pending call settles on a real answer rather than
/// on teardown killing the engine mid-question.
///
/// Returns whether the message was queued.
#[must_use]
pub fn post_window_command(window: isize, command: usize) -> bool {
    // SAFETY: `window` is a top-level HWND the caller found through the OS handle
    // table; PostMessageW is callable from any thread and only queues the message,
    // so there is nothing it can invalidate. A stale handle simply queues nowhere.
    unsafe { PostMessageW(window as HWND, WM_COMMAND, command, 0) != 0 }
}

/// Reads one composited pixel off the screen — the direct evidence of what the
/// compositor actually shows at that point.
///
/// Answers `None` when the pixel cannot be read. DPI-virtualised processes read
/// in their own virtualised coordinate space, which is consistent for a caller
/// that takes its coordinates from `window_screen_rect` in the same process.
#[must_use]
pub fn screen_pixel(x: i32, y: i32) -> Option<ScreenPixel> {
    // SAFETY: GetDC(null) is the screen device context, GetPixel reads a point
    // within it, and the DC is released before returning. No object outlives
    // the call.
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return None;
        }
        let pixel = GetPixel(screen, x, y);
        ReleaseDC(std::ptr::null_mut(), screen);
        if pixel == 0xFFFF_FFFF {
            return None;
        }
        // COLORREF is 0x00bbggrr.
        Some(ScreenPixel {
            red: (pixel & 0xff) as u8,
            green: ((pixel >> 8) & 0xff) as u8,
            blue: ((pixel >> 16) & 0xff) as u8,
        })
    }
}

/// Places a window topmost in the z-order, so screen-level evidence reads its
/// content rather than whatever happens to occlude it.
///
/// Screen capture is only evidence when the subject is actually visible: a
/// window buried under the examiner's own tools reads back the tools' pixels,
/// which say nothing about the subject. Topmost placement is what visibility
/// needs and is unrestricted; input foreground is a separate, permissioned
/// concern that pixels do not care about, so this does not attempt it.
pub fn make_visible_for_capture(window: isize) {
    // SAFETY: the call accepts any window handle and merely fails for a dead
    // one; the flags move no pixels and resize nothing. A refusal leaves the
    // z-order unchanged — the evidence that follows simply shows it.
    unsafe {
        SetWindowPos(
            window as HWND,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE,
        );
    }
}

/// The virtual screen's extent across every monitor, in the calling process's
/// own coordinate space.
#[must_use]
pub fn virtual_screen() -> ScreenRect {
    // SAFETY: GetSystemMetrics is a pure query with no preconditions.
    unsafe {
        ScreenRect {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN),
            top: GetSystemMetrics(SM_YVIRTUALSCREEN),
            right: GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN),
            bottom: GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

/// Moves a window to a screen position without touching its size or z-order.
///
/// Evidence needs the subject somewhere the examiner's own windows are not —
/// placement is half of visibility, z-order the other half.
pub fn move_window(window: isize, x: i32, y: i32) {
    // SAFETY: accepts any window handle; a dead one merely fails the call.
    unsafe {
        SetWindowPos(
            window as HWND,
            std::ptr::null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOZORDER | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
    }
}

/// Resizes a window — the diagnostic for host-resize behavior questions.
///
/// Size changes are the moment z-order assertions face their real test:
/// `WebView2` re-raises its own child on its own schedule, and a host resize is
/// exactly the kind of event that schedule reacts to.
pub fn resize_window(window: isize, width: i32, height: i32) {
    // SAFETY: accepts any window handle; a dead one merely fails the call.
    unsafe {
        SetWindowPos(
            window as HWND,
            std::ptr::null_mut(),
            0,
            0,
            width,
            height,
            SWP_NOZORDER | SWP_NOMOVE | SWP_SHOWWINDOW,
        );
    }
}

/// A captured image: row-major RGBA pixels with the region's dimensions.
#[derive(Debug, Clone)]
pub struct CaptureImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row-major RGBA bytes, stride exactly `width * 4`.
    pub rgba: Vec<u8>,
}

impl CaptureImage {
    /// The pixel at (x, y) in image coordinates, if inside.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<ScreenPixel> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let offset = ((y * self.width + x) * 4) as usize;
        Some(ScreenPixel {
            red: self.rgba[offset],
            green: self.rgba[offset + 1],
            blue: self.rgba[offset + 2],
        })
    }
}

/// Captures a rectangle of the composited screen as an image.
///
/// This is the evidence primitive the pixel probe cannot be: a region read as
/// one consistent snapshot, immune to the per-point coordinate guessing that
/// made single-pixel evidence fragile. What the compositor showed within the
/// rect is what the image contains, in the caller's own coordinate space.
#[must_use]
pub fn capture_screen_rect(rect: ScreenRect) -> Option<CaptureImage> {
    let width = rect.width();
    let height = rect.height();
    if width <= 0 || height <= 0 {
        return None;
    }
    // SAFETY: the classic GDI capture dance — every object created here is
    // released before return, in reverse order of creation, and the bitmap is
    // selected out of the memory DC before either dies. GetDIBits writes
    // exactly the buffer it is given the length of.
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return None;
        }
        let memory = CreateCompatibleDC(screen);
        if memory.is_null() {
            ReleaseDC(std::ptr::null_mut(), screen);
            return None;
        }
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        if bitmap.is_null() {
            DeleteDC(memory);
            ReleaseDC(std::ptr::null_mut(), screen);
            return None;
        }
        let previous = SelectObject(memory, bitmap);
        let copied = BitBlt(
            memory, 0, 0, width, height, screen, rect.left, rect.top, SRCCOPY,
        );
        let mut result = None;
        if copied != 0 {
            let mut rgba = vec![0u8; (width * height * 4) as usize];
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = width;
            // Negative height: rows arrive top-down, matching the image model.
            info.bmiHeader.biHeight = -height;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;
            let rows = GetDIBits(
                memory,
                bitmap,
                0,
                height as u32,
                rgba.as_mut_ptr().cast(),
                &raw mut info,
                DIB_RGB_COLORS,
            );
            if rows == height {
                // GDI emits BGRA; the evidence model is RGBA.
                for pixel in rgba.as_chunks_mut::<4>().0 {
                    pixel.swap(0, 2);
                    pixel[3] = 255;
                }
                result = Some(CaptureImage {
                    width: width as u32,
                    height: height as u32,
                    rgba,
                });
            }
        }
        SelectObject(memory, previous);
        DeleteObject(bitmap);
        DeleteDC(memory);
        ReleaseDC(std::ptr::null_mut(), screen);
        result
    }
}

/// The primary monitor's extent — real estate guaranteed to exist, unlike the
/// virtual screen's bounding box, whose corners can fall in the dead space
/// between mismatched monitors. A window placed in dead space composes nothing,
/// and evidence captured there is evidence of the void.
#[must_use]
pub fn primary_screen() -> ScreenRect {
    // SAFETY: GetSystemMetrics is a pure query with no preconditions; the
    // primary monitor's origin is (0, 0) by definition.
    unsafe {
        ScreenRect {
            left: 0,
            top: 0,
            right: GetSystemMetrics(SM_CXSCREEN),
            bottom: GetSystemMetrics(SM_CYSCREEN),
        }
    }
}

/// The z-ordered class names of a window's children, topmost first.
///
/// Among sibling child windows, z-order IS compositing order: with
/// `WS_CLIPSIBLINGS` the topmost sibling's pixels win every overlap. This is
/// the environment-independent half of the compositing proof — it does not
/// care what occludes the host window on the user's screen, because it reads
/// the window manager's own truth rather than the compositor's output.
#[must_use]
pub fn child_zorder_classes(parent: isize) -> Vec<String> {
    let mut classes = Vec::new();
    // SAFETY: GetWindow walks the window manager's own sibling chain and
    // GetClassNameW writes only into the buffer it is given the length of.
    // Every handle read is live at the moment of use; a window dying mid-walk
    // ends the chain with null, which ends the loop.
    unsafe {
        let mut child = GetWindow(parent as HWND, GW_CHILD);
        while !child.is_null() {
            let mut name = [0u16; 128];
            let len = GetClassNameW(child, name.as_mut_ptr(), name.len() as i32) as usize;
            if len > 0 {
                classes.push(String::from_utf16_lossy(&name[..len]));
            }
            child = GetWindow(child, GW_HWNDNEXT);
        }
    }
    classes
}

/// Finds the first child of `parent` whose class is `class_name` and reads its
/// screen rect — the OS-level truth of where one specific child sits.
///
/// Child discovery walks the same sibling chain the z-order truth does; the
/// first match is the topmost, which is what a measurement of "the surface"
/// wants when exactly one exists.
#[must_use]
pub fn first_child_rect_of_class(parent: isize, class_name: &str) -> Option<ScreenRect> {
    // SAFETY: the same walk as child_zorder_classes — every handle is live at
    // the moment of use, and a dying window ends the chain in null.
    unsafe {
        let mut child = GetWindow(parent as HWND, GW_CHILD);
        while !child.is_null() {
            let mut name = [0u16; 128];
            let len = GetClassNameW(child, name.as_mut_ptr(), name.len() as i32) as usize;
            if len > 0 && String::from_utf16_lossy(&name[..len]) == class_name {
                return window_screen_rect(child as isize);
            }
            child = GetWindow(child, GW_HWNDNEXT);
        }
    }
    None
}
