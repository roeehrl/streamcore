//! Host-side shortcut rules: undo and redo per front app, stored-to-literal
//! shortcut resolution, and the guard against fallbacks that close windows.

use serde::{Deserialize, Serialize};

use crate::modifiers::{self, COMMAND, CONTROL, HostOs, OPTION, SHIFT};
use crate::profile::{Profile, Shortcut, normalize_app_key};
use crate::wire::{KeyPhase, KeyShortcutMessage, KeySpec, NOTICE_KEY_UNAVAILABLE, ServerNotice};

const HID_Q: u16 = 0x14;
const HID_W: u16 = 0x1A;
const HID_Y: u16 = 0x1C;
const HID_Z: u16 = 0x1D;
const HID_F4: u16 = 0x3D;

/// Windows process names (lowercase, without `.exe`) whose redo is Ctrl+Shift+Z;
/// every other Windows app gets Ctrl+Y. From PadWin's input injector.
pub const CTRL_SHIFT_Z_REDO_APPS: [&str; 13] = [
    "krita",
    "photoshop",
    "illustrator",
    "indesign",
    "afterfx",
    "adobe premiere pro",
    "affinity",
    "photo",
    "designer",
    "publisher",
    "blender",
    "inkscape",
    "zbrush",
];

/// Whether a Windows app (exe name, process name or path) redoes with Ctrl+Shift+Z.
#[uniffi::export]
pub fn uses_ctrl_shift_z_redo(app_key: String) -> bool {
    let key = normalize_app_key(&app_key);
    let name = key.strip_suffix(".exe").unwrap_or(&key);
    CTRL_SHIFT_Z_REDO_APPS.contains(&name)
}

/// A stored shortcut (profile convention) as a literal `keyShortcut` tap for
/// `host`: the host's own key when the shortcut has one (`hosts`), else `key`.
#[uniffi::export]
pub fn resolve_shortcut(shortcut: Shortcut, host: HostOs) -> KeyShortcutMessage {
    let shortcut = shortcut.for_host(host);
    KeyShortcutMessage::tap(
        shortcut.key,
        modifiers::resolve_modifiers(shortcut.modifiers, host),
    )
}

/// The app's undo, literal: ⌘Z on a Mac, Ctrl+Z on Windows.
#[uniffi::export]
pub fn undo_shortcut(host: HostOs) -> KeyShortcutMessage {
    KeyShortcutMessage::tap(
        KeySpec::char_with_hid("z", HID_Z),
        modifiers::resolve_modifiers(COMMAND, host),
    )
}

/// The front app's redo, literal for `host`.
///
/// 1. `profile.fallbacks.redo` of the app's own profile (not the `"*"` profile,
///    whose redo cannot express the per-app difference on Windows);
/// 2. Mac: ⇧⌘Z;
/// 3. Windows: Ctrl+Shift+Z for [`CTRL_SHIFT_Z_REDO_APPS`], else Ctrl+Y.
#[uniffi::export]
pub fn redo_shortcut(
    app_key: String,
    host: HostOs,
    profile: Option<Profile>,
) -> KeyShortcutMessage {
    if let Some(redo) = profile
        .filter(|p| !p.is_every_other_app())
        .and_then(|p| p.fallbacks)
        .and_then(|f| f.redo)
    {
        return resolve_shortcut(redo, host);
    }
    match host {
        HostOs::Mac => KeyShortcutMessage::tap(KeySpec::char_with_hid("z", HID_Z), COMMAND | SHIFT),
        HostOs::Windows if uses_ctrl_shift_z_redo(app_key) => {
            KeyShortcutMessage::tap(KeySpec::char_with_hid("z", HID_Z), CONTROL | SHIFT)
        }
        HostOs::Windows => KeyShortcutMessage::tap(KeySpec::char_with_hid("y", HID_Y), CONTROL),
    }
}

/// Whether a fallback key press would close a window or quit: ⌘W or ⌘Q on a Mac
/// (whatever else is held); Ctrl+W, Ctrl+Q, Ctrl+F4 or Alt+F4 on Windows (Ctrl+Q
/// quits Krita, GIMP and Inkscape; Ctrl+F4 closes the document in most apps).
///
/// `character` is what the fallback key types on the host's layout with these
/// modifiers, when the host can tell (then it decides); otherwise the US position
/// (`hid`) decides. `modifiers` are literal.
#[uniffi::export]
pub fn is_closing_combination(
    character: Option<String>,
    hid: u16,
    modifiers: u8,
    host: HostOs,
) -> bool {
    let typed = character.map(|c| c.to_lowercase());
    let types = |letter: &str, position: u16| match &typed {
        Some(t) => t == letter,
        None => hid == position,
    };
    match host {
        HostOs::Mac => modifiers & COMMAND != 0 && (types("w", HID_W) || types("q", HID_Q)),
        HostOs::Windows => {
            (modifiers & (OPTION | CONTROL) != 0 && hid == HID_F4)
                || (modifiers & CONTROL != 0 && (types("w", HID_W) || types("q", HID_Q)))
        }
    }
}

/// What the host found when resolving a `keyShortcut` on its layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutResolution {
    /// The character was found on the current layout.
    pub found_on_layout: bool,
    /// What the `hid` fallback key types on the layout with the modifiers, if known.
    #[serde(default)]
    pub fallback_types: Option<String>,
    /// The final literal modifiers, including any Shift or Option the layout added.
    pub modifiers: u8,
}

