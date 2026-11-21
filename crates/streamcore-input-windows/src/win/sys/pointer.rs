//! Synthetic pointer devices: a pen, and a touch device with two contacts.

use crate::pen::PenFrame;
use crate::touch::Contacts;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_FEEDBACK_NONE, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_INFO, POINTER_PEN_INFO, POINTER_TOUCH_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    PEN_MASK_PRESSURE, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN, PT_TOUCH, TOUCH_MASK_CONTACTAREA,
    TOUCH_MASK_ORIENTATION, TOUCH_MASK_PRESSURE,
};

/// A synthetic pointer device, destroyed when dropped. It is created and used on the injection
/// thread only (the handle is not `Send`).
pub struct PointerDevice(HSYNTHETICPOINTERDEVICE);

impl PointerDevice {
    /// A pen with the default visual feedback. `None` where Windows has no synthetic pointer
    /// devices (before Windows 10 1809) or refuses one.
    pub fn pen() -> Option<Self> {
        // SAFETY: the call takes plain values and returns a new device handle or an error.
        unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) }
            .ok()
            .map(Self)
    }

    /// A touch device with two contacts and no visual feedback.
    pub fn touch() -> Option<Self> {
        // SAFETY: the call takes plain values and returns a new device handle or an error.
        unsafe { CreateSyntheticPointerDevice(PT_TOUCH, 2, POINTER_FEEDBACK_NONE) }
            .ok()
            .map(Self)
    }

    /// One pen frame at `position`, relative to the virtual screen's top-left. Call on a device
    /// made by [`pen`](Self::pen).
    pub fn inject_pen(&self, frame: &PenFrame, position: (i32, i32), pressure: u32) -> bool {
        let pen = POINTER_PEN_INFO {
            pointerInfo: POINTER_INFO {
                pointerType: PT_PEN,
                pointerId: 1,
                pointerFlags: POINTER_FLAGS(frame.pointer_flags),
                ptPixelLocation: POINT {
                    x: position.0,
                    y: position.1,
                },
                ..Default::default()
            },
            // The eraser end is INVERTED for its whole session and ERASER while it touches;
            // BARREL while the side button is held (the pen session sets them).
            penFlags: frame.pen_flags,
            penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
            pressure,
            rotation: 0,
            tiltX: frame.tilt_x,
            tiltY: frame.tilt_y,
        };
        self.inject(&[POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 { penInfo: pen },
        }])
    }

    /// Both contacts, IDs 0 and 1, in one call so Windows sees them as one frame. Call on a
    /// device made by [`touch`](Self::touch).
    pub fn inject_touch(&self, flags: u32, contacts: Contacts) -> bool {
        let mut id = 0;
        let frame = contacts.map(|(x, y)| {
            let touch = POINTER_TOUCH_INFO {
                pointerInfo: POINTER_INFO {
                    pointerType: PT_TOUCH,
                    pointerId: id,
                    pointerFlags: POINTER_FLAGS(flags),
                    ptPixelLocation: POINT { x, y },
                    ..Default::default()
                },
                touchFlags: 0,
                touchMask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE,
                // A fingertip: a small contact area, upright, at about half pressure.
                rcContact: RECT {
                    left: x.saturating_sub(2),
                    top: y.saturating_sub(2),
                    right: x.saturating_add(2),
                    bottom: y.saturating_add(2),
                },
                rcContactRaw: RECT::default(),
                orientation: 90,
                pressure: 32000,
            };
            id += 1;
            POINTER_TYPE_INFO {
                r#type: PT_TOUCH,
                Anonymous: POINTER_TYPE_INFO_0 { touchInfo: touch },
            }
        });
        self.inject(&frame)
    }

    fn inject(&self, frame: &[POINTER_TYPE_INFO]) -> bool {
        // SAFETY: `self.0` is a device this type created and has not destroyed, and `frame` is a
        // live slice of initialized POINTER_TYPE_INFO structures whose union member matches
        // `type`.
        unsafe { InjectSyntheticPointerInput(self.0, frame) }.is_ok()
    }
}

impl Drop for PointerDevice {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from CreateSyntheticPointerDevice and is destroyed exactly once,
        // here.
        unsafe { DestroySyntheticPointerDevice(self.0) };
    }
}
