//! Pad's `Modifiers` bits, the stored-to-literal resolution for each host OS, and
//! the mapping to StreamCore/MyCNC input bits.
//!
//! Pad bits: shift 1, control 2, option/alt 4, command 8, platformKey 16.
//!
//! - **Stored** (profiles, presets, latches): `command` is the *primary* modifier,
//!   ⌘ on a Mac and Ctrl on Windows (like Electron's CommandOrControl);
//!   `platformKey` is the literal ⌘ / Windows key. Range 0...31.
//! - **Literal** (on the wire to a 1.1+ host, `hostAwareKeys`): `command` is the
//!   literal ⌘ on a Mac and the Windows key on a PC; `platformKey` never appears.
//!   Range 0...15.
//!
//! StreamCore (protocol/BINARY.md, input payloads) uses capsLock 1, shift 2,
//! control 4, option 8, command 16, function 32 and rejects other bits.

use serde::{Deserialize, Serialize};

use crate::{Result, StreamInputError};

pub const SHIFT: u8 = 1;
pub const CONTROL: u8 = 1 << 1;
pub const OPTION: u8 = 1 << 2;
pub const COMMAND: u8 = 1 << 3;
pub const PLATFORM_KEY: u8 = 1 << 4;
/// Every bit a stored (profile/latch) modifier set may use.
pub const STORED_MASK: u8 = 0x1F;
/// Every bit a literal (wire) modifier set may use.
pub const LITERAL_MASK: u8 = 0x0F;

pub const STREAM_CORE_CAPS_LOCK: u32 = 1;
pub const STREAM_CORE_SHIFT: u32 = 1 << 1;
pub const STREAM_CORE_CONTROL: u32 = 1 << 2;
pub const STREAM_CORE_OPTION: u32 = 1 << 3;
pub const STREAM_CORE_COMMAND: u32 = 1 << 4;
pub const STREAM_CORE_FUNCTION: u32 = 1 << 5;
pub const STREAM_CORE_MASK: u32 = 0x3F;

/// The operating system family of the computer being controlled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum HostOs {
    /// macOS. Also the default for hosts that do not say (`hostOS` "unknown").
    Mac,
    /// Windows.
    Windows,
}

impl HostOs {
    /// `ServerState.hostOS` ("macOS", "windows", "unknown") to a host OS. Anything
    /// other than Windows is treated as a Mac, as Pad's clients always have.
    pub fn from_name(name: &str) -> Self {
        if name.trim().eq_ignore_ascii_case("windows") {
            Self::Windows
        } else {
            Self::Mac
        }
    }
}

/// `ServerState.hostOS` to a [`HostOs`] (anything but "windows" is a Mac).
#[uniffi::export]
pub fn host_os_from_name(name: String) -> HostOs {
    HostOs::from_name(&name)
}

/// Stored modifiers to the literal modifiers sent to `host`.
///
/// Mac: primary and platform key both become ⌘. Windows: primary becomes Ctrl and
/// the platform key becomes `command` (the Windows key). Bits above 31 are dropped.
#[uniffi::export]
pub fn resolve_modifiers(stored: u8, host: HostOs) -> u8 {
    let mut out = stored & (SHIFT | CONTROL | OPTION);
    match host {
        HostOs::Mac => {
            if stored & (COMMAND | PLATFORM_KEY) != 0 {
                out |= COMMAND;
            }
        }
        HostOs::Windows => {
            if stored & COMMAND != 0 {
                out |= CONTROL;
            }
            if stored & PLATFORM_KEY != 0 {
                out |= COMMAND;
            }
        }
    }
    out
}

/// Whether these literal modifiers turn typed characters into shortcuts: ⌘ or Ctrl
/// on a Mac; Ctrl, the Windows key or Alt on Windows. Shift alone never does.
#[uniffi::export]
pub fn makes_shortcuts(literal: u8, host: HostOs) -> bool {
    match host {
        HostOs::Mac => literal & (COMMAND | CONTROL) != 0,
        HostOs::Windows => literal & (CONTROL | COMMAND | OPTION) != 0,
    }
}

/// Literal Pad modifiers to StreamCore/MyCNC input bits. Fails on `platformKey`
/// (resolve first) and on bits above 16, as StreamCore rejects unknown bits.
#[uniffi::export]
pub fn modifiers_to_stream_core(literal: u8) -> Result<u32> {
    if literal & !LITERAL_MASK != 0 {
        return Err(StreamInputError::message(format!(
            "modifiers {literal} are not literal: only shift, control, option and command map to StreamCore"
        )));
    }
    let mut bits = 0;
    if literal & SHIFT != 0 {
        bits |= STREAM_CORE_SHIFT;
    }
    if literal & CONTROL != 0 {
        bits |= STREAM_CORE_CONTROL;
    }
    if literal & OPTION != 0 {
        bits |= STREAM_CORE_OPTION;
    }
    if literal & COMMAND != 0 {
        bits |= STREAM_CORE_COMMAND;
    }
    Ok(bits)
}

