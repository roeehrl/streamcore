//! Named values of open sets and fixed values the apps need, exported once.
//!
//! UniFFI 0.32 has no exported constants, so each is a function without
//! arguments (Swift `capabilityTextInput()`, Kotlin `capabilityTextInput()`).
//! The C ABI returns all of them from `constants`, keyed by the same camelCase
//! names, and `streamcore_input.h` defines each as `STREAMCORE_INPUT_<SNAKE_NAME>`.
//!
//! Capability names and notice codes are open sets: a peer may send names this
//! version does not know, so they stay strings; [`capability_names`] and
//! [`notice_codes`] list the known ones.
//!
//! [`capability_names`]: crate::wire::capability_names
//! [`notice_codes`]: crate::wire::notice_codes

use serde_json::Value;

use crate::keys;
use crate::profile;
use crate::wire;

/// Declares exported zero-argument functions returning constants, plus the
/// table [`constants`] builds the C ABI's `constants` result from.
macro_rules! constants {
    ($($(#[$doc:meta])* $func:ident: $ty:ty = $value:expr;)+) => {
        $(
            $(#[$doc])*
            #[uniffi::export]
            pub fn $func() -> $ty {
                <$ty>::from($value)
            }
        )+

        /// Every constant: (snake_case function name, value).
        pub fn constants() -> Vec<(&'static str, Value)> {
            vec![$((stringify!($func), Value::from($func())),)+]
        }
    };
}

constants! {
    /// "hostAwareKeys": the host reads literal modifiers (1.1+).
    capability_host_aware_keys: String = wire::CAPABILITY_HOST_AWARE_KEYS;
    /// "textInput": the host accepts `text`.
    capability_text_input: String = wire::CAPABILITY_TEXT_INPUT;
    /// "keyShortcut": the host accepts `keyShortcut`.
    capability_key_shortcut: String = wire::CAPABILITY_KEY_SHORTCUT;
    /// "layoutInfo": the host sends `layoutInfo`.
    capability_layout_info: String = wire::CAPABILITY_LAYOUT_INFO;
    /// "twoFinger": the host accepts `twoFinger` and `click` gestures.
    capability_two_finger: String = wire::CAPABILITY_TWO_FINGER;
    /// "penV2": PencilPacket phase 5 (cancel).
    capability_pen_v2: String = wire::CAPABILITY_PEN_V2;
    /// "profiles": profile sync messages.
    capability_profiles: String = wire::CAPABILITY_PROFILES;
    /// "noticeCodes": `ServerState.notices`.
    capability_notice_codes: String = wire::CAPABILITY_NOTICE_CODES;
    /// "hostStats": the host sends `hostStats`.
    capability_host_stats: String = wire::CAPABILITY_HOST_STATS;
    /// "openSetup": show the computer's setup checklist.
    capability_open_setup: String = wire::CAPABILITY_OPEN_SETUP;

    /// "permission.accessibility.missing".
    notice_accessibility_missing: String = wire::NOTICE_ACCESSIBILITY_MISSING;
    /// "permission.screenRecording.missing".
    notice_screen_recording_missing: String = wire::NOTICE_SCREEN_RECORDING_MISSING;
    /// "permission.needsRelaunch".
    notice_needs_relaunch: String = wire::NOTICE_NEEDS_RELAUNCH;
    /// "capture.windowClosed".
    notice_capture_window_closed: String = wire::NOTICE_CAPTURE_WINDOW_CLOSED;
    /// "capture.fellBack".
    notice_capture_fell_back: String = wire::NOTICE_CAPTURE_FELL_BACK;
    /// "capture.failed".
    notice_capture_failed: String = wire::NOTICE_CAPTURE_FAILED;
    /// "key.unavailable".
    notice_key_unavailable: String = wire::NOTICE_KEY_UNAVAILABLE;
    /// "gesture.fallback".
    notice_gesture_fallback: String = wire::NOTICE_GESTURE_FALLBACK;
    /// "display.asleep".
    notice_display_asleep: String = wire::NOTICE_DISPLAY_ASLEEP;
    /// "computer.locked".
    notice_computer_locked: String = wire::NOTICE_COMPUTER_LOCKED;
    /// "permission.localNetwork.missing".
    notice_local_network_missing: String = wire::NOTICE_LOCAL_NETWORK_MISSING;


    /// HID usage of the left Control key (0xE0).
    hid_left_control: u16 = keys::HID_LEFT_CONTROL;
    /// HID usage of the left Shift key (0xE1).
    hid_left_shift: u16 = keys::HID_LEFT_SHIFT;
    /// HID usage of the left Option/Alt key (0xE2).
    hid_left_option: u16 = keys::HID_LEFT_OPTION;
    /// HID usage of the left Command/Windows (GUI) key (0xE3).
    hid_left_command: u16 = keys::HID_LEFT_COMMAND;
    /// HID usage of the right Control key (0xE4).
    hid_right_control: u16 = keys::HID_RIGHT_CONTROL;
    /// HID usage of the right Shift key (0xE5).
    hid_right_shift: u16 = keys::HID_RIGHT_SHIFT;
    /// HID usage of the right Option/Alt key (0xE6).
    hid_right_option: u16 = keys::HID_RIGHT_OPTION;
    /// HID usage of the right Command/Windows (GUI) key (0xE7).
    hid_right_command: u16 = keys::HID_RIGHT_COMMAND;

    /// "2000-01-01T00:00:00Z": `updatedAt` of every built-in preset, so any
    /// edit wins over it.
    preset_updated_at: String = profile::PRESET_UPDATED_AT;
    /// "2000-01-02T00:00:00Z": `updatedAt` a client gives migrated profiles that
    /// carry no edit time: newer than a preset, older than any real edit.
    migrated_updated_at: String = profile::MIGRATED_UPDATED_AT;
    /// "*": the app key of the profile for every app without its own.
    every_other_app_key: String = profile::EVERY_OTHER_APP;
}

/// A snake_case name as UniFFI's Swift and Kotlin bindings spell it
/// ("capability_pen_v2" → "capabilityPenV2").
pub fn camel_case(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    let mut upper = false;
    for c in snake.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_their_sources() {
        let all = constants();
        assert_eq!(all.len(), 32);
        let get = |name: &str| all.iter().find(|(n, _)| *n == name).unwrap().1.clone();
        assert_eq!(get("migrated_updated_at"), "2000-01-02T00:00:00Z");
        assert_eq!(get("preset_updated_at"), "2000-01-01T00:00:00Z");
        assert_eq!(get("capability_pen_v2"), "penV2");
        assert_eq!(get("hid_right_command"), 0xE7);
        let capabilities: Vec<Value> = all
            .iter()
            .filter(|(n, _)| n.starts_with("capability_"))
            .map(|(_, v)| v.clone())
            .collect();
        assert_eq!(capabilities, wire::capability_names());
        let notices: Vec<Value> = all
            .iter()
            .filter(|(n, _)| n.starts_with("notice_"))
            .map(|(_, v)| v.clone())
            .collect();
        assert_eq!(notices, wire::notice_codes());
        let hids: Vec<Value> = all
            .iter()
            .filter(|(n, _)| n.starts_with("hid_"))
            .map(|(_, v)| v.clone())
            .collect();
        assert_eq!(hids.len(), 8);
        for hid in hids {
            assert!(keys::is_modifier_hid(hid.as_u64().unwrap() as u16));
        }
    }

    #[test]
    fn camel_case_names() {
        assert_eq!(camel_case("capability_pen_v2"), "capabilityPenV2");
        assert_eq!(camel_case("hid_left_shift"), "hidLeftShift");
        assert_eq!(camel_case("version"), "version");
    }
}