/// Whether the host posts a resolved `keyShortcut`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct GuardDecision {
    pub post: bool,
    /// Post the `hid` position instead of a layout key.
    pub use_hid: bool,
    /// `key.unavailable` (`char`, `layout`) when the key is not posted.
    #[serde(default)]
    pub notice: Option<ServerNotice>,
}

/// The shortcut guard (Pad's input protocol v2, Key shortcut). Named keys, raw HID
/// positions and characters found on the layout are posted. A character the
/// layout lacks falls back to `hid`, unless there is none or the fallback would
/// close a window or quit ([`is_closing_combination`]); then nothing is posted and
/// the host reports `key.unavailable`. A key-up is always let through so a held
/// key cannot stick.
#[uniffi::export]
pub fn guard_shortcut(
    message: KeyShortcutMessage,
    resolution: ShortcutResolution,
    host: HostOs,
    layout_name: String,
) -> GuardDecision {
    let post = |use_hid| GuardDecision {
        post: true,
        use_hid,
        notice: None,
    };
    let Some(character) = message.key.char_value().map(str::to_owned) else {
        return post(message.key.named_key().is_none() && message.key.hid.is_some());
    };
    if resolution.found_on_layout {
        return post(false);
    }
    let blocked = GuardDecision {
        post: false,
        use_hid: false,
        notice: Some(ServerNotice::new(
            NOTICE_KEY_UNAVAILABLE,
            &[("char", &character), ("layout", &layout_name)],
        )),
    };
    match message.key.hid {
        None => blocked,
        Some(hid)
            if message.phase != KeyPhase::Up
                && is_closing_combination(
                    resolution.fallback_types,
                    hid,
                    resolution.modifiers,
                    host,
                ) =>
        {
            blocked
        }
        Some(_) => post(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::decode_profile;

    #[test]
    fn redo_per_host_and_app() {
        let r = redo_shortcut("krita.exe".into(), HostOs::Windows, None);
        assert_eq!((r.key.character.as_deref(), r.modifiers), (Some("z"), 3));
        let r = redo_shortcut("C:\\Apps\\Notepad.exe".into(), HostOs::Windows, None);
        assert_eq!((r.key.character.as_deref(), r.modifiers), (Some("y"), 2));
        let r = redo_shortcut("com.apple.TextEdit".into(), HostOs::Mac, None);
        assert_eq!(r.modifiers, 9);
        let p = decode_profile(r#"{"schema":1,"appKey":"x.exe","name":"X","updatedAt":"2026-01-01T00:00:00Z","groups":[],"fallbacks":{"redo":{"key":{"char":"y"},"modifiers":9}}}"#.into()).unwrap();
        let r = redo_shortcut("x.exe".into(), HostOs::Windows, Some(p.clone()));
        assert_eq!((r.key.character.as_deref(), r.modifiers), (Some("y"), 3));
        let mut any = p;
        any.app_key = "*".into();
        let r = redo_shortcut("notepad.exe".into(), HostOs::Windows, Some(any));
        assert_eq!((r.key.character.as_deref(), r.modifiers), (Some("y"), 2));
        assert_eq!(undo_shortcut(HostOs::Windows).modifiers, 2);
        assert_eq!(undo_shortcut(HostOs::Mac).modifiers, 8);
    }

    #[test]
    fn closing_combinations() {
        assert!(is_closing_combination(Some("w".into()), 29, 8, HostOs::Mac));
        assert!(is_closing_combination(None, 20, 8, HostOs::Mac));
        assert!(!is_closing_combination(
            Some("w".into()),
            26,
            2,
            HostOs::Mac
        ));
        assert!(is_closing_combination(None, 61, 4, HostOs::Windows));
        assert!(!is_closing_combination(
            Some("w".into()),
            26,
            8,
            HostOs::Windows
        ));
        assert!(is_closing_combination(
            Some("W".into()),
            26,
            2,
            HostOs::Windows
        ));
    }

    #[test]
    fn guard_decisions() {
        let m = KeyShortcutMessage::tap(KeySpec::char_with_hid("z", 29), 8);
        let r = |found, types: Option<&str>| ShortcutResolution {
            found_on_layout: found,
            fallback_types: types.map(str::to_owned),
            modifiers: 8,
        };
        assert!(guard_shortcut(m.clone(), r(true, None), HostOs::Mac, "French".into()).post);
        let d = guard_shortcut(m.clone(), r(false, Some("w")), HostOs::Mac, "French".into());
        assert!(!d.post);
        let n = d.notice.unwrap();
        assert_eq!(n.code, "key.unavailable");
        assert_eq!(n.params["char"], "z");
        assert_eq!(n.params["layout"], "French");
        let d = guard_shortcut(m, r(false, Some("y")), HostOs::Mac, "German".into());
        assert!(d.post && d.use_hid);
        let no_hid = KeyShortcutMessage::tap(KeySpec::char("é"), 8);
        assert!(!guard_shortcut(no_hid, r(false, None), HostOs::Mac, "US".into()).post);
        let named = KeyShortcutMessage::tap(KeySpec::named(crate::keys::NamedKey::F4), 4);
        assert!(guard_shortcut(named, r(false, None), HostOs::Windows, "US".into()).post);
    }
}
