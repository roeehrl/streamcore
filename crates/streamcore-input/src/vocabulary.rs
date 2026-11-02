//! The closed vocabularies of the profile format (Pad's profile
//! schema) as enums, so Swift and Kotlin can
//! switch over them exhaustively.
//!
//! Profiles stay open: a stored or received profile keeps every value as the
//! string it was written with (a value from a newer version survives a round trip
//! and does nothing). These enums are a typed view of those strings:
//! `*_from_name` returns `None` for a value this version does not know, and
//! `*_name` gives the wire name back. In the C ABI the values are their wire
//! names; `profile.vocabularies` lists them.
//!
//! | Vocabulary | Profile member | Values |
//! | --- | --- | --- |
//! | [`ActionType`] | `action.type` | shortcut, text, click, hold, pad, none |
//! | [`PadAction`] | `action.pad` | eraserToggle, previousTool, hover, toggleControls, keyboard, fit, zoomViewReset, undo, redo |
//! | [`MouseButton`] | `action.button` (and the `click` gesture) | left, right, middle |
//! | [`KeyColor`] | `quickKey.color` | neutral, red, blue, green, amber, purple |
//! | [`TwoFingerMode`] | `gestures.twoFinger` | sendToApp, zoomView |
//! | [`GestureDelivery`] | `gestures.delivery` | native, shortcuts, scroll |
//! | [`PanDelivery`] | `gestures.pan` | scroll, middleDrag, spaceDrag |

use serde::{Deserialize, Serialize};

/// Declares a closed vocabulary: the enum (serde uses the wire names), its
/// ordered `ALL` list, `name`/`from_name`, and the exported `$from`/`$to`.
macro_rules! vocabulary {
    (
        $(#[$doc:meta])*
        $name:ident, $key:literal, $from:ident, $to:ident {
            $($(#[$vdoc:meta])* $variant:ident = $wire:literal,)+
        }
    ) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
        pub enum $name {
            $($(#[$vdoc])* #[serde(rename = $wire)] $variant,)+
        }

        impl $name {
            /// Every value, in schema order.
            pub const ALL: &'static [$name] = &[$($name::$variant,)+];

            /// The vocabulary's name in `profile.vocabularies`.
            pub const VOCABULARY: &'static str = $key;

            /// The wire name.
            pub fn name(self) -> &'static str {
                match self {
                    $($name::$variant => $wire,)+
                }
            }

            /// A wire name to a value; `None` for a name this version does not
            /// know (case-sensitive).
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($wire => Some($name::$variant),)+
                    _ => None,
                }
            }

            /// Every wire name, in schema order.
            pub fn names() -> Vec<&'static str> {
                Self::ALL.iter().map(|v| v.name()).collect()
            }
        }

        /// A wire name to a value; `None` when this version does not know it
        /// (the profile keeps the string; the setting does nothing).
        #[uniffi::export]
        pub fn $from(name: String) -> Option<$name> {
            $name::from_name(&name)
        }

        /// The value's wire name.
        #[uniffi::export]
        pub fn $to(value: $name) -> String {
            value.name().to_owned()
        }
    };
}

vocabulary! {
    /// `action.type`: what a Quick Key, gesture or pen button does.
    ActionType, "actionType", action_type_from_name, action_type_name {
        /// Sends `action.shortcut`.
        Shortcut = "shortcut",
        /// Types `action.text`.
        Text = "text",
        /// Clicks `action.button`.
        Click = "click",
        /// Holds `action.modifiers` (or `action.shortcut`) while held.
        Hold = "hold",
        /// Runs the Pad action `action.pad` on the tablet.
        Pad = "pad",
        /// Does nothing. (Not `None`, which Swift would confuse with an absent
        /// optional.)
        NoAction = "none",
    }
}

vocabulary! {
    /// `action.pad`: something the tablet app does itself.
    PadAction, "padAction", pad_action_from_name, pad_action_name {
        EraserToggle = "eraserToggle",
        PreviousTool = "previousTool",
        Hover = "hover",
        ToggleControls = "toggleControls",
        Keyboard = "keyboard",
        Fit = "fit",
        ZoomViewReset = "zoomViewReset",
        Undo = "undo",
        Redo = "redo",
    }
}

vocabulary! {
    /// A mouse button: `action.button` in profiles and `button` of a `click`
    /// gesture.
    MouseButton, "mouseButton", mouse_button_from_name, mouse_button_name {
        Left = "left",
        Right = "right",
        Middle = "middle",
    }
}

