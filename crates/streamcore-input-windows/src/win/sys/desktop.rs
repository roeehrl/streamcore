//! Reading the desktop: top-level windows and their processes, the session's desktops and the
//! Windows version. Every function takes and returns plain values; a [`WindowId`] is a window
//! handle kept as a number, and a stale one only makes the calls here find nothing.

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use crate::desktop::WindowId;
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, HWND, LPARAM, POINT, RECT,
};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK, WTS_SESSIONSTATE_UNLOCK,
    WTSActive, WTSFreeMemory, WTSINFOEXW, WTSQuerySessionInformationW, WTSSessionInfoEx,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetProcessWindowStation,
    GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetClientRect,
    GetForegroundWindow, GetShellWindow, GetWindow, GetWindowLongW, GetWindowThreadProcessId,
    InternalGetWindowText, IsIconic, IsWindow, IsWindowVisible, SW_RESTORE, SetForegroundWindow,
    ShowWindowAsync,
};
use windows::core::{BOOL, PWSTR, w};

fn handle(window: WindowId) -> HWND {
    HWND(window.0 as *mut c_void)
}

fn id(window: HWND) -> Option<WindowId> {
    (!window.is_invalid()).then_some(WindowId(window.0 as isize))
}

fn until_nul(units: &[u16]) -> String {
    let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// The window in the foreground, if any.
pub fn foreground_window() -> Option<WindowId> {
    // SAFETY: the call takes no arguments and returns a window handle or null.
    id(unsafe { GetForegroundWindow() })
}

/// The process that created a window; `None` for a window that is gone.
pub fn window_process(window: WindowId) -> Option<u32> {
    let mut pid = 0u32;
    // SAFETY: `pid` is a live u32 the call writes; a handle that names no window yields 0.
    let thread = unsafe { GetWindowThreadProcessId(handle(window), Some(&mut pid)) };
    (thread != 0 && pid != 0).then_some(pid)
}

/// `QueryFullProcessImageNameW` into a buffer of `units`: the path, or the error.
fn image_name(process: HANDLE, units: usize) -> windows::core::Result<Vec<u16>> {
    let mut buffer = vec![0u16; units];
    let mut length = buffer.len() as u32;
    // SAFETY: `process` is a process handle the caller holds open; `buffer` is writable for the
    // `length` units given, and the call sets `length` to the units it wrote, without the NUL.
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }?;
    buffer.truncate(length as usize);
    Ok(buffer)
}

/// The full path of a process's executable. Asks for the least access that allows it, which a
/// process may have even to one running elevated.
pub fn process_image(pid: u32) -> Option<PathBuf> {
    // SAFETY: opens a process by ID with query-only access; the handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    // Twice MAX_PATH holds nearly every path; only a longer one pays for the largest Windows
    // allows (32767 units and a NUL).
    let path = image_name(process, 520).or_else(|error| {
        if error.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() {
            image_name(process, 32768)
        } else {
            Err(error)
        }
    });
    // SAFETY: `process` was opened above and is closed exactly once.
    let _ = unsafe { CloseHandle(process) };
    let path = path.ok()?;
    (!path.is_empty()).then(|| PathBuf::from(OsString::from_wide(&path)))
}

