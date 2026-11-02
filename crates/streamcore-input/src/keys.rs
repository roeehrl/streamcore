//! Named keys, USB HID usages (page 0x07) and the US key positions used as the
//! `hid` fallback and for hosts that predate `keyShortcut`.

use serde::{Deserialize, Serialize};

use crate::modifiers::{self, HostOs};

/// Keys that type no character (`KeySpec.named`). The serde names are the wire names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum NamedKey {
    Escape,
    Tab,
    Return,
    Backspace,
    /// Forward delete.
    Delete,
    Space,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    CapsLock,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,
    F21,
    F22,
    F23,
    F24,
    Keypad0,
    Keypad1,
    Keypad2,
    Keypad3,
    Keypad4,
    Keypad5,
    Keypad6,
    Keypad7,
    Keypad8,
    Keypad9,
    KeypadPlus,
    KeypadMinus,
    KeypadMultiply,
    KeypadDivide,
    KeypadDecimal,
    KeypadEnter,
    KeypadEquals,
    Help,
    Menu,
}

impl NamedKey {
    /// Every named key, in the order of Pad's input protocol v2.
    pub const ALL: [NamedKey; 59] = {
        use NamedKey::*;
        [
            Escape,
            Tab,
            Return,
            Backspace,
            Delete,
            Space,
            Left,
            Right,
            Up,
            Down,
            Home,
            End,
            PageUp,
            PageDown,
            Insert,
            CapsLock,
            F1,
            F2,
            F3,
            F4,
            F5,
            F6,
            F7,
            F8,
            F9,
            F10,
            F11,
            F12,
            F13,
            F14,
            F15,
            F16,
            F17,
            F18,
            F19,
            F20,
            F21,
            F22,
            F23,
            F24,
            Keypad0,
            Keypad1,
            Keypad2,
            Keypad3,
            Keypad4,
            Keypad5,
            Keypad6,
            Keypad7,
            Keypad8,
            Keypad9,
            KeypadPlus,
            KeypadMinus,
            KeypadMultiply,
            KeypadDivide,
            KeypadDecimal,
            KeypadEnter,
            KeypadEquals,
            Help,
            Menu,
        ]
    };

    /// The wire name ("pageUp", "f13", "keypadEnter").
    pub fn name(self) -> String {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::String(s)) => s,
            _ => String::new(),
        }
    }

    /// A wire name to a named key; `None` for unknown names (case-sensitive).
    pub fn from_name(name: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()
    }

    /// USB HID usage (page 0x07).
    pub fn hid(self) -> u16 {
        use NamedKey::*;
        let index = |list: &[NamedKey]| list.iter().position(|k| *k == self).unwrap_or(0) as u16;
        const FN_LOW: [NamedKey; 12] = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
        const FN_HIGH: [NamedKey; 12] =
            [F13, F14, F15, F16, F17, F18, F19, F20, F21, F22, F23, F24];
        const PAD: [NamedKey; 9] = [
            Keypad1, Keypad2, Keypad3, Keypad4, Keypad5, Keypad6, Keypad7, Keypad8, Keypad9,
        ];
        match self {
            Escape => 0x29,
            Tab => 0x2B,
            Return => 0x28,
            Backspace => 0x2A,
            Delete => 0x4C,
            Space => 0x2C,
            Right => 0x4F,
            Left => 0x50,
            Down => 0x51,
            Up => 0x52,
            Home => 0x4A,
            End => 0x4D,
            PageUp => 0x4B,
            PageDown => 0x4E,
            Insert => 0x49,
            CapsLock => 0x39,
            F1 | F2 | F3 | F4 | F5 | F6 | F7 | F8 | F9 | F10 | F11 | F12 => 0x3A + index(&FN_LOW),
            F13 | F14 | F15 | F16 | F17 | F18 | F19 | F20 | F21 | F22 | F23 | F24 => {
                0x68 + index(&FN_HIGH)
            }
            Keypad0 => 0x62,
            Keypad1 | Keypad2 | Keypad3 | Keypad4 | Keypad5 | Keypad6 | Keypad7 | Keypad8
            | Keypad9 => 0x59 + index(&PAD),
            KeypadPlus => 0x57,
            KeypadMinus => 0x56,
            KeypadMultiply => 0x55,
            KeypadDivide => 0x54,
            KeypadDecimal => 0x63,
            KeypadEnter => 0x58,
            KeypadEquals => 0x67,
            Help => 0x75,
            Menu => 0x76,
        }
    }
}