vocabulary! {
    /// `quickKey.color`.
    KeyColor, "keyColor", key_color_from_name, key_color_name {
        Neutral = "neutral",
        Red = "red",
        Blue = "blue",
        Green = "green",
        Amber = "amber",
        Purple = "purple",
    }
}

vocabulary! {
    /// `gestures.twoFinger`: where pinch, rotate and pan go.
    TwoFingerMode, "twoFingerMode", two_finger_mode_from_name, two_finger_mode_name {
        /// To the app on the computer (`twoFinger` messages). The default.
        SendToApp = "sendToApp",
        /// They zoom the tablet's view of the picture, as in 1.2.
        ZoomView = "zoomView",
    }
}

vocabulary! {
    /// `gestures.delivery`: how the host gives two-finger gestures to the app.
    GestureDelivery, "gestureDelivery", gesture_delivery_from_name, gesture_delivery_name {
        /// Native magnify/rotate events. The default.
        Native = "native",
        /// The profile's fallback shortcuts.
        Shortcuts = "shortcuts",
        /// Ctrl/⌘ + scroll for zoom.
        Scroll = "scroll",
    }
}

vocabulary! {
    /// `gestures.pan`: how the host gives a two-finger pan to the app when the
    /// router delivers it (every delivery on a Mac; shortcuts and scroll on
    /// Windows, where native delivery is real touch).
    PanDelivery, "panDelivery", pan_delivery_from_name, pan_delivery_name {
        /// A scroll: a phased trackpad scroll on a Mac, the mouse wheel on
        /// Windows. The default.
        Scroll = "scroll",
        /// A drag with the middle mouse button held (Krita, Blender, most
        /// canvas apps pan this way; Krita zooms on the plain wheel).
        MiddleDrag = "middleDrag",
        /// A drag with the left button while Space is held (the hand tool of
        /// Krita, Photoshop, Affinity, Clip Studio Paint).
        SpaceDrag = "spaceDrag",
    }
}