/// A window's class name; empty for a window that is gone.
pub fn class_name(window: WindowId) -> String {
    // A class name is at most 256 characters.
    let mut buffer = [0u16; 260];
    // SAFETY: `buffer` is the slice the call writes at most its length into, NUL-terminated; a
    // handle that names no window yields 0.
    let length = unsafe { GetClassNameW(handle(window), &mut buffer) };
    let length = usize::try_from(length).unwrap_or(0).min(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

/// The shell's desktop window (`GetShellWindow`), if a shell is running.
pub fn shell_window() -> Option<WindowId> {
    // SAFETY: the call takes no arguments and returns a window handle or null.
    id(unsafe { GetShellWindow() })
}

/// A window's title, without sending the window a message: `InternalGetWindowText` copies the
/// text Windows keeps for the window, so a window that has stopped responding cannot stall the
/// caller. Titles longer than the buffer are cut.
pub fn window_title(window: WindowId) -> String {
    let mut buffer = [0u16; 512];
    // SAFETY: `buffer` is the slice the call writes at most its length into, NUL-terminated; a
    // handle that names no window yields 0.
    let length = unsafe { InternalGetWindowText(handle(window), &mut buffer) };
    let length = usize::try_from(length).unwrap_or(0).min(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

/// `IsWindow`: the handle names a window that exists.
pub fn is_window(window: WindowId) -> bool {
    // SAFETY: the call takes any value and says whether it names a window.
    unsafe { IsWindow(Some(handle(window))) }.as_bool()
}

/// `IsWindowVisible`.
pub fn is_visible(window: WindowId) -> bool {
    // SAFETY: a query on a window handle; false for one that names no window.
    unsafe { IsWindowVisible(handle(window)) }.as_bool()
}

/// `IsIconic`: the window is minimized.
pub fn is_minimized(window: WindowId) -> bool {
    // SAFETY: a query on a window handle; false for one that names no window.
    unsafe { IsIconic(handle(window)) }.as_bool()
}

/// DWM hides the window although it counts as visible (`DWMWA_CLOAKED` is not zero).
pub fn is_cloaked(window: WindowId) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: DWMWA_CLOAKED writes one DWORD, and `cloaked` is a live u32 of the size given.
    let read = unsafe {
        DwmGetWindowAttribute(
            handle(window),
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            size_of::<u32>() as u32,
        )
    };
    read.is_ok() && cloaked != 0
}

/// The window's rectangle as DWM draws it (`DWMWA_EXTENDED_FRAME_BOUNDS`), as
/// `[left, top, right, bottom]` in physical desktop pixels whatever the caller's DPI awareness.
/// `None` for a window that is gone or that DWM does not compose.
pub fn frame_bounds(window: WindowId) -> Option<[i32; 4]> {
    let mut rect = RECT::default();
    // SAFETY: DWMWA_EXTENDED_FRAME_BOUNDS writes one RECT, and `rect` is a live RECT of the
    // size given.
    unsafe {
        DwmGetWindowAttribute(
            handle(window),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut rect).cast(),
            size_of::<RECT>() as u32,
        )
    }
    .ok()?;
    Some([rect.left, rect.top, rect.right, rect.bottom])
}

/// The window's client area as `[left, top, right, bottom]` in physical desktop pixels: read
/// under a per-monitor-aware thread context, which is put back before returning. `None` for a
/// window that is gone.
pub fn client_bounds(window: WindowId) -> Option<[i32; 4]> {
    // SAFETY: changes only the calling thread's DPI awareness context and returns the one it
    // had, which is restored below on every path.
    let before =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let mut rect = RECT::default();
    let mut origin = POINT::default();
    // SAFETY: `rect` is a live RECT the call writes; it fails for a handle that names no
    // window.
    let read = unsafe { GetClientRect(handle(window), &mut rect) }.is_ok()
        // SAFETY: `origin` is a live POINT the call reads and writes; it returns false for a
        // handle that names no window.
        && unsafe { ClientToScreen(handle(window), &mut origin) }.as_bool();
    if !before.is_invalid() {
        // SAFETY: restores the context this thread had before this function changed it.
        unsafe { SetThreadDpiAwarenessContext(before) };
    }
    read.then(|| {
        [
            origin.x,
            origin.y,
            origin.x.saturating_add(rect.right),
            origin.y.saturating_add(rect.bottom),
        ]
    })
}

/// The window's extended style (`GWL_EXSTYLE`); 0 for a window that is gone.
pub fn ex_style(window: WindowId) -> u32 {
    // SAFETY: reads one of the window's own values by a predefined index.
    unsafe { GetWindowLongW(handle(window), GWL_EXSTYLE) as u32 }
}

/// The window has an owner window.
pub fn has_owner(window: WindowId) -> bool {
    // SAFETY: a query on a window handle; an error (or null) when there is no owner.
    unsafe { GetWindow(handle(window), GW_OWNER) }.is_ok_and(|owner| !owner.is_invalid())
}

/// The process of the packaged app shown inside a frame host's window: the process of the
/// frame's core window, when it is not the frame host itself.
pub fn hosted_app_process(frame: WindowId, frame_pid: u32) -> Option<u32> {
    let mut after = None;
    // A frame has a handful of children; the bound only keeps a misbehaving one from looping.
    for _ in 0..64 {
        // SAFETY: the class name is a NUL-terminated literal; the call searches the children of
        // `frame` after `after` and returns one, or an error when there are no more.
        let child = unsafe {
            FindWindowExW(
                Some(handle(frame)),
                after,
                w!("Windows.UI.Core.CoreWindow"),
                None,
            )
        }
        .ok()
        .filter(|child| !child.is_invalid())?;
        let pid = id(child).and_then(window_process);
        if let Some(pid) = pid.filter(|pid| *pid != frame_pid) {
            return Some(pid);
        }
        after = Some(child);
    }
    None
}

/// `EnumWindows`' callback: adds the window to the list `list` points to.
unsafe extern "system" fn collect(window: HWND, list: LPARAM) -> BOOL {
    // SAFETY: `list` is the pointer `top_level_windows` passed to EnumWindows: a `Vec<WindowId>`
    // that outlives the enumeration and that nothing else touches while it runs.
    let list = unsafe { &mut *(list.0 as *mut Vec<WindowId>) };
    list.extend(id(window));
    true.into()
}

/// Every top-level window of the calling thread's desktop, front to back.
pub fn top_level_windows() -> Vec<WindowId> {
    let mut list: Vec<WindowId> = Vec::new();
    // SAFETY: `collect` matches the callback's signature and is called only during this call,
    // on this thread, with the pointer to `list` given here, which is live until it returns.
    let _ = unsafe { EnumWindows(Some(collect), LPARAM(&raw mut list as isize)) };
    list
}

/// Posts a restore to a minimized window (`ShowWindowAsync`, so the caller never waits for the
/// window's thread). `SW_RESTORE` also activates the window.
pub fn restore(window: WindowId) {
    // SAFETY: posts a show command to a window handle; nothing happens for one that names none.
    let _ = unsafe { ShowWindowAsync(handle(window), SW_RESTORE) };
}

/// `SetForegroundWindow`: whether Windows brought the window to the foreground.
pub fn set_foreground(window: WindowId) -> bool {
    // SAFETY: a request on a window handle; refused (false) when Windows does not allow the
    // calling process to change the foreground, or for a handle that names no window.
    unsafe { SetForegroundWindow(handle(window)) }.as_bool()
}

/// The name of a window station or desktop.
fn object_name(object: HANDLE) -> Option<String> {
    let mut buffer = [0u16; 256];
    // SAFETY: `object` is a window station or desktop handle the caller holds open; `buffer` is
    // writable for the byte length given, and UOI_NAME writes a NUL-terminated string into it.
    unsafe {
        GetUserObjectInformationW(
            object,
            UOI_NAME,
            Some(buffer.as_mut_ptr().cast()),
            size_of_val(&buffer) as u32,
            None,
        )
    }
    .ok()?;
    Some(until_nul(&buffer))
}

/// The name of the process's window station ("WinSta0" for the interactive one).
pub fn station_name() -> Option<String> {
    // SAFETY: returns the process's own window station handle, which is not to be closed.
    let station = unsafe { GetProcessWindowStation() }.ok()?;
    object_name(HANDLE(station.0))
}

/// The name of the desktop the calling thread runs on.
pub fn thread_desktop_name() -> Option<String> {
    // SAFETY: GetCurrentThreadId takes no arguments; GetThreadDesktop returns the thread's own
    // desktop handle, which is not to be closed.
    let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.ok()?;
    object_name(HANDLE(desktop.0))
}

/// The name of the desktop that has the input now; `None` when this process may not open it
/// (the secure desktop) or has no interactive window station.
pub fn input_desktop_name() -> Option<String> {
    // SAFETY: opens the input desktop with read access, not inheritable; closed below.
    let desktop =
        unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) }.ok()?;
    let name = object_name(HANDLE(desktop.0));
    // SAFETY: `desktop` was opened above and is closed exactly once.
    let _ = unsafe { CloseDesktop(desktop) };
    name
}

