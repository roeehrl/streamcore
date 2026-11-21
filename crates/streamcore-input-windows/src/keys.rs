//! Key tables: USB HID usages (page 0x07) to Windows virtual keys and set-1 scan codes, the
//! virtual keys of the named keys, and which virtual keys a real keyboard sends with the E0
//! prefix.

use streamcore_input::NamedKey;

pub const VK_BACK: u16 = 0x08;
pub const VK_TAB: u16 = 0x09;
pub const VK_RETURN: u16 = 0x0D;
pub const VK_SHIFT: u16 = 0x10;
pub const VK_CONTROL: u16 = 0x11;
/// Alt.
pub const VK_MENU: u16 = 0x12;
pub const VK_CAPITAL: u16 = 0x14;
pub const VK_ESCAPE: u16 = 0x1B;
pub const VK_SPACE: u16 = 0x20;
pub const VK_PRIOR: u16 = 0x21;
pub const VK_NEXT: u16 = 0x22;
pub const VK_END: u16 = 0x23;
pub const VK_HOME: u16 = 0x24;
pub const VK_LEFT: u16 = 0x25;
pub const VK_UP: u16 = 0x26;
pub const VK_RIGHT: u16 = 0x27;
pub const VK_DOWN: u16 = 0x28;
pub const VK_INSERT: u16 = 0x2D;
pub const VK_DELETE: u16 = 0x2E;
pub const VK_HELP: u16 = 0x2F;
pub const VK_LWIN: u16 = 0x5B;
pub const VK_RWIN: u16 = 0x5C;
pub const VK_APPS: u16 = 0x5D;
pub const VK_NUMPAD0: u16 = 0x60;
pub const VK_NUMPAD1: u16 = 0x61;
pub const VK_MULTIPLY: u16 = 0x6A;
pub const VK_ADD: u16 = 0x6B;
pub const VK_SUBTRACT: u16 = 0x6D;
pub const VK_DECIMAL: u16 = 0x6E;
pub const VK_DIVIDE: u16 = 0x6F;
pub const VK_F1: u16 = 0x70;
pub const VK_F13: u16 = 0x7C;

/// VK 0xE8 is unassigned. Tapped while Alt is down it makes the Alt a chord, so releasing Alt
/// does not activate the menu bar of the focused app.
pub const VK_MASK: u16 = 0xE8;

/// An Alt tap that does not open a menu bar: Alt down, the mask key down and up, Alt up, as
/// (virtual key, is key-up).
///
/// A host that brings a window to the foreground sends an Alt tap first, because Windows refuses
/// `SetForegroundWindow` from a background process unless it has just seen input. A lone Alt
/// press is also what opens the menu bar of the app it reaches, so the next letter typed picked a
/// menu; the mask key in between prevents that. The injector uses the same mask after an
/// Alt+wheel zoom.
pub const MASKED_ALT_TAP: [(u16, bool); 4] = [
    (VK_MENU, false),
    (VK_MASK, false),
    (VK_MASK, true),
    (VK_MENU, true),
];

/// USB HID usage of the space bar.
pub const HID_SPACE: u16 = 0x2C;

/// HID keyboard usage to the Windows virtual key with the US meaning of that key. 0 when none.
pub fn hid_to_vk(hid: u16) -> u16 {
    match hid {
        0x04..=0x1D => u16::from(b'A') + (hid - 0x04),
        0x1E..=0x26 => u16::from(b'1') + (hid - 0x1E),
        0x27 => u16::from(b'0'),
        0x3A..=0x45 => VK_F1 + (hid - 0x3A),
        0x59..=0x61 => VK_NUMPAD1 + (hid - 0x59),
        // F13 to F24.
        0x68..=0x73 => VK_F13 + (hid - 0x68),
        0x28 => VK_RETURN,
        0x29 => VK_ESCAPE,
        0x2A => VK_BACK,
        0x2B => VK_TAB,
        0x2C => VK_SPACE,
        0x2D => 0xBD, // -
        0x2E => 0xBB, // =
        0x2F => 0xDB, // [
        0x30 => 0xDD, // ]
        0x31 => 0xDC, // \
        0x33 => 0xBA, // ;
        0x34 => 0xDE, // '
        0x35 => 0xC0, // `
        0x36 => 0xBC, // ,
        0x37 => 0xBE, // .
        0x38 => 0xBF, // /
        0x39 => VK_CAPITAL,
        0x46 => 0x2C, // Print Screen
        0x47 => 0x91, // Scroll Lock
        0x48 => 0x13, // Pause
        0x49 => VK_INSERT,
        0x4A => VK_HOME,
        0x4B => VK_PRIOR,
        0x4C => VK_DELETE,
        0x4D => VK_END,
        0x4E => VK_NEXT,
        0x4F => VK_RIGHT,
        0x50 => VK_LEFT,
        0x51 => VK_DOWN,
        0x52 => VK_UP,
        0x53 => 0x90, // Num Lock
        0x54 => VK_DIVIDE,
        0x55 => VK_MULTIPLY,
        0x56 => VK_SUBTRACT,
        0x57 => VK_ADD,
        0x58 => VK_RETURN, // keypad Enter
        0x62 => VK_NUMPAD0,
        0x63 => VK_DECIMAL,
        0x65 => VK_APPS,
        0x67 => 0xBB, // keypad =: Windows has no such key, so the plain =
        0x75 => VK_HELP,
        0x76 => VK_APPS, // Menu
        0x7F => 0xAD,    // mute
        0x80 => 0xAF,    // volume up
        0x81 => 0xAE,    // volume down
        0x8A => 0x1C,    // Henkan
        0x8B => 0x1D,    // Muhenkan
        0x90 => 0x15,    // Hangul / Kana
        0x91 => 0x19,    // Hanja
        0xE0 | 0xE4 => VK_CONTROL,
        0xE1 | 0xE5 => VK_SHIFT,
        0xE2 | 0xE6 => VK_MENU,
        0xE3 => VK_LWIN,
        0xE7 => VK_RWIN,
        _ => 0,
    }
}

