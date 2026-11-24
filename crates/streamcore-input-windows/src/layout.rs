//! Keyboard-layout decisions: how a shortcut named by its character is found on the layout of
//! the window that gets it, and how committed text is typed. The layout itself is behind
//! [`KeyLayout`] (Win32 on Windows, fakes in tests), so the rules are tested on every platform.

use std::fmt;

use streamcore_input::modifiers::{self, CONTROL, LITERAL_MASK, OPTION, SHIFT};
use streamcore_input::shortcuts::guard_shortcut;
use streamcore_input::{
    HostOs, KeyPhase, KeyShortcutMessage, KeySpec, NamedKey, ServerNotice, ShortcutResolution,
};

use crate::keys;

/// What one keyboard layout can tell about its keys. Characters are UTF-16 units, as Windows
/// takes them.
pub trait KeyLayout {
    /// `VkKeyScanExW`: the virtual key that types `unit` and its shift state (1 Shift, 2 Ctrl,
    /// 4 Alt); `None` when the layout has no key for it.
    fn vk_key_scan(&self, unit: u16) -> Option<(u16, u8)>;
    /// `MapVirtualKeyExW(MAPVK_VK_TO_VSC_EX)`: the key's scan code, 0xE0xx when extended, 0 if
    /// none.
    fn scan_from_vk(&self, vk: u16) -> u16;
    /// `MapVirtualKeyExW(MAPVK_VSC_TO_VK_EX)`: the virtual key at a scan code, 0 if none.
    fn vk_from_scan(&self, scan: u16) -> u16;
    /// `ToUnicodeEx`: what the key types with Shift up or down and Caps Lock off or on; `None`
    /// for a dead key or a key that types nothing.
    fn translate(&self, vk: u16, scan: u16, shift: bool, caps_lock: bool) -> Option<String>;
    /// For a dead key (one that types nothing until the next key), the accent it starts, with
    /// Shift up or down; `None` for every other key. Only a description of the layout asks; a
    /// layout that cannot tell answers `None`.
    fn dead_key(&self, _vk: u16, _scan: u16, _shift: bool) -> Option<String> {
        None
    }
    /// The layout's name, for telling the user a key is not on it.
    fn name(&self) -> String;
}

/// How a keyboard layout handle (the low 32 bits of an `HKL`) names its layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutHandle {
    /// The layout's identifier ("0000040C").
    Id(String),
    /// A variant layout, known by its "Layout Id" in the registry; `fallback` is the identifier
    /// of its language's default layout, for when the registry has no such entry. Layout Ids are
    /// unique, and the variant need not share the handle's language.
    Variant { layout_id: String, fallback: String },
}

/// Reads a layout handle: the device half names the layout, except for variants (F00x), which
/// carry a registry Layout Id there, and input-method layouts (E0xx), which are their own
/// identifier.
pub fn layout_handle(handle: u32) -> LayoutHandle {
    let device = handle >> 16;
    match device & 0xF000 {
        0xE000 => LayoutHandle::Id(format!("{handle:08X}")),
        0xF000 => LayoutHandle::Variant {
            layout_id: format!("{:04X}", device & 0x0FFF),
            fallback: format!("{:08X}", handle & 0xFFFF),
        },
        _ => LayoutHandle::Id(format!("{device:08X}")),
    }
}

/// A key as `SendInput` wants it: virtual key, scan code (low byte) and the E0 flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyStroke {
    pub vk: u16,
    pub scan: u16,
    pub extended: bool,
}

/// Why a shortcut key was not pressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyUnavailable {
    /// The key names neither a character, a known named key nor a position.
    EmptyKey,
    /// A named key this version does not know.
    UnknownKey(String),
    /// No key sits at this HID usage.
    NoKeyForHid(u16),
    /// The character is not on the layout. `would_close`: its US position is a key there, but
    /// pressing it with these modifiers would close a window or quit.
    NotOnLayout {
        character: String,
        would_close: bool,
    },
}

impl fmt::Display for KeyUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyKey => write!(f, "empty key"),
            Self::UnknownKey(name) => write!(f, "unknown key '{name}'"),
            Self::NoKeyForHid(hid) => write!(f, "no key for HID {hid:#04X}"),
            Self::NotOnLayout {
                character,
                would_close: false,
            } => write!(f, "'{character}' is not on the layout"),
            Self::NotOnLayout {
                character,
                would_close: true,
            } => write!(
                f,
                "'{character}' is not on the layout, and its US key would close a window"
            ),
        }
    }
}

/// How one shortcut is delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortcutPlan {
    /// A key with literal modifiers, any Shift, Ctrl or Alt the layout needs included.
    Key { key: KeyStroke, modifiers: u8 },
    /// The character typed as text (characters that need AltGr).
    Text(String),
    /// Nothing is pressed. `notice` is the shared guard's notice when it refused the key.
    Unavailable {
        reason: KeyUnavailable,
        notice: Option<ServerNotice>,
    },
}

