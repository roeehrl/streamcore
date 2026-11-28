//! A window as a capture source: its WGC capture item, whether it still exists or is minimized,
//! where DWM draws it, the monitor it is on, and the titled windows the bench chooses from.
//!
//! A window is named by its handle kept as a number (`crate::window::handle_from_native_id`). A
//! stale value names no window, or another one: every call here takes any value and then finds
//! nothing or reads that other window, and none of them writes to a window.

use std::ffi::c_void;
use windows::Graphics::Capture::GraphicsCaptureItem;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, InternalGetWindowText, IsIconic, IsWindow, IsWindowVisible,
};
use windows::core::{BOOL, Result};

fn hwnd(handle: isize) -> HWND {
    HWND(handle as *mut c_void)
}

/// The WGC capture item for a window. Fails for a value that names no window, and for a window
/// WGC cannot capture.
pub fn capture_item(handle: isize) -> Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    // SAFETY: the call takes a window handle by value and validates it; it keeps nothing of
    // ours and returns an owned WinRT object.
    unsafe { interop.CreateForWindow(hwnd(handle)) }
}

/// `IsWindow`: the value names a window that exists.
pub fn exists(handle: isize) -> bool {
    // SAFETY: the call takes any value and says whether it names a window.
    unsafe { IsWindow(Some(hwnd(handle))) }.as_bool()
}

/// `IsIconic`: the window is minimized.
pub fn is_minimized(handle: isize) -> bool {
    // SAFETY: a query on a window handle; false for one that names no window.
    unsafe { IsIconic(hwnd(handle)) }.as_bool()
}

/// The window's rectangle as DWM draws it (`DWMWA_EXTENDED_FRAME_BOUNDS`), as
/// `[left, top, right, bottom]` in physical desktop pixels whatever the caller's DPI awareness:
/// the visible frame, without the invisible resize borders `GetWindowRect` includes. `None` for
/// a window that is gone or that DWM does not compose.
pub fn frame_bounds(handle: isize) -> Option<[i32; 4]> {
    let mut rect = RECT::default();
    // SAFETY: DWMWA_EXTENDED_FRAME_BOUNDS writes one RECT, and `rect` is a live RECT of the
    // size given.
    unsafe {
        DwmGetWindowAttribute(
            hwnd(handle),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut rect).cast(),
            size_of::<RECT>() as u32,
        )
    }
    .ok()?;
    Some([rect.left, rect.top, rect.right, rect.bottom])
}

/// The monitor most of the window is on, as an HMONITOR value (compare with
/// `OutputDesc::hmonitor`); the nearest monitor for a window that is off every screen.
pub fn monitor(handle: isize) -> usize {
    // SAFETY: a query on a window handle; with MONITOR_DEFAULTTONEAREST it returns a monitor
    // for any value.
    unsafe { MonitorFromWindow(hwnd(handle), MONITOR_DEFAULTTONEAREST) }.0 as usize
}

fn title(window: HWND) -> String {
    let mut buffer = [0u16; 512];
    // SAFETY: `buffer` is the slice the call writes at most its length into; it copies the text
    // Windows keeps for the window without sending the window a message, and yields 0 for a
    // handle that names no window.
    let length = unsafe { InternalGetWindowText(window, &mut buffer) };
    let length = usize::try_from(length).unwrap_or(0).min(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

fn is_cloaked(window: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: DWMWA_CLOAKED writes one DWORD, and `cloaked` is a live u32 of the size given.
    let read = unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            size_of::<u32>() as u32,
        )
    };
    read.is_ok() && cloaked != 0
}

/// `EnumWindows`' callback: adds a window that is shown and has a title to the list `list`
/// points to.
unsafe extern "system" fn collect(window: HWND, list: LPARAM) -> BOOL {
    // SAFETY: `list` is the pointer `titled_windows` passed to EnumWindows: a
    // `Vec<(isize, String)>` that outlives the enumeration and that nothing else touches while
    // it runs.
    let list = unsafe { &mut *(list.0 as *mut Vec<(isize, String)>) };
    // SAFETY: a query on the handle EnumWindows passed.
    let visible = unsafe { IsWindowVisible(window) }.as_bool();
    if visible && !is_cloaked(window) {
        let title = title(window);
        if !title.is_empty() {
            list.push((window.0 as isize, title));
        }
    }
    true.into()
}

/// The top-level windows of the calling thread's desktop that are shown (visible and not
/// cloaked) and have a title, front to back, each with its handle value and title.
pub fn titled_windows() -> Vec<(isize, String)> {
    let mut list: Vec<(isize, String)> = Vec::new();
    // SAFETY: `collect` matches the callback's signature and is called only during this call,
    // on this thread, with the pointer to `list` given here, which is live until it returns.
    let _ = unsafe { EnumWindows(Some(collect), LPARAM(&raw mut list as isize)) };
    list
}