/// The process's own session, as the session manager reports it.
#[derive(Debug, Clone, Copy)]
pub struct SessionInfo {
    /// Connected to its console or remote display (`WTSActive`).
    pub connected: bool,
    /// Locked or unlocked; `None` when reported as unknown.
    pub locked: Option<bool>,
}

/// `WTSQuerySessionInformationW(WTSSessionInfoEx)` for the calling process's session.
pub fn session_info() -> Option<SessionInfo> {
    let mut buffer = PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: `buffer` and `bytes` are live locals the call writes; on success `buffer` is
    // memory the call allocated, freed below.
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes,
        )
    }
    .ok()?;
    if buffer.is_null() {
        return None;
    }
    let info = (bytes as usize >= size_of::<WTSINFOEXW>()).then(|| {
        // SAFETY: the call succeeded for WTSSessionInfoEx, so `buffer` holds a WTSINFOEXW, and
        // it is at least that long. Read unaligned: the allocation's alignment is not promised.
        unsafe { buffer.0.cast::<WTSINFOEXW>().read_unaligned() }
    });
    // SAFETY: `buffer` was allocated by WTSQuerySessionInformationW and is freed exactly once.
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    let info = info.filter(|info| info.Level == 1)?;
    // SAFETY: Level 1 says the union holds its WTSINFOEX_LEVEL1_W member, which is plain data.
    let level1 = unsafe { info.Data.WTSInfoExLevel1 };
    Some(SessionInfo {
        connected: level1.SessionState == WTSActive,
        locked: match level1.SessionFlags as u32 {
            WTS_SESSIONSTATE_LOCK => Some(true),
            WTS_SESSIONSTATE_UNLOCK => Some(false),
            _ => None,
        },
    })
}

/// `RtlGetVersion`: the running system's major and minor version and build number, whatever
/// the executable's manifest declares.
pub fn os_version() -> Option<(u32, u32, u32)> {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a live OSVERSIONINFOW with its size set, which the call fills.
    let status = unsafe { RtlGetVersion(&mut info) };
    status
        .is_ok()
        .then_some((info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber))
}