impl ShortcutPlan {
    fn unavailable(reason: KeyUnavailable) -> Self {
        Self::Unavailable {
            reason,
            notice: None,
        }
    }
}

/// Resolves a shortcut on a keyboard layout.
///
/// A character is found with `VkKeyScanExW` on the layout of the window that gets it, adding any
/// Shift the layout needs. ASCII letters a layout lacks (Cyrillic, Greek) use their own virtual
/// key, which apps match shortcuts on. Anything else may fall back to the US position in `hid`,
/// which the shared guard allows unless it would close a window or quit (Ctrl+W, Ctrl+Q, Ctrl+F4,
/// Alt+F4). Characters that need AltGr are typed as text when no shortcut modifier is asked for:
/// a synthesized Ctrl+Alt is a different shortcut in many apps.
///
/// `phase` is tap or down; an up releases what its down pressed and is not planned.
pub fn plan_shortcut(
    key: &KeySpec,
    modifiers: u8,
    layout: &dyn KeyLayout,
    phase: KeyPhase,
) -> ShortcutPlan {
    let modifiers = modifiers & LITERAL_MASK;
    if let Some(name) = key.named_value() {
        return match NamedKey::from_name(name) {
            None => ShortcutPlan::unavailable(KeyUnavailable::UnknownKey(name.to_owned())),
            Some(named) => match keys::named_to_vk(named) {
                // Windows has no keypad = key.
                None => plan_char("=", None, modifiers, layout, phase),
                Some((vk, force_extended)) => ShortcutPlan::Key {
                    key: stroke(vk, layout, force_extended),
                    modifiers,
                },
            },
        };
    }
    if let Some(character) = key.char_value() {
        return plan_char(character, key.hid, modifiers, layout, phase);
    }
    match key.hid {
        Some(hid) => positional(hid, modifiers, layout)
            .unwrap_or_else(|| ShortcutPlan::unavailable(KeyUnavailable::NoKeyForHid(hid))),
        None => ShortcutPlan::unavailable(KeyUnavailable::EmptyKey),
    }
}

fn plan_char(
    text: &str,
    hid: Option<u16>,
    modifiers: u8,
    layout: &dyn KeyLayout,
    phase: KeyPhase,
) -> ShortcutPlan {
    let mut units = text.encode_utf16();
    if let (Some(unit), None) = (units.next(), units.next()) {
        let plain = |vk| ShortcutPlan::Key {
            key: stroke(vk, layout, false),
            modifiers,
        };
        if unit == u16::from(b'\n') || unit == u16::from(b'\r') {
            return plain(keys::VK_RETURN);
        }
        if unit == u16::from(b'\t') {
            return plain(keys::VK_TAB);
        }
        if let Some((vk, shift_state)) = layout.vk_key_scan(unit) {
            let alt_gr = shift_state & 6 == 6;
            if !alt_gr {
                let mut add = 0;
                if shift_state & 1 != 0 {
                    add |= SHIFT;
                }
                if shift_state & 2 != 0 {
                    add |= CONTROL;
                }
                if shift_state & 4 != 0 {
                    add |= OPTION;
                }
                return ShortcutPlan::Key {
                    key: stroke(vk, layout, false),
                    modifiers: modifiers | add,
                };
            }
            // An AltGr character: as text when nothing but Shift is asked for.
            if !modifiers::makes_shortcuts(modifiers, HostOs::Windows) {
                return ShortcutPlan::Text(text.to_owned());
            }
        } else if let Some(letter) = u8::try_from(unit).ok().filter(u8::is_ascii_alphabetic) {
            // Layouts without Latin letters keep VK_A to VK_Z, and apps match shortcuts on them.
            let add = if letter.is_ascii_uppercase() {
                SHIFT
            } else {
                0
            };
            return ShortcutPlan::Key {
                key: stroke(u16::from(letter.to_ascii_uppercase()), layout, false),
                modifiers: modifiers | add,
            };
        }
    }
    // Not on this layout: the shared guard decides whether the US position may stand in, knowing
    // what that key is here (Ctrl+W must not close a tab in place of another character).
    let fallback = hid.and_then(|h| positional(h, modifiers, layout));
    let fallback_types = match &fallback {
        Some(ShortcutPlan::Key { key, .. }) => shortcut_character(*key, modifiers, layout),
        _ => None,
    };
    let message = KeyShortcutMessage {
        key: KeySpec {
            character: Some(text.to_owned()),
            hid,
            ..KeySpec::default()
        },
        modifiers,
        phase,
        auto_repeat: false,
    };
    let resolution = ShortcutResolution {
        found_on_layout: false,
        fallback_types,
        modifiers,
    };
    let decision = guard_shortcut(message, resolution, HostOs::Windows, layout.name());
    match fallback {
        Some(plan) if decision.post && decision.use_hid => plan,
        fallback => ShortcutPlan::Unavailable {
            reason: KeyUnavailable::NotOnLayout {
                character: text.to_owned(),
                would_close: fallback.is_some() && !decision.post,
            },
            notice: decision.notice,
        },
    }
}

