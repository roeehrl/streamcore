//! Keyboard layouts. Windows keeps a layout per thread, so the one that matters is the layout of
//! the thread that owns the focused window, not the injection thread's own.

use crate::keys::{VK_CAPITAL, VK_SHIFT};
use crate::layout::{KeyLayout, LayoutHandle, layout_handle};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW,
    RegOpenKeyExW,
};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, GetKeyboardLayout, HKL, MAPVK_VK_TO_VSC_EX, MAPVK_VSC_TO_VK_EX, MapVirtualKeyExW,
    ToUnicodeEx, VkKeyScanExW,
};
use windows::Win32::UI::Shell::SHLoadIndirectString;
use windows::Win32::UI::WindowsAndMessaging::{
    GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
};
use windows::core::{HSTRING, PWSTR};

/// Where Windows registers its keyboard layouts, under `HKEY_LOCAL_MACHINE`.
const LAYOUTS_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Keyboard Layouts";

fn until_nul(units: &[u16]) -> String {
    let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// A string value of one registered layout.
fn layout_value(klid: &str, value: &str) -> Option<String> {
    let subkey = HSTRING::from(format!(r"{LAYOUTS_KEY}\{klid}"));
    let mut buffer = [0u16; 260];
    let mut bytes = size_of_val(&buffer) as u32;
    // SAFETY: the key and value names are NUL-terminated strings that outlive the call, and
    // `buffer` is writable for the `bytes` given. RRF_RT_REG_SZ makes the call refuse any other
    // type and NUL-terminate what it writes.
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &subkey,
            &HSTRING::from(value),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    (result == ERROR_SUCCESS).then(|| until_nul(&buffer))
}

/// The names of the registered layouts' keys (their identifiers).
fn registered_layouts() -> Vec<String> {
    let mut key = HKEY::default();
    // SAFETY: a predefined root key and a NUL-terminated path; `key` is a live local the call
    // writes, closed below.
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(LAYOUTS_KEY),
            None,
            KEY_READ,
            &mut key,
        )
    };
    if opened != ERROR_SUCCESS {
        return Vec::new();
    }
    let mut names = Vec::new();
    for index in 0.. {
        let mut name = [0u16; 64];
        let mut length = name.len() as u32;
        // SAFETY: `key` is open; `name` is writable for the `length` units given, and the call
        // sets `length` to what it wrote.
        let result = unsafe {
            RegEnumKeyExW(
                key,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut length,
                None,
                None,
                None,
                None,
            )
        };
        if result != ERROR_SUCCESS {
            break;
        }
        names.push(until_nul(&name));
    }
    // SAFETY: `key` was opened above and is closed exactly once.
    let _ = unsafe { RegCloseKey(key) };
    names
}

/// The layout's name as Windows shows it (in the Windows display language), else its
/// registered English name.
fn display_name(klid: &str) -> Option<String> {
    if let Some(indirect) = layout_value(klid, "Layout Display Name").filter(|s| s.starts_with('@'))
    {
        let mut buffer = [0u16; 256];
        // SAFETY: the source is a NUL-terminated string that outlives the call, and `buffer` is
        // the slice the call writes a NUL-terminated string into.
        let loaded = unsafe { SHLoadIndirectString(&HSTRING::from(indirect), &mut buffer, None) };
        let name = until_nul(&buffer);
        if loaded.is_ok() && !name.is_empty() {
            return Some(name);
        }
    }
    layout_value(klid, "Layout Text")
}

/// `ToUnicodeEx` flag (Windows 10 1607 and later): leave the keyboard state (dead keys) alone.
const TO_UNICODE_NO_STATE_CHANGE: u32 = 0x4;

/// The thread that owns the focused window and the foreground window's thread (the same when no
/// window has the focus); 0 for none.
fn focus_threads() -> (u32, u32) {
    // SAFETY: the call takes no arguments and returns a window handle or null.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_invalid() {
        return (0, 0);
    }
    // SAFETY: the handle came from GetForegroundWindow; a window that has closed since yields 0.
    let foreground_thread = unsafe { GetWindowThreadProcessId(foreground, None) };
    if foreground_thread == 0 {
        return (0, 0);
    }
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a live GUITHREADINFO with cbSize set, which the call fills.
    let found = unsafe { GetGUIThreadInfo(foreground_thread, &mut info) }.is_ok();
    if found && !info.hwndFocus.is_invalid() {
        // SAFETY: the handle came from GetGUIThreadInfo; a window that has closed yields 0.
        let focus_thread = unsafe { GetWindowThreadProcessId(info.hwndFocus, None) };
        return (focus_thread, foreground_thread);
    }
    (foreground_thread, foreground_thread)
}

