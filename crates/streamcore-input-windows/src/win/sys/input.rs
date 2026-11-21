//! `SendInput`, the pointer and screen metrics, and the keyboard state of the computer.

use crate::engine::SendOp;
use crate::geometry::VirtualScreen;
use crate::keys::{VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT};
use streamcore_input::StrokeButton;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC,
    MAPVK_VSC_TO_VK_EX, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, MapVirtualKeyW, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SPI_GETKEYBOARDDELAY, SPI_GETKEYBOARDSPEED, SYSTEM_PARAMETERS_INFO_ACTION,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};

/// Makes the calling thread per-monitor DPI aware, so screen metrics and pointer coordinates are
/// physical pixels.
pub fn use_physical_pixels() {
    // SAFETY: the call takes a predefined context value and changes only the calling thread's
    // DPI awareness; the previous context it returns needs no cleanup.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

pub fn virtual_screen() -> VirtualScreen {
    // SAFETY: GetSystemMetrics takes an index and returns a value; it has no other effect.
    let metric = |index| unsafe { GetSystemMetrics(index) };
    VirtualScreen {
        x: metric(SM_XVIRTUALSCREEN),
        y: metric(SM_YVIRTUALSCREEN),
        width: metric(SM_CXVIRTUALSCREEN),
        height: metric(SM_CYVIRTUALSCREEN),
    }
}

/// The pointer's position in desktop pixels; the origin when Windows will not say (another
/// desktop has the input).
pub fn cursor_position() -> (i32, i32) {
    let mut point = POINT::default();
    // SAFETY: `point` is a live POINT the call writes; on failure it stays zeroed.
    let _ = unsafe { GetCursorPos(&mut point) };
    (point.x, point.y)
}

/// `MapVirtualKeyW(MAPVK_VSC_TO_VK_EX)` on the calling thread's layout.
pub fn vk_from_scan(scan: u16) -> u16 {
    // SAFETY: a pure lookup in the calling thread's keyboard layout.
    unsafe { MapVirtualKeyW(u32::from(scan), MAPVK_VSC_TO_VK_EX) as u16 }
}

/// `MapVirtualKeyW(MAPVK_VK_TO_VSC)` on the calling thread's layout.
pub fn scan_from_vk(vk: u16) -> u16 {
    // SAFETY: a pure lookup in the calling thread's keyboard layout.
    unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) as u16 }
}

fn key_down(vk: u16) -> bool {
    // SAFETY: GetAsyncKeyState takes a virtual key and returns its state; no other effect.
    let state = unsafe { GetAsyncKeyState(i32::from(vk)) };
    state < 0
}

/// Shift is held on the computer.
pub fn shift_down() -> bool {
    key_down(VK_SHIFT)
}

/// Ctrl, Alt or a Windows key is held on the computer.
pub fn shortcut_modifier_down() -> bool {
    [VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
        .into_iter()
        .any(key_down)
}

/// The system's keyboard delay (0 to 3) and speed (0 to 31) settings; the Windows defaults when
/// they cannot be read.
pub fn key_repeat_settings() -> (u32, u32) {
    let setting = |action: SYSTEM_PARAMETERS_INFO_ACTION, default: u32| {
        let mut value = default;
        // SAFETY: for these two actions pvParam points to an integer the call writes (an int
        // for the delay, a DWORD for the speed; both 32 bits), and `value` outlives the call.
        // On failure it keeps the default.
        let _ = unsafe {
            SystemParametersInfoW(
                action,
                0,
                Some((&raw mut value).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        value
    };
    (
        setting(SPI_GETKEYBOARDDELAY, 1),
        setting(SPI_GETKEYBOARDSPEED, 31),
    )
}

fn key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS, up: bool) -> INPUT {
    let up = if up {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags | up,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                // A signed wheel amount in an unsigned field, as Windows defines it.
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn input(op: SendOp) -> INPUT {
    match op {
        SendOp::Key {
            vk,
            scan,
            extended,
            scan_only,
            up,
        } => {
            let mut flags = KEYBD_EVENT_FLAGS(0);
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if scan_only {
                flags |= KEYEVENTF_SCANCODE;
            }
            key(vk, scan, flags, up)
        }
        SendOp::Unicode { unit, up } => key(0, unit, KEYEVENTF_UNICODE, up),
        SendOp::MoveTo { x, y } => mouse(
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            x,
            y,
            0,
        ),
        SendOp::Button { button, down } => {
            let flags = match (button, down) {
                (StrokeButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                (StrokeButton::Left, false) => MOUSEEVENTF_LEFTUP,
                (StrokeButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                (StrokeButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                (StrokeButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                (StrokeButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
            };
            mouse(flags, 0, 0, 0)
        }
        SendOp::Wheel { delta, horizontal } => {
            let flags = if horizontal {
                MOUSEEVENTF_HWHEEL
            } else {
                MOUSEEVENTF_WHEEL
            };
            mouse(flags, 0, 0, delta)
        }
    }
}

/// One `SendInput` call for the whole batch, so nothing can land in between. Windows refuses
/// input while a secure desktop or a higher-integrity window has it; there is nothing to do
/// about that here, and the events are dropped.
pub fn send(batch: &[SendOp]) {
    let inputs: Vec<INPUT> = batch.iter().copied().map(input).collect();
    if inputs.is_empty() {
        return;
    }
    // SAFETY: `inputs` is a live slice of initialized INPUT structures and cbSize is the size of
    // one of them, as the call requires.
    unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
}