/// StreamCore/MyCNC input bits to literal Pad modifiers. capsLock and function have
/// no Pad bit and are dropped; bits above 5 fail.
#[uniffi::export]
pub fn modifiers_from_stream_core(bits: u32) -> Result<u8> {
    if bits & !STREAM_CORE_MASK != 0 {
        return Err(StreamInputError::message(format!(
            "StreamCore modifier bits {bits} use bits other than 0 through 5"
        )));
    }
    let mut out = 0;
    if bits & STREAM_CORE_SHIFT != 0 {
        out |= SHIFT;
    }
    if bits & STREAM_CORE_CONTROL != 0 {
        out |= CONTROL;
    }
    if bits & STREAM_CORE_OPTION != 0 {
        out |= OPTION;
    }
    if bits & STREAM_CORE_COMMAND != 0 {
        out |= COMMAND;
    }
    Ok(out)
}

/// Stored modifiers labelled in the host's own convention: Mac glyphs in Apple's
/// order ("⌃⌥⇧⌘"), or Windows names in Microsoft's order joined with "+"
/// ("Ctrl+Alt+Shift+Win"). Empty for no modifiers.
#[uniffi::export]
pub fn modifier_label(stored: u8, host: HostOs) -> String {
    match host {
        HostOs::Mac => {
            let mut s = String::new();
            if stored & CONTROL != 0 {
                s.push('⌃');
            }
            if stored & OPTION != 0 {
                s.push('⌥');
            }
            if stored & SHIFT != 0 {
                s.push('⇧');
            }
            if stored & (COMMAND | PLATFORM_KEY) != 0 {
                s.push('⌘');
            }
            s
        }
        HostOs::Windows => {
            let mut parts = Vec::new();
            if stored & (COMMAND | CONTROL) != 0 {
                parts.push("Ctrl");
            }
            if stored & OPTION != 0 {
                parts.push("Alt");
            }
            if stored & SHIFT != 0 {
                parts.push("Shift");
            }
            if stored & PLATFORM_KEY != 0 {
                parts.push("Win");
            }
            parts.join("+")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_primary_and_platform_key() {
        assert_eq!(resolve_modifiers(COMMAND, HostOs::Mac), COMMAND);
        assert_eq!(resolve_modifiers(COMMAND, HostOs::Windows), CONTROL);
        assert_eq!(resolve_modifiers(PLATFORM_KEY, HostOs::Windows), COMMAND);
        assert_eq!(resolve_modifiers(PLATFORM_KEY, HostOs::Mac), COMMAND);
        assert_eq!(resolve_modifiers(31, HostOs::Windows), 15);
        assert_eq!(
            resolve_modifiers(COMMAND | PLATFORM_KEY, HostOs::Windows),
            10
        );
        assert_eq!(resolve_modifiers(0xFF, HostOs::Mac), 15);
    }

    #[test]
    fn shortcut_modifiers_per_host() {
        assert!(!makes_shortcuts(SHIFT, HostOs::Mac));
        assert!(!makes_shortcuts(OPTION, HostOs::Mac));
        assert!(makes_shortcuts(OPTION, HostOs::Windows));
        assert!(makes_shortcuts(COMMAND, HostOs::Windows));
        assert!(makes_shortcuts(CONTROL, HostOs::Mac));
    }

    #[test]
    fn stream_core_mapping_round_trips() {
        for literal in 0..=LITERAL_MASK {
            let bits = modifiers_to_stream_core(literal).unwrap();
            assert_eq!(modifiers_from_stream_core(bits).unwrap(), literal);
        }
        assert_eq!(
            modifiers_to_stream_core(COMMAND).unwrap(),
            STREAM_CORE_COMMAND
        );
        assert!(modifiers_to_stream_core(PLATFORM_KEY).is_err());
        assert_eq!(
            modifiers_from_stream_core(STREAM_CORE_CAPS_LOCK | STREAM_CORE_FUNCTION).unwrap(),
            0
        );
        assert!(modifiers_from_stream_core(1 << 6).is_err());
    }

    #[test]
    fn labels() {
        assert_eq!(modifier_label(COMMAND | SHIFT, HostOs::Mac), "⇧⌘");
        assert_eq!(
            modifier_label(COMMAND | SHIFT, HostOs::Windows),
            "Ctrl+Shift"
        );
        assert_eq!(
            modifier_label(PLATFORM_KEY | OPTION, HostOs::Windows),
            "Alt+Win"
        );
        assert_eq!(modifier_label(0, HostOs::Mac), "");
    }

    #[test]
    fn host_names() {
        assert_eq!(HostOs::from_name("Windows"), HostOs::Windows);
        assert_eq!(HostOs::from_name("macOS"), HostOs::Mac);
        assert_eq!(HostOs::from_name("unknown"), HostOs::Mac);
    }
}