/// The layout of the focused window's thread (the foreground window's when no window has the
/// focus, the calling thread's as a last resort).
pub fn focused() -> Win32Layout {
    let (focus, foreground) = focus_threads();
    // SAFETY: GetKeyboardLayout takes a thread ID (0 for the calling thread) and returns that
    // thread's layout handle, or null for a thread that is gone.
    let layout_of = |thread| unsafe { GetKeyboardLayout(thread) };
    let mut layout = HKL::default();
    for thread in [focus, foreground] {
        if layout.is_invalid() && thread != 0 {
            layout = layout_of(thread);
        }
    }
    if layout.is_invalid() {
        layout = layout_of(0);
    }
    Win32Layout(layout)
}

/// Caps Lock as the focused window sees it. `GetKeyState` answers for the calling thread's input
/// state, so share the window's for the moment it takes to ask.
pub fn focused_caps_lock() -> bool {
    let (target, _) = focus_threads();
    // SAFETY: the call takes no arguments and returns the calling thread's ID.
    let own = unsafe { GetCurrentThreadId() };
    let attached = target != 0
        && target != own
        // SAFETY: attaching two thread IDs' input queues; it fails harmlessly for a thread that
        // is gone, and is undone below when it succeeded.
        && unsafe { AttachThreadInput(own, target, true) }.as_bool();
    // SAFETY: GetKeyState takes a virtual key and returns its state; no other effect.
    let caps_lock = unsafe { GetKeyState(i32::from(VK_CAPITAL)) } & 1 != 0;
    if attached {
        // SAFETY: detaches exactly the attachment made above.
        let _ = unsafe { AttachThreadInput(own, target, false) };
    }
    caps_lock
}

/// One Windows keyboard layout. The handle is a value Windows hands out for a loaded layout; it
/// is never dereferenced here, and a stale one only makes the lookups find nothing.
pub struct Win32Layout(HKL);

impl Win32Layout {
    fn translated(&self, vk: u16, scan: u16, state: &[u8; 256]) -> Option<String> {
        let mut buffer = [0u16; 8];
        // SAFETY: `state` is the 256-byte key state the call reads and `buffer` the slice it
        // writes at most its length into. The flag keeps it from changing any keyboard state.
        let written = unsafe {
            ToUnicodeEx(
                u32::from(vk),
                u32::from(scan),
                state,
                &mut buffer,
                TO_UNICODE_NO_STATE_CHANGE,
                Some(self.0),
            )
        };
        // 0: the key types nothing. Negative: a dead key, which is not usable as a character.
        let length = usize::try_from(written).ok().filter(|n| *n > 0)?;
        String::from_utf16(buffer.get(..length)?).ok()
    }
}

impl KeyLayout for Win32Layout {
    fn vk_key_scan(&self, unit: u16) -> Option<(u16, u8)> {
        // SAFETY: a pure lookup of a UTF-16 unit in the layout.
        let found = unsafe { VkKeyScanExW(unit, self.0) };
        // -1 in both bytes: no key types this character.
        let [vk, shift_state] = found.to_le_bytes();
        (found != -1).then_some((u16::from(vk), shift_state))
    }

    fn scan_from_vk(&self, vk: u16) -> u16 {
        // SAFETY: a pure lookup in the layout.
        unsafe { MapVirtualKeyExW(u32::from(vk), MAPVK_VK_TO_VSC_EX, Some(self.0)) as u16 }
    }

    fn vk_from_scan(&self, scan: u16) -> u16 {
        // SAFETY: a pure lookup in the layout; an E0-prefixed scan code goes in as 0xE0xx.
        unsafe { MapVirtualKeyExW(u32::from(scan), MAPVK_VSC_TO_VK_EX, Some(self.0)) as u16 }
    }

    fn translate(&self, vk: u16, scan: u16, shift: bool, caps_lock: bool) -> Option<String> {
        let mut state = [0u8; 256];
        if shift {
            state[usize::from(VK_SHIFT)] = 0x80;
        }
        if caps_lock {
            state[usize::from(VK_CAPITAL)] = 0x01;
        }
        self.translated(vk, scan, &state)
    }

    /// The layout's name as Windows shows it, or its identifier ("0000040C") when the registry
    /// has no name for it.
    fn name(&self) -> String {
        let klid = match layout_handle(self.0.0 as usize as u32) {
            LayoutHandle::Id(klid) => klid,
            LayoutHandle::Variant {
                layout_id,
                fallback,
            } => registered_layouts()
                .into_iter()
                .find(|klid| {
                    layout_value(klid, "Layout Id")
                        .is_some_and(|id| id.eq_ignore_ascii_case(&layout_id))
                })
                .map_or(fallback, |klid| klid.to_uppercase()),
        };
        display_name(&klid).unwrap_or(klid)
    }
}