const LETTER_SCANS: [u16; 26] = [
    0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, // a-m
    0x31, 0x18, 0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C, // n-z
];
/// Keypad 1 to 9.
const KEYPAD_SCANS: [u16; 9] = [0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49];
const F13_TO_F24_SCANS: [u16; 12] = [
    0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x6B, 0x6C, 0x6D, 0x6E, 0x76,
];

/// HID keyboard usage to the set-1 scan code of that physical key, independent of the layout.
/// E0-prefixed keys come back as 0xE0xx. 0 when none.
pub fn hid_to_scan(hid: u16) -> u16 {
    match hid {
        0x04..=0x1D => LETTER_SCANS[usize::from(hid - 0x04)],
        // 1 to 9, then 0.
        0x1E..=0x27 => 0x02 + (hid - 0x1E),
        // F1 to F10.
        0x3A..=0x43 => 0x3B + (hid - 0x3A),
        0x59..=0x61 => KEYPAD_SCANS[usize::from(hid - 0x59)],
        0x68..=0x73 => F13_TO_F24_SCANS[usize::from(hid - 0x68)],
        0x28 => 0x1C,
        0x29 => 0x01,
        0x2A => 0x0E,
        0x2B => 0x0F,
        0x2C => 0x39,
        0x2D => 0x0C,
        0x2E => 0x0D,
        0x2F => 0x1A,
        0x30 => 0x1B,
        0x31 | 0x32 => 0x2B,
        0x33 => 0x27,
        0x34 => 0x28,
        0x35 => 0x29,
        0x36 => 0x33,
        0x37 => 0x34,
        0x38 => 0x35,
        0x39 => 0x3A,
        0x44 => 0x57,
        0x45 => 0x58,
        0x46 => 0xE037,
        0x47 => 0x46,
        0x49 => 0xE052,
        0x4A => 0xE047,
        0x4B => 0xE049,
        0x4C => 0xE053,
        0x4D => 0xE04F,
        0x4E => 0xE051,
        0x4F => 0xE04D,
        0x50 => 0xE04B,
        0x51 => 0xE050,
        0x52 => 0xE048,
        0x53 => 0x45,
        0x54 => 0xE035,
        0x55 => 0x37,
        0x56 => 0x4A,
        0x57 => 0x4E,
        0x58 => 0xE01C,
        0x62 => 0x52,
        0x63 => 0x53,
        0x64 => 0x56,
        0x65 => 0xE05D,
        0x67 => 0x59,
        0x87 => 0x73,
        0x88 => 0x70,
        0x89 => 0x7D,
        0x8A => 0x79,
        0x8B => 0x7B,
        0xE0 => 0x1D,
        0xE1 => 0x2A,
        0xE2 => 0x38,
        0xE3 => 0xE05B,
        0xE4 => 0xE01D,
        0xE5 => 0x36,
        0xE6 => 0xE038,
        0xE7 => 0xE05C,
        _ => 0,
    }
}

/// Keys whose character depends on the layout (ISO, JIS) are sent by position: their scan code,
/// with whichever virtual key the current layout puts there. 0 for every other key.
pub fn positional_scan(hid: u16) -> u16 {
    match hid {
        0x32 => 0x2B, // non-US # ~ (where US has \)
        0x64 => 0x56, // ISO \ | beside the left Shift
        0x87 => 0x73, // JIS Ro
        0x88 => 0x70, // Katakana / Hiragana
        0x89 => 0x7D, // JIS Yen
        _ => 0,
    }
}

