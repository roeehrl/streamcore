//! The Win32 [`Backend`]: `SendInput` for the mouse and keys, synthetic pointer devices for the
//! pen and touch, and the focused window's keyboard layout. Safe code; the calls themselves are
//! in `sys`.

pub(crate) mod sys;

use crate::engine::{Backend, SendOp};
use crate::geometry::VirtualScreen;
use crate::layout::{HostKeyState, KeyLayout};
use crate::pen::PenFrame;
use crate::touch::Contacts;
use sys::pointer::PointerDevice;

pub(crate) struct Win32Backend {
    pen: Option<PointerDevice>,
    touch: Option<PointerDevice>,
}

impl Win32Backend {
    /// Creates the pen and touch devices. Called on the injection thread, which it also makes
    /// per-monitor DPI aware: every coordinate here is in physical pixels, whatever the host's
    /// manifest says.
    pub(crate) fn new() -> Self {
        sys::input::use_physical_pixels();
        Self {
            pen: PointerDevice::pen(),
            touch: PointerDevice::touch(),
        }
    }
}

impl Backend for Win32Backend {
    fn virtual_screen(&self) -> VirtualScreen {
        sys::input::virtual_screen()
    }

    fn cursor_position(&self) -> (i32, i32) {
        sys::input::cursor_position()
    }

    fn send(&mut self, batch: &[SendOp]) {
        sys::input::send(batch);
    }

    fn pen_available(&self) -> bool {
        self.pen.is_some()
    }

    fn inject_pen(&mut self, frame: &PenFrame, position: (i32, i32), pressure: u32) -> bool {
        self.pen
            .as_ref()
            .is_some_and(|pen| pen.inject_pen(frame, position, pressure))
    }

    fn touch_available(&self) -> bool {
        self.touch.is_some()
    }

    fn inject_touch(&mut self, flags: u32, contacts: Contacts) -> bool {
        self.touch
            .as_ref()
            .is_some_and(|touch| touch.inject_touch(flags, contacts))
    }

    fn focused_layout(&self) -> Box<dyn KeyLayout + '_> {
        Box::new(sys::layout::focused())
    }

    fn vk_from_scan(&self, scan: u16) -> u16 {
        sys::input::vk_from_scan(scan)
    }

    fn scan_from_vk(&self, vk: u16) -> u16 {
        sys::input::scan_from_vk(vk)
    }

    fn keyboard_state(&self) -> HostKeyState {
        HostKeyState {
            shift_held: sys::input::shift_down(),
            shortcut_mods_held: sys::input::shortcut_modifier_down(),
            caps_lock: sys::layout::focused_caps_lock(),
        }
    }

    fn key_repeat_settings(&self) -> (u32, u32) {
        sys::input::key_repeat_settings()
    }
}