/// The character a key stands for in a shortcut on Windows. Accelerators match letters on the
/// virtual key, not on what the key types: VK_W is Ctrl+W to an app even on a layout where that
/// key types a Cyrillic letter. Other keys go by what they type.
fn shortcut_character(key: KeyStroke, modifiers: u8, layout: &dyn KeyLayout) -> Option<String> {
    match u8::try_from(key.vk) {
        Ok(vk) if vk.is_ascii_uppercase() => Some(char::from(vk.to_ascii_lowercase()).to_string()),
        _ => layout.translate(key.vk, key.scan, modifiers & SHIFT != 0, false),
    }
}

/// The key at a US position on this layout; `None` when there is none.
fn positional(hid: u16, modifiers: u8, layout: &dyn KeyLayout) -> Option<ShortcutPlan> {
    let mut scan = keys::hid_to_scan(hid);
    let mut vk = if scan != 0 {
        layout.vk_from_scan(scan)
    } else {
        0
    };
    if vk == 0 {
        vk = keys::hid_to_vk(hid);
    }
    if vk == 0 {
        return None;
    }
    if scan == 0 {
        scan = layout.scan_from_vk(vk);
    }
    let extended = scan & 0xFF00 == 0xE000 || (scan == 0 && keys::is_extended_vk(vk));
    Some(ShortcutPlan::Key {
        key: KeyStroke {
            vk,
            scan: scan & 0xFF,
            extended,
        },
        modifiers,
    })
}

/// A key by its virtual key, with the layout's scan code for it.
///
/// Home, End, the arrows, Page Up and Down, Insert and Delete exist twice: on the numeric keypad
/// and in the cluster beside it, told apart only by the extended flag. For their virtual keys
/// Windows answers with the keypad key's scan code and no E0 prefix, so the flag cannot be taken
/// from the layout's answer alone: without it the key is the keypad's, and with Num Lock on
/// Windows lifts a held Shift around a keypad navigation key, so Shift+Home moved the caret and
/// selected nothing. A key named by these virtual keys always means the cluster key, so it is
/// always extended.
fn stroke(vk: u16, layout: &dyn KeyLayout, force_extended: bool) -> KeyStroke {
    let scan = layout.scan_from_vk(vk);
    KeyStroke {
        vk,
        scan: scan & 0xFF,
        extended: force_extended || scan & 0xFF00 == 0xE000 || keys::is_extended_vk(vk),
    }
}

/// How one piece of committed text goes out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextStep {
    /// A key of the layout, with Shift pressed around it when `shift`.
    Key { key: KeyStroke, shift: bool },
    /// UTF-16 units as `KEYEVENTF_UNICODE` (VK_PACKET) events; a surrogate pair is one step.
    Packet(Vec<u16>),
}

impl TextStep {
    fn key(key: KeyStroke) -> Self {
        Self::Key { key, shift: false }
    }

    fn unit(unit: u16) -> Self {
        Self::Packet(vec![unit])
    }
}

/// The computer's keyboard state while text is typed: a Shift that is held (by the client or on
/// the computer itself), a held Ctrl, Alt or Windows key, and Caps Lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostKeyState {
    pub shift_held: bool,
    pub shortcut_mods_held: bool,
    pub caps_lock: bool,
}

pub const RETURN: KeyStroke = KeyStroke {
    vk: keys::VK_RETURN,
    scan: 0x1C,
    extended: false,
};
pub const TAB: KeyStroke = KeyStroke {
    vk: keys::VK_TAB,
    scan: 0x0F,
    extended: false,
};
pub const BACKSPACE: KeyStroke = KeyStroke {
    vk: keys::VK_BACK,
    scan: 0x0E,
    extended: false,
};

/// Most characters one text event may delete before typing.
const MAX_REPLACE_BEFORE: u32 = 10_000;