/// The named key at a USB HID usage; `None` for keys that type a character
/// (letters, digits, punctuation), modifier keys and usages no named key has.
/// The reverse of [`named_key_hid`].
#[uniffi::export]
pub fn named_key_for_hid(hid: u16) -> Option<NamedKey> {
    NamedKey::ALL.into_iter().find(|k| k.hid() == hid)
}

/// A wire name to a named key; `None` when unknown.
#[uniffi::export]
pub fn named_key_from_name(name: String) -> Option<NamedKey> {
    NamedKey::from_name(&name)
}

/// A named key's wire name.
#[uniffi::export]
pub fn named_key_name(key: NamedKey) -> String {
    key.name()
}

/// A named key's USB HID usage.
#[uniffi::export]
pub fn named_key_hid(key: NamedKey) -> u16 {
    key.hid()
}

/// Every named key's wire name.
#[uniffi::export]
pub fn named_key_names() -> Vec<String> {
    NamedKey::ALL.iter().map(|k| k.name()).collect()
}

/// Where a character sits on a US keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct UsKey {
    /// USB HID usage (page 0x07).
    pub hid: u16,
    /// Whether Shift is needed to type the character.
    pub shift: bool,
}

/// The US key for one ASCII character (letters, digits, punctuation, space,
/// newline as Return, tab as Tab); `None` for anything else.
pub fn us_key(c: char) -> Option<UsKey> {
    let plain = |hid| Some(UsKey { hid, shift: false });
    let shifted = |hid| Some(UsKey { hid, shift: true });
    match c {
        'a'..='z' => plain(0x04 + (c as u16 - 'a' as u16)),
        'A'..='Z' => shifted(0x04 + (c as u16 - 'A' as u16)),
        '1'..='9' => plain(0x1E + (c as u16 - '1' as u16)),
        '0' => plain(0x27),
        '!' => shifted(0x1E),
        '@' => shifted(0x1F),
        '#' => shifted(0x20),
        '$' => shifted(0x21),
        '%' => shifted(0x22),
        '^' => shifted(0x23),
        '&' => shifted(0x24),
        '*' => shifted(0x25),
        '(' => shifted(0x26),
        ')' => shifted(0x27),
        '\n' | '\r' => plain(0x28),
        '\t' => plain(0x2B),
        ' ' => plain(0x2C),
        '-' => plain(0x2D),
        '_' => shifted(0x2D),
        '=' => plain(0x2E),
        '+' => shifted(0x2E),
        '[' => plain(0x2F),
        '{' => shifted(0x2F),
        ']' => plain(0x30),
        '}' => shifted(0x30),
        '\\' => plain(0x31),
        '|' => shifted(0x31),
        ';' => plain(0x33),
        ':' => shifted(0x33),
        '\'' => plain(0x34),
        '"' => shifted(0x34),
        '`' => plain(0x35),
        '~' => shifted(0x35),
        ',' => plain(0x36),
        '<' => shifted(0x36),
        '.' => plain(0x37),
        '>' => shifted(0x37),
        '/' => plain(0x38),
        '?' => shifted(0x38),
        _ => None,
    }
}

/// The US key for a one-character string; `None` for longer strings or
/// characters that are not on a US keyboard.
#[uniffi::export]
pub fn us_key_for_char(character: String) -> Option<UsKey> {
    let mut chars = character.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => us_key(c),
        _ => None,
    }
}

/// The printing character (U+0020...U+007E) the US key at a HID usage types,
/// with or without Shift; `None` for keys that type no printing character there
/// (Return, Tab, named keys, modifiers) and unknown usages. The reverse of
/// [`us_key_for_char`].
#[uniffi::export]
pub fn us_key_for_hid(hid: u16, shift: bool) -> Option<String> {
    (' '..='~')
        .find(|&c| us_key(c) == Some(UsKey { hid, shift }))
        .map(String::from)
}