/// Every closed vocabulary with its wire names, in schema order:
/// (`profile.vocabularies` key, names).
pub fn vocabularies() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        (ActionType::VOCABULARY, ActionType::names()),
        (PadAction::VOCABULARY, PadAction::names()),
        (MouseButton::VOCABULARY, MouseButton::names()),
        (KeyColor::VOCABULARY, KeyColor::names()),
        (TwoFingerMode::VOCABULARY, TwoFingerMode::names()),
        (GestureDelivery::VOCABULARY, GestureDelivery::names()),
        (PanDelivery::VOCABULARY, PanDelivery::names()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(all: &[T], name: fn(T) -> &'static str, from: fn(&str) -> Option<T>)
    where
        T: Copy + PartialEq + std::fmt::Debug + Serialize,
    {
        for &value in all {
            assert_eq!(from(name(value)), Some(value));
            assert_eq!(serde_json::to_value(value).unwrap(), name(value));
        }
    }

    #[test]
    fn names_round_trip_and_match_serde() {
        round_trip(ActionType::ALL, ActionType::name, ActionType::from_name);
        round_trip(PadAction::ALL, PadAction::name, PadAction::from_name);
        round_trip(MouseButton::ALL, MouseButton::name, MouseButton::from_name);
        round_trip(KeyColor::ALL, KeyColor::name, KeyColor::from_name);
        round_trip(
            TwoFingerMode::ALL,
            TwoFingerMode::name,
            TwoFingerMode::from_name,
        );
        round_trip(
            GestureDelivery::ALL,
            GestureDelivery::name,
            GestureDelivery::from_name,
        );
        round_trip(PanDelivery::ALL, PanDelivery::name, PanDelivery::from_name);
    }

    #[test]
    fn matches_the_profile_schema() {
        let lists: Vec<(&str, Vec<&str>)> = vocabularies();
        let expect: [(&str, &[&str]); 7] = [
            (
                "actionType",
                &["shortcut", "text", "click", "hold", "pad", "none"],
            ),
            (
                "padAction",
                &[
                    "eraserToggle",
                    "previousTool",
                    "hover",
                    "toggleControls",
                    "keyboard",
                    "fit",
                    "zoomViewReset",
                    "undo",
                    "redo",
                ],
            ),
            ("mouseButton", &["left", "right", "middle"]),
            (
                "keyColor",
                &["neutral", "red", "blue", "green", "amber", "purple"],
            ),
            ("twoFingerMode", &["sendToApp", "zoomView"]),
            ("gestureDelivery", &["native", "shortcuts", "scroll"]),
            ("panDelivery", &["scroll", "middleDrag", "spaceDrag"]),
        ];
        assert_eq!(lists.len(), expect.len());
        for ((key, names), (want_key, want)) in lists.iter().zip(expect) {
            assert_eq!(*key, want_key);
            assert_eq!(names.as_slice(), want);
        }
    }

    /// The typed view never changes the stored profile: unknown values read as
    /// `None` and still round-trip unchanged.
    #[test]
    fn unknown_values_stay_in_the_profile() {
        let text = r#"{"schema":1,"appKey":"x","name":"X","updatedAt":"2026-01-01T00:00:00Z","groups":[{"id":"g","name":"G","keys":[{"id":"k","name":"K","color":"teal","action":{"type":"macro","button":"back","pad":"lasso"}}]}],"gestures":{"twoFinger":"orbit","delivery":"haptic"}}"#;
        let profile = crate::profile::decode_profile(text.into()).unwrap();
        let key = &profile.groups[0].keys[0];
        assert_eq!(action_type_from_name(key.action.action_type.clone()), None);
        assert_eq!(
            mouse_button_from_name(key.action.button.clone().unwrap()),
            None
        );
        assert_eq!(pad_action_from_name(key.action.pad.clone().unwrap()), None);
        assert_eq!(key_color_from_name(key.color.clone().unwrap()), None);
        let gestures = profile.gestures.clone().unwrap();
        assert_eq!(
            two_finger_mode_from_name(gestures.two_finger.unwrap()),
            None
        );
        assert_eq!(gesture_delivery_from_name(gestures.delivery.unwrap()), None);
        let back: serde_json::Value =
            serde_json::from_str(&crate::profile::encode_profile(&profile)).unwrap();
        assert_eq!(
            back,
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
        let defaults = crate::profile::profile_defaults(&profile);
        assert_eq!(defaults.two_finger, "sendToApp");
        assert_eq!(defaults.delivery, "native");
    }

    #[test]
    fn unknown_and_miscased_names_are_none() {
        assert_eq!(action_type_from_name("macro".into()), None);
        assert_eq!(action_type_from_name("Shortcut".into()), None);
        assert_eq!(action_type_from_name(String::new()), None);
        assert_eq!(pad_action_from_name("lasso".into()), None);
        assert_eq!(mouse_button_from_name("back".into()), None);
        assert_eq!(key_color_from_name("teal".into()), None);
        assert_eq!(two_finger_mode_from_name("zoomview".into()), None);
        assert_eq!(gesture_delivery_from_name("haptic".into()), None);
        assert_eq!(pan_delivery_from_name("middledrag".into()), None);
        assert_eq!(pan_delivery_name(PanDelivery::MiddleDrag), "middleDrag");
        assert_eq!(
            two_finger_mode_from_name("zoomView".into()),
            Some(TwoFingerMode::ZoomView)
        );
        assert_eq!(action_type_name(ActionType::NoAction), "none");
        assert_eq!(pad_action_name(PadAction::ZoomViewReset), "zoomViewReset");
        assert_eq!(gesture_delivery_name(GestureDelivery::Scroll), "scroll");
        assert_eq!(key_color_name(KeyColor::Amber), "amber");
        assert_eq!(mouse_button_name(MouseButton::Middle), "middle");
    }
}

// Ordered wire names for native option lists, without duplicating the vocabulary.

#[uniffi::export]
pub fn action_type_names() -> Vec<String> {
    ActionType::names().into_iter().map(str::to_owned).collect()
}

#[uniffi::export]
pub fn pad_action_names() -> Vec<String> {
    PadAction::names().into_iter().map(str::to_owned).collect()
}

#[uniffi::export]
pub fn mouse_button_names() -> Vec<String> {
    MouseButton::names()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

#[uniffi::export]
pub fn key_color_names() -> Vec<String> {
    KeyColor::names().into_iter().map(str::to_owned).collect()
}

#[uniffi::export]
pub fn two_finger_mode_names() -> Vec<String> {
    TwoFingerMode::names()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

#[uniffi::export]
pub fn gesture_delivery_names() -> Vec<String> {
    GestureDelivery::names()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

#[uniffi::export]
pub fn pan_delivery_names() -> Vec<String> {
    PanDelivery::names()
        .into_iter()
        .map(str::to_owned)
        .collect()
}