/// Plans how text is typed: `replace_before` Backspaces, then the text.
///
/// Characters the focused window's layout has are pressed as its keys, with the Shift they need,
/// like a person typing. VK_PACKET is kept for what the layout cannot type (AltGr-only
/// characters, dead-key compositions, emoji, CJK). A key is used only when `ToUnicodeEx` confirms
/// it types exactly that character in the current Caps Lock state; a Shift the user holds is
/// never released for it; and while Ctrl, Alt or the Windows key is held, everything goes as
/// VK_PACKET so no letter becomes a shortcut. Newline is Return, tab is Tab.
pub fn plan_text(
    text: &str,
    replace_before: u32,
    layout: &dyn KeyLayout,
    state: HostKeyState,
) -> Vec<TextStep> {
    let keys_allowed = !state.shortcut_mods_held;
    let control = |key: KeyStroke, unit: u8| {
        if keys_allowed {
            TextStep::key(key)
        } else {
            TextStep::unit(u16::from(unit))
        }
    };
    let mut steps = Vec::new();
    for _ in 0..replace_before.min(MAX_REPLACE_BEFORE) {
        steps.push(control(BACKSPACE, 0x08));
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut i = 0;
    while i < units.len() {
        let unit = units[i];
        let next = units.get(i + 1).copied();
        i += 1;
        if is_high_surrogate(unit) && next.is_some_and(is_low_surrogate) {
            steps.push(TextStep::Packet(vec![unit, units[i]]));
            i += 1;
            continue;
        }
        match unit {
            // CRLF is one Return.
            0x0D if next == Some(0x0A) => {}
            0x0A | 0x0D => steps.push(control(RETURN, b'\r')),
            0x09 => steps.push(control(TAB, b'\t')),
            _ => steps.push(
                keys_allowed
                    .then(|| layout_key(unit, layout, state))
                    .flatten()
                    .unwrap_or_else(|| TextStep::unit(unit)),
            ),
        }
    }
    steps
}

fn is_high_surrogate(unit: u16) -> bool {
    (0xD800..0xDC00).contains(&unit)
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xDC00..0xE000).contains(&unit)
}