/// USB HID usages of the eight modifier keys (page 0x07). The `constants`
/// module exports them (`hid_left_control()` …).
pub const HID_LEFT_CONTROL: u16 = 0xE0;
pub const HID_LEFT_SHIFT: u16 = 0xE1;
pub const HID_LEFT_OPTION: u16 = 0xE2;
pub const HID_LEFT_COMMAND: u16 = 0xE3;
pub const HID_RIGHT_CONTROL: u16 = 0xE4;
pub const HID_RIGHT_SHIFT: u16 = 0xE5;
pub const HID_RIGHT_OPTION: u16 = 0xE6;
pub const HID_RIGHT_COMMAND: u16 = 0xE7;

/// Whether a HID usage is one of the eight modifier keys.
#[uniffi::export]
pub fn is_modifier_hid(hid: u16) -> bool {
    (HID_LEFT_CONTROL..=HID_RIGHT_COMMAND).contains(&hid)
}

/// A key as the host receives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct HostKey {
    /// USB HID usage (page 0x07).
    pub hid_usage: u16,
    /// Literal modifiers for the host.
    pub modifiers: u8,
}

/// A key from a hardware keyboard on the tablet to what the host receives: the
/// rule `LatchState.key_packet` applies (after merging latched and held
/// modifiers into `stored`). The modifiers follow Mac conventions, stored-style
/// with ⌘ as `command`, and are resolved for the host. On Windows ⌘ acts as Ctrl
/// (the "Cmd→Ctrl" mode remote-desktop apps offer), the ⌘ keys themselves
/// included, so ⌘C/⌘V/⌘Z do what a Mac user expects instead of opening
/// Win+C/Win+V/Win+Z panels.
#[uniffi::export]
pub fn hardware_key_for_host(hid: u16, stored: u8, host: HostOs) -> HostKey {
    let hid_usage = match (host, hid) {
        (HostOs::Windows, HID_LEFT_COMMAND) => HID_LEFT_CONTROL,
        (HostOs::Windows, HID_RIGHT_COMMAND) => HID_RIGHT_CONTROL,
        (_, other) => other,
    };
    HostKey {
        hid_usage,
        modifiers: modifiers::resolve_modifiers(stored, host),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_round_trip_and_map_to_hid() {
        assert_eq!(NamedKey::ALL.len(), 59);
        for key in NamedKey::ALL {
            let name = key.name();
            assert_eq!(NamedKey::from_name(&name), Some(key), "{name}");
        }
        assert_eq!(NamedKey::PageUp.name(), "pageUp");
        assert_eq!(NamedKey::F13.name(), "f13");
        assert_eq!(NamedKey::Keypad0.name(), "keypad0");
        assert_eq!(NamedKey::F1.hid(), 0x3A);
        assert_eq!(NamedKey::F12.hid(), 0x45);
        assert_eq!(NamedKey::F13.hid(), 0x68);
        assert_eq!(NamedKey::F24.hid(), 0x73);
        assert_eq!(NamedKey::Keypad1.hid(), 0x59);
        assert_eq!(NamedKey::Keypad9.hid(), 0x61);
        assert_eq!(NamedKey::Delete.hid(), 0x4C);
        assert_eq!(NamedKey::from_name("PageUp"), None);
        assert_eq!(NamedKey::from_name("f25"), None);
    }

    #[test]
    fn us_positions() {
        assert_eq!(
            us_key('z'),
            Some(UsKey {
                hid: 0x1D,
                shift: false
            })
        );
        assert_eq!(
            us_key('Z'),
            Some(UsKey {
                hid: 0x1D,
                shift: true
            })
        );
        assert_eq!(
            us_key('0'),
            Some(UsKey {
                hid: 0x27,
                shift: false
            })
        );
        assert_eq!(
            us_key('+'),
            Some(UsKey {
                hid: 0x2E,
                shift: true
            })
        );
        assert_eq!(us_key('é'), None);
        assert_eq!(us_key_for_char("ab".into()), None);
        assert_eq!(us_key_for_char("w".into()).map(|k| k.hid), Some(0x1A));
    }

    fn host_key(hid_usage: u16, modifiers: u8) -> HostKey {
        HostKey {
            hid_usage,
            modifiers,
        }
    }

    #[test]
    fn hardware_command_is_control_on_windows() {
        use modifiers::{COMMAND, CONTROL, OPTION, PLATFORM_KEY, SHIFT};
        let win = HostOs::Windows;
        let mac = HostOs::Mac;
        assert_eq!(
            hardware_key_for_host(HID_LEFT_COMMAND, COMMAND, win),
            host_key(HID_LEFT_CONTROL, CONTROL)
        );
        assert_eq!(
            hardware_key_for_host(HID_RIGHT_COMMAND, COMMAND, win),
            host_key(HID_RIGHT_CONTROL, CONTROL)
        );
        assert_eq!(
            hardware_key_for_host(HID_LEFT_COMMAND, COMMAND, mac),
            host_key(HID_LEFT_COMMAND, COMMAND)
        );
        // ⌘C on Windows is Ctrl+C; the C key keeps its position.
        assert_eq!(
            hardware_key_for_host(0x06, COMMAND | SHIFT, win),
            host_key(0x06, CONTROL | SHIFT)
        );
        // Other modifier keys pass through; the platform key is the Windows key.
        assert_eq!(
            hardware_key_for_host(HID_LEFT_OPTION, OPTION, win),
            host_key(HID_LEFT_OPTION, OPTION)
        );
        assert_eq!(
            hardware_key_for_host(0x04, PLATFORM_KEY, win),
            host_key(0x04, COMMAND)
        );
        assert_eq!(
            hardware_key_for_host(0x04, PLATFORM_KEY | COMMAND, mac),
            host_key(0x04, COMMAND)
        );
        assert_eq!(hardware_key_for_host(0x04, 0xFF, mac).modifiers, 15);
    }

    #[test]
    fn modifier_hids() {
        let all = [
            HID_LEFT_CONTROL,
            HID_LEFT_SHIFT,
            HID_LEFT_OPTION,
            HID_LEFT_COMMAND,
            HID_RIGHT_CONTROL,
            HID_RIGHT_SHIFT,
            HID_RIGHT_OPTION,
            HID_RIGHT_COMMAND,
        ];
        assert_eq!(all, [0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7]);
        assert!(all.iter().all(|&h| is_modifier_hid(h)));
        assert!(!is_modifier_hid(0xDF));
        assert!(!is_modifier_hid(0xE8));
        assert!(!is_modifier_hid(0x04));
        assert!(all.iter().all(|&h| named_key_for_hid(h).is_none()));
        assert!(all.iter().all(|&h| us_key_for_hid(h, false).is_none()));
    }

    #[test]
    fn named_keys_by_hid() {
        for key in NamedKey::ALL {
            assert_eq!(named_key_for_hid(key.hid()), Some(key), "{key:?}");
        }
        assert_eq!(named_key_for_hid(0x29), Some(NamedKey::Escape));
        assert_eq!(named_key_for_hid(0x2C), Some(NamedKey::Space));
        assert_eq!(named_key_for_hid(0x3A), Some(NamedKey::F1));
        assert_eq!(named_key_for_hid(0x04), None);
        assert_eq!(named_key_for_hid(0x64), None);
        assert_eq!(named_key_for_hid(0), None);
    }

    #[test]
    fn us_characters_by_hid() {
        let char_at = |hid, shift| us_key_for_hid(hid, shift);
        assert_eq!(char_at(0x1D, false).as_deref(), Some("z"));
        assert_eq!(char_at(0x1D, true).as_deref(), Some("Z"));
        assert_eq!(char_at(0x2F, false).as_deref(), Some("["));
        assert_eq!(char_at(0x2F, true).as_deref(), Some("{"));
        assert_eq!(char_at(0x27, false).as_deref(), Some("0"));
        assert_eq!(char_at(0x1E, true).as_deref(), Some("!"));
        assert_eq!(char_at(0x2C, false).as_deref(), Some(" "));
        assert_eq!(char_at(0x2C, true), None);
        assert_eq!(char_at(0x28, false), None, "Return is a named key");
        assert_eq!(char_at(0x2B, false), None, "Tab is a named key");
        assert_eq!(char_at(0x64, false), None);
        // Every printing ASCII character comes back from its own key.
        for c in ' '..='~' {
            let key = us_key(c).unwrap_or_else(|| panic!("{c:?}"));
            assert_eq!(
                us_key_for_hid(key.hid, key.shift),
                Some(c.to_string()),
                "{c:?}"
            );
        }
    }
}