/// A named key's virtual key, and whether it needs the extended flag whatever the layout says
/// (keypad Enter shares VK_RETURN with the main Enter). `None` for keypad =, which Windows has no
/// key for: callers type "=" instead.
pub fn named_to_vk(key: NamedKey) -> Option<(u16, bool)> {
    use NamedKey::*;
    let hid = key.hid();
    let vk = match key {
        KeypadEquals => return None,
        Escape => VK_ESCAPE,
        Tab => VK_TAB,
        Return | KeypadEnter => VK_RETURN,
        Backspace => VK_BACK,
        Delete => VK_DELETE,
        Space => VK_SPACE,
        Left => VK_LEFT,
        Right => VK_RIGHT,
        Up => VK_UP,
        Down => VK_DOWN,
        Home => VK_HOME,
        End => VK_END,
        PageUp => VK_PRIOR,
        PageDown => VK_NEXT,
        Insert => VK_INSERT,
        CapsLock => VK_CAPITAL,
        KeypadPlus => VK_ADD,
        KeypadMinus => VK_SUBTRACT,
        KeypadMultiply => VK_MULTIPLY,
        KeypadDivide => VK_DIVIDE,
        KeypadDecimal => VK_DECIMAL,
        Help => VK_HELP,
        Menu => VK_APPS,
        // F1 to F24 and keypad 0 to 9 sit where their HID usages say.
        _ => hid_to_vk(hid),
    };
    Some((vk, key == KeypadEnter))
}

/// Keys a real keyboard sends with the E0 prefix (used when the layout gives no scan code).
pub fn is_extended_vk(vk: u16) -> bool {
    matches!(
        vk,
        0x21..=0x28          // Page Up, Page Down, End, Home and the arrows
        | 0x2C..=0x2E        // Print Screen, Insert, Delete
        | 0x5B..=0x5D        // the Windows keys, Apps
        | 0x6F | 0x90        // keypad Divide, Num Lock
        | 0xAD..=0xAF // volume
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_function_keys() {
        assert_eq!(hid_to_vk(0x04), u16::from(b'A'));
        assert_eq!(hid_to_vk(0x1D), u16::from(b'Z'));
        assert_eq!(hid_to_vk(0x1E), u16::from(b'1'));
        assert_eq!(hid_to_vk(0x27), u16::from(b'0'));
        assert_eq!(hid_to_vk(0x3A), 0x70);
        assert_eq!(hid_to_vk(0x45), 0x7B);
        assert_eq!(hid_to_vk(0x68), 0x7C);
        assert_eq!(hid_to_vk(0x73), 0x87);
        assert_eq!(hid_to_vk(0x59), 0x61);
        assert_eq!(hid_to_vk(0x61), 0x69);
        assert_eq!(hid_to_vk(0x62), 0x60);
        assert_eq!(hid_to_vk(0xE7), VK_RWIN);
        assert_eq!(hid_to_vk(0x03), 0);
        assert_eq!(hid_to_vk(0xFFFF), 0);
    }

    #[test]
    fn scan_codes_by_position() {
        assert_eq!(hid_to_scan(0x04), 0x1E); // a
        assert_eq!(hid_to_scan(0x14), 0x10); // q
        assert_eq!(hid_to_scan(0x1A), 0x11); // w
        assert_eq!(hid_to_scan(0x1D), 0x2C); // z
        assert_eq!(hid_to_scan(0x27), 0x0B); // 0
        assert_eq!(hid_to_scan(0x43), 0x44); // F10
        assert_eq!(hid_to_scan(0x44), 0x57); // F11
        assert_eq!(hid_to_scan(0x4F), 0xE04D); // right arrow
        assert_eq!(hid_to_scan(0x58), 0xE01C); // keypad Enter
        assert_eq!(hid_to_scan(0xE6), 0xE038); // right Alt
        assert_eq!(hid_to_scan(0x64), 0x56);
        assert_eq!(hid_to_scan(0), 0);
    }

    #[test]
    fn named_keys() {
        assert_eq!(named_to_vk(NamedKey::F1), Some((0x70, false)));
        assert_eq!(named_to_vk(NamedKey::F24), Some((0x87, false)));
        assert_eq!(named_to_vk(NamedKey::Keypad0), Some((0x60, false)));
        assert_eq!(named_to_vk(NamedKey::Keypad9), Some((0x69, false)));
        assert_eq!(named_to_vk(NamedKey::Return), Some((VK_RETURN, false)));
        assert_eq!(named_to_vk(NamedKey::KeypadEnter), Some((VK_RETURN, true)));
        assert_eq!(named_to_vk(NamedKey::Delete), Some((VK_DELETE, false)));
        assert_eq!(named_to_vk(NamedKey::KeypadEquals), None);
        for key in NamedKey::ALL {
            if key != NamedKey::KeypadEquals {
                assert_ne!(named_to_vk(key).map(|k| k.0), Some(0), "{key:?}");
            }
        }
    }

    #[test]
    fn extended_keys() {
        for vk in [
            VK_LEFT, VK_HOME, VK_INSERT, VK_DELETE, VK_RWIN, VK_APPS, VK_DIVIDE, 0x2C, 0xAE,
        ] {
            assert!(is_extended_vk(vk), "{vk:#x}");
        }
        for vk in [
            VK_RETURN,
            VK_SHIFT,
            VK_NUMPAD0,
            VK_F1,
            u16::from(b'A'),
            0x2F,
        ] {
            assert!(!is_extended_vk(vk), "{vk:#x}");
        }
    }

    #[test]
    fn masked_alt_tap_never_leaves_alt_alone() {
        assert_eq!(
            MASKED_ALT_TAP,
            [(0x12, false), (0xE8, false), (0xE8, true), (0x12, true)]
        );
    }
}