/// The layout key that types `unit` in this state, if there is one.
fn layout_key(unit: u16, layout: &dyn KeyLayout, state: HostKeyState) -> Option<TextStep> {
    let is_control = unit < 0x20 || (0x7F..0xA0).contains(&unit);
    if is_control || is_high_surrogate(unit) || is_low_surrogate(unit) {
        return None;
    }
    let (vk, shift_state) = layout.vk_key_scan(unit)?;
    if shift_state & !1 != 0 {
        // Needs Ctrl or Alt (AltGr) or a special shift: typed as a character.
        return None;
    }
    let scan = layout.scan_from_vk(vk);
    let key = KeyStroke {
        vk,
        scan: scan & 0xFF,
        extended: scan & 0xFF00 == 0xE000,
    };
    let want = String::from_utf16(&[unit]).ok()?;
    let types = |shift| layout.translate(vk, scan, shift, state.caps_lock).as_ref() == Some(&want);
    let need_shift = shift_state & 1 != 0;
    if state.shift_held {
        // A held Shift stays held, so the key has to type the character with Shift down.
        return types(true).then(|| TextStep::key(key));
    }
    // Caps Lock can flip what Shift does (letters, and on some layouts the digit row).
    [need_shift, !need_shift]
        .into_iter()
        .find(|&shift| types(shift))
        .map(|shift| TextStep::Key { key, shift })
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::collections::HashMap;

    /// A layout described by its character keys: (character, virtual key, scan code, shift
    /// state). Letters follow Caps Lock.
    pub(crate) struct FakeLayout {
        pub name: &'static str,
        pub keys: Vec<(char, u16, u16, u8)>,
        /// Scan codes of keys that type nothing here, by virtual key (Return, arrows).
        pub scans: HashMap<u16, u16>,
    }

    impl FakeLayout {
        pub(crate) fn new(name: &'static str, keys: &[(char, u16, u16, u8)]) -> Self {
            let scans = HashMap::from([
                (keys::VK_RETURN, 0x1C),
                (keys::VK_TAB, 0x0F),
                (keys::VK_BACK, 0x0E),
                (keys::VK_F1 + 3, 0x3E),
                (keys::VK_NUMPAD0 + 5, 0x4C),
                // As Windows answers for the navigation keys: the scan code of the keypad key
                // that doubles as each, with no E0 prefix.
                (keys::VK_HOME, 0x47),
                (keys::VK_UP, 0x48),
                (keys::VK_PRIOR, 0x49),
                (keys::VK_LEFT, 0x4B),
                (keys::VK_RIGHT, 0x4D),
                (keys::VK_END, 0x4F),
                (keys::VK_DOWN, 0x50),
                (keys::VK_NEXT, 0x51),
                (keys::VK_INSERT, 0x52),
                (keys::VK_DELETE, 0x53),
                (keys::VK_DIVIDE, 0x35),
            ]);
            Self {
                name,
                keys: keys.to_vec(),
                scans,
            }
        }

        /// US QWERTY: letters, digits and a little punctuation.
        pub(crate) fn us() -> Self {
            let mut keys = Vec::new();
            for letter in b'a'..=b'z' {
                let hid = 0x04 + u16::from(letter - b'a');
                let (vk, scan) = (keys::hid_to_vk(hid), keys::hid_to_scan(hid));
                keys.push((char::from(letter), vk, scan, 0));
                keys.push((char::from(letter.to_ascii_uppercase()), vk, scan, 1));
            }
            for (plain, shifted, hid) in [
                ('1', '!', 0x1E),
                ('2', '@', 0x1F),
                ('3', '#', 0x20),
                ('=', '+', 0x2E),
                ('[', '{', 0x2F),
                (' ', ' ', 0x2C),
            ] {
                let (vk, scan) = (keys::hid_to_vk(hid), keys::hid_to_scan(hid));
                keys.push((plain, vk, scan, 0));
                if shifted != plain {
                    keys.push((shifted, vk, scan, 1));
                }
            }
            Self::new("US", &keys)
        }
    }

    impl KeyLayout for FakeLayout {
        fn vk_key_scan(&self, unit: u16) -> Option<(u16, u8)> {
            let c = char::from_u32(u32::from(unit))?;
            self.keys.iter().find(|k| k.0 == c).map(|k| (k.1, k.3))
        }

        fn scan_from_vk(&self, vk: u16) -> u16 {
            self.keys
                .iter()
                .find(|k| k.1 == vk)
                .map(|k| k.2)
                .or_else(|| self.scans.get(&vk).copied())
                .unwrap_or(0)
        }

        fn vk_from_scan(&self, scan: u16) -> u16 {
            self.keys
                .iter()
                .find(|k| k.2 == scan)
                .map(|k| k.1)
                .or_else(|| self.scans.iter().find(|s| *s.1 == scan).map(|s| *s.0))
                .unwrap_or(0)
        }

        fn translate(&self, vk: u16, _scan: u16, shift: bool, caps_lock: bool) -> Option<String> {
            let typed = |state: u8| {
                self.keys
                    .iter()
                    .find(|k| k.1 == vk && k.3 == state)
                    .map(|k| k.0)
            };
            let plain = typed(0)?;
            let effective = if plain.is_alphabetic() {
                shift != caps_lock
            } else {
                shift
            };
            typed(u8::from(effective)).map(String::from)
        }

        fn name(&self) -> String {
            self.name.to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeLayout;
    use super::*;
    use streamcore_input::modifiers::COMMAND;

    fn key_plan(plan: ShortcutPlan) -> (u16, u16, bool, u8) {
        match plan {
            ShortcutPlan::Key { key, modifiers } => (key.vk, key.scan, key.extended, modifiers),
            other => panic!("not a key: {other:?}"),
        }
    }

    /// A layout that swaps a/q and z/w, puts digits on Shift, has "é" unshifted on the 2 key and
    /// "@" on AltGr+0, and types "w" where US has z.
    fn azerty() -> FakeLayout {
        FakeLayout::new(
            "French",
            &[
                ('a', 0x41, 0x10, 0),
                ('q', 0x51, 0x1E, 0),
                ('z', 0x5A, 0x11, 0),
                ('w', 0x57, 0x2C, 0),
                ('&', 0x31, 0x02, 0),
                ('1', 0x31, 0x02, 1),
                ('é', 0x32, 0x03, 0),
                ('2', 0x32, 0x03, 1),
                ('@', 0x30, 0x0B, 6),
            ],
        )
    }

    /// A layout with no Latin letters: the keys keep VK_A to VK_Z and type Cyrillic.
    fn cyrillic() -> FakeLayout {
        FakeLayout::new(
            "Russian",
            &[
                ('ц', 0x57, 0x11, 0),
                ('я', 0x5A, 0x2C, 0),
                ('й', 0x51, 0x10, 0),
            ],
        )
    }

    #[test]
    fn layout_handles_name_their_layout() {
        assert_eq!(
            layout_handle(0x0409_0409),
            LayoutHandle::Id("00000409".into())
        );
        assert_eq!(
            layout_handle(0x040C_040C),
            LayoutHandle::Id("0000040C".into())
        );
        assert_eq!(
            layout_handle(0xE001_0411),
            LayoutHandle::Id("E0010411".into())
        );
        assert_eq!(
            layout_handle(0xF001_0413),
            LayoutHandle::Variant {
                layout_id: "0001".into(),
                fallback: "00000413".into()
            }
        );
    }

    #[test]
    fn a_character_is_found_where_the_layout_has_it() {
        let us = FakeLayout::us();
        let z = plan_shortcut(
            &KeySpec::char_with_hid("z", 0x1D),
            CONTROL,
            &us,
            KeyPhase::Tap,
        );
        assert_eq!(key_plan(z), (0x5A, 0x2C, false, CONTROL));
        let fr = azerty();
        let z = plan_shortcut(
            &KeySpec::char_with_hid("z", 0x1D),
            CONTROL,
            &fr,
            KeyPhase::Tap,
        );
        assert_eq!(key_plan(z), (0x5A, 0x11, false, CONTROL));
    }

    #[test]
    fn the_layout_adds_the_shift_a_character_needs() {
        let one = plan_shortcut(&KeySpec::char("1"), CONTROL, &azerty(), KeyPhase::Tap);
        assert_eq!(key_plan(one), (0x31, 0x02, false, CONTROL | SHIFT));
        let plus = plan_shortcut(&KeySpec::char("+"), 0, &FakeLayout::us(), KeyPhase::Tap);
        assert_eq!(key_plan(plus), (0xBB, 0x0D, false, SHIFT));
    }

    #[test]
    fn alt_gr_characters_are_typed_unless_a_shortcut_modifier_is_asked_for() {
        let fr = azerty();
        let at = KeySpec::char("@");
        assert_eq!(
            plan_shortcut(&at, 0, &fr, KeyPhase::Tap),
            ShortcutPlan::Text("@".into())
        );
        assert_eq!(
            plan_shortcut(&at, SHIFT, &fr, KeyPhase::Tap),
            ShortcutPlan::Text("@".into())
        );
        // With Ctrl it is a shortcut, and no key of this layout makes it without AltGr.
        assert!(matches!(
            plan_shortcut(&at, CONTROL, &fr, KeyPhase::Tap),
            ShortcutPlan::Unavailable {
                reason: KeyUnavailable::NotOnLayout {
                    would_close: false,
                    ..
                },
                notice: Some(_),
            }
        ));
    }

    #[test]
    fn ascii_letters_keep_their_virtual_key_on_a_layout_without_them() {
        let ru = cyrillic();
        let z = plan_shortcut(
            &KeySpec::char_with_hid("z", 0x1D),
            CONTROL,
            &ru,
            KeyPhase::Tap,
        );
        assert_eq!(key_plan(z), (0x5A, 0x2C, false, CONTROL));
        let upper = plan_shortcut(&KeySpec::char("W"), CONTROL, &ru, KeyPhase::Tap);
        assert_eq!(key_plan(upper), (0x57, 0x11, false, CONTROL | SHIFT));
    }

    #[test]
    fn a_missing_character_falls_back_to_its_us_position() {
        // "[" is not on the layout; its US position (scan 0x1A) has no key there either, so the
        // US virtual key stands in.
        let plan = plan_shortcut(
            &KeySpec::char_with_hid("[", 0x2F),
            CONTROL,
            &azerty(),
            KeyPhase::Tap,
        );
        assert_eq!(key_plan(plan), (0xDB, 0x1A, false, CONTROL));
    }

    #[test]
    fn the_guard_refuses_a_fallback_that_would_close_a_window() {
        // "ß" is not on the layout, and the client says it sits where US has z. On this layout
        // that key is VK_W: Ctrl+W, which closes a tab.
        let fr = azerty();
        let key = KeySpec::char_with_hid("ß", 0x1D);
        let plan = plan_shortcut(&key, CONTROL, &fr, KeyPhase::Tap);
        let ShortcutPlan::Unavailable { reason, notice } = plan else {
            panic!("the fallback was pressed");
        };
        assert_eq!(
            reason,
            KeyUnavailable::NotOnLayout {
                character: "ß".into(),
                would_close: true
            }
        );
        let notice = notice.expect("the guard's notice");
        assert_eq!(notice.code, "key.unavailable");
        assert_eq!(notice.params["layout"], "French");
        // Without Ctrl the same key is harmless, and so is Ctrl with a harmless position.
        assert_eq!(
            key_plan(plan_shortcut(&key, SHIFT, &fr, KeyPhase::Tap)).0,
            0x57
        );
        let harmless = KeySpec::char_with_hid("ß", 0x14);
        assert_eq!(
            key_plan(plan_shortcut(&harmless, CONTROL, &fr, KeyPhase::Tap)).0,
            0x41
        );
    }

    #[test]
    fn the_guard_judges_letters_by_virtual_key_and_f4_by_position() {
        // VK_W types a Cyrillic letter here, and is still Ctrl+W to the app.
        let ru = cyrillic();
        let key = KeySpec::char_with_hid("ω", 0x1A);
        assert!(matches!(
            plan_shortcut(&key, CONTROL, &ru, KeyPhase::Tap),
            ShortcutPlan::Unavailable {
                reason: KeyUnavailable::NotOnLayout {
                    would_close: true,
                    ..
                },
                ..
            }
        ));
        for modifiers in [CONTROL, OPTION] {
            let f4 = KeySpec::char_with_hid("ω", 0x3D);
            assert!(matches!(
                plan_shortcut(&f4, modifiers, &ru, KeyPhase::Tap),
                ShortcutPlan::Unavailable {
                    reason: KeyUnavailable::NotOnLayout {
                        would_close: true,
                        ..
                    },
                    ..
                }
            ));
        }
        // The Windows key with W closes nothing.
        assert_eq!(
            key_plan(plan_shortcut(&key, COMMAND, &ru, KeyPhase::Tap)).0,
            0x57
        );
    }

    #[test]
    fn a_character_without_a_position_is_unavailable() {
        assert!(matches!(
            plan_shortcut(
                &KeySpec::char("é"),
                CONTROL,
                &FakeLayout::us(),
                KeyPhase::Tap
            ),
            ShortcutPlan::Unavailable {
                reason: KeyUnavailable::NotOnLayout {
                    would_close: false,
                    ..
                },
                notice: Some(_)
            }
        ));
    }

    #[test]
    fn named_keys_and_raw_positions() {
        let us = FakeLayout::us();
        let f4 = plan_shortcut(&KeySpec::named(NamedKey::F4), OPTION, &us, KeyPhase::Tap);
        assert_eq!(key_plan(f4), (0x73, 0x3E, false, OPTION));
        let left = plan_shortcut(&KeySpec::named(NamedKey::Left), 0, &us, KeyPhase::Tap);
        assert_eq!(key_plan(left), (keys::VK_LEFT, 0x4B, true, 0));
        // Keypad Enter shares Return's virtual key; only the extended flag tells them apart.
        let enter = plan_shortcut(
            &KeySpec::named(NamedKey::KeypadEnter),
            0,
            &us,
            KeyPhase::Tap,
        );
        assert_eq!(key_plan(enter), (keys::VK_RETURN, 0x1C, true, 0));
        // A key the layout gives no scan code for is extended when a real keyboard sends it so.
        let menu = plan_shortcut(&KeySpec::named(NamedKey::Menu), 0, &us, KeyPhase::Tap);
        assert_eq!(key_plan(menu), (keys::VK_APPS, 0, true, 0));
        let equals = plan_shortcut(
            &KeySpec::named(NamedKey::KeypadEquals),
            0,
            &us,
            KeyPhase::Tap,
        );
        assert_eq!(key_plan(equals), (0xBB, 0x0D, false, 0));
        let raw = KeySpec {
            hid: Some(0x50),
            ..KeySpec::default()
        };
        assert_eq!(
            key_plan(plan_shortcut(&raw, 0, &us, KeyPhase::Tap)),
            (keys::VK_LEFT, 0x4B, true, 0)
        );
        let newline = plan_shortcut(&KeySpec::char("\n"), CONTROL, &us, KeyPhase::Tap);
        assert_eq!(key_plan(newline), (keys::VK_RETURN, 0x1C, false, CONTROL));
    }

    #[test]
    fn navigation_keys_are_the_cluster_keys_whatever_the_layout_answers() {
        // The layout answers with the keypad's scan codes, unprefixed. Sent without the extended
        // flag these are keypad keys, and Windows drops a held Shift around them with Num Lock
        // on.
        let us = FakeLayout::us();
        for (named, vk, scan) in [
            (NamedKey::Home, keys::VK_HOME, 0x47),
            (NamedKey::End, keys::VK_END, 0x4F),
            (NamedKey::Left, keys::VK_LEFT, 0x4B),
            (NamedKey::Right, keys::VK_RIGHT, 0x4D),
            (NamedKey::Up, keys::VK_UP, 0x48),
            (NamedKey::Down, keys::VK_DOWN, 0x50),
            (NamedKey::PageUp, keys::VK_PRIOR, 0x49),
            (NamedKey::PageDown, keys::VK_NEXT, 0x51),
            (NamedKey::Insert, keys::VK_INSERT, 0x52),
            (NamedKey::Delete, keys::VK_DELETE, 0x53),
            (NamedKey::KeypadDivide, keys::VK_DIVIDE, 0x35),
        ] {
            for modifiers in [0, SHIFT, SHIFT | CONTROL] {
                let plan = plan_shortcut(&KeySpec::named(named), modifiers, &us, KeyPhase::Tap);
                assert_eq!(key_plan(plan), (vk, scan, true, modifiers), "{named:?}");
            }
        }
        // The keypad's own keys are not extended.
        let five = plan_shortcut(
            &KeySpec::named(NamedKey::Keypad5),
            SHIFT,
            &us,
            KeyPhase::Tap,
        );
        assert_eq!(key_plan(five), (keys::VK_NUMPAD0 + 5, 0x4C, false, SHIFT));
    }

    #[test]
    fn keys_that_name_nothing() {
        let us = FakeLayout::us();
        let unknown = KeySpec {
            named: Some("f99".into()),
            ..KeySpec::default()
        };
        assert_eq!(
            plan_shortcut(&unknown, 0, &us, KeyPhase::Tap),
            ShortcutPlan::unavailable(KeyUnavailable::UnknownKey("f99".into()))
        );
        assert_eq!(
            plan_shortcut(&KeySpec::default(), 0, &us, KeyPhase::Tap),
            ShortcutPlan::unavailable(KeyUnavailable::EmptyKey)
        );
        let nowhere = KeySpec {
            hid: Some(0x03),
            ..KeySpec::default()
        };
        assert_eq!(
            plan_shortcut(&nowhere, 0, &us, KeyPhase::Tap),
            ShortcutPlan::unavailable(KeyUnavailable::NoKeyForHid(3))
        );
        assert_eq!(
            KeyUnavailable::NoKeyForHid(3).to_string(),
            "no key for HID 0x03"
        );
    }

    fn typed(text: &str, layout: &FakeLayout, state: HostKeyState) -> Vec<TextStep> {
        plan_text(text, 0, layout, state)
    }

    fn key_step(c: char, shift: bool, layout: &FakeLayout) -> TextStep {
        let (vk, _) = layout.vk_key_scan(c as u16).expect("on the layout");
        let scan = layout.scan_from_vk(vk);
        TextStep::Key {
            key: KeyStroke {
                vk,
                scan,
                extended: false,
            },
            shift,
        }
    }

    #[test]
    fn text_on_the_layout_is_pressed_as_keys_with_their_shift() {
        let us = FakeLayout::us();
        assert_eq!(
            typed("Hi!", &us, HostKeyState::default()),
            vec![
                key_step('h', true, &us),
                key_step('i', false, &us),
                key_step('1', true, &us)
            ]
        );
    }

    #[test]
    fn text_the_layout_cannot_type_goes_as_packets() {
        let us = FakeLayout::us();
        let steps = typed("é😀", &us, HostKeyState::default());
        let emoji: Vec<u16> = "😀".encode_utf16().collect();
        assert_eq!(
            steps,
            vec![TextStep::Packet(vec![0xE9]), TextStep::Packet(emoji)]
        );
        // AltGr characters are on the layout, and still typed as characters.
        assert_eq!(
            typed("@", &azerty(), HostKeyState::default()),
            vec![TextStep::Packet(vec![0x40])]
        );
    }

    #[test]
    fn newlines_tabs_and_replacements() {
        let us = FakeLayout::us();
        let steps = plan_text("a\r\nb\n\tc\r", 2, &us, HostKeyState::default());
        assert_eq!(
            steps,
            vec![
                TextStep::key(BACKSPACE),
                TextStep::key(BACKSPACE),
                key_step('a', false, &us),
                TextStep::key(RETURN),
                key_step('b', false, &us),
                TextStep::key(RETURN),
                TextStep::key(TAB),
                key_step('c', false, &us),
                TextStep::key(RETURN),
            ]
        );
        assert_eq!(
            plan_text("", u32::MAX, &us, HostKeyState::default()).len(),
            10_000
        );
    }

    #[test]
    fn caps_lock_flips_the_shift_letters_need() {
        let us = FakeLayout::us();
        let caps = HostKeyState {
            caps_lock: true,
            ..HostKeyState::default()
        };
        assert_eq!(
            typed("aB1", &us, caps),
            vec![
                key_step('a', true, &us),
                key_step('b', false, &us),
                key_step('1', false, &us)
            ]
        );
    }

    #[test]
    fn a_held_shift_is_never_released() {
        let us = FakeLayout::us();
        let held = HostKeyState {
            shift_held: true,
            ..HostKeyState::default()
        };
        // "A" comes out with the held Shift; "a" and "1" cannot, so they are typed as characters.
        assert_eq!(
            typed("Aa1!", &us, held),
            vec![
                key_step('a', false, &us),
                TextStep::Packet(vec![0x61]),
                TextStep::Packet(vec![0x31]),
                key_step('1', false, &us),
            ]
        );
    }

    #[test]
    fn nothing_is_pressed_as_a_key_while_a_shortcut_modifier_is_held() {
        let us = FakeLayout::us();
        let held = HostKeyState {
            shortcut_mods_held: true,
            ..HostKeyState::default()
        };
        assert_eq!(
            plan_text("a\n\t", 1, &us, held),
            vec![
                TextStep::Packet(vec![0x08]),
                TextStep::Packet(vec![0x61]),
                TextStep::Packet(vec![0x0D]),
                TextStep::Packet(vec![0x09]),
            ]
        );
    }
}
