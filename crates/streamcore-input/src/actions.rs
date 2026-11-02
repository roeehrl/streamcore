//! The actions a client runs for taps, fingers resting while the pen draws, pen
//! buttons and Apple Pencil gestures, with the defaults every Pad client uses when
//! a profile leaves an action out (the schema gives these actions no default).
//!
//! | Slot | Default |
//! | --- | --- |
//! | `tap2`, `tap3`, `tap4` | undo, redo, toggle controls |
//! | `fingerPen1`, `fingerPen2`, `fingerPen3` | eraser (the stroke erases), hover, right-button stroke |
//! | `doubleTap`, `squeeze` | eraser toggle, right click |
//! | `lowerButton`, `upperButton`, `eraserEnd` | right click, middle click, eraser |
//!
//! "Eraser" is the pad action `eraserToggle`: held as a chord the stroke erases,
//! tapped it flips the tool. Unknown action types from a newer version are
//! returned as they are; a client that cannot run them does nothing.

use serde::{Deserialize, Serialize};

use crate::profile::{Profile, ProfileAction};
use crate::vocabulary::{ActionType, MouseButton, PadAction};

/// Where an action sits in a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum ActionSlot {
    Tap2,
    Tap3,
    Tap4,
    FingerPen1,
    FingerPen2,
    FingerPen3,
    DoubleTap,
    Squeeze,
    LowerButton,
    UpperButton,
    EraserEnd,
}

impl ActionSlot {
    pub const ALL: [ActionSlot; 11] = [
        Self::Tap2,
        Self::Tap3,
        Self::Tap4,
        Self::FingerPen1,
        Self::FingerPen2,
        Self::FingerPen3,
        Self::DoubleTap,
        Self::Squeeze,
        Self::LowerButton,
        Self::UpperButton,
        Self::EraserEnd,
    ];
}

fn pad(action: PadAction) -> ProfileAction {
    ProfileAction {
        action_type: ActionType::Pad.name().to_owned(),
        shortcut: None,
        text: None,
        button: None,
        modifiers: None,
        pad: Some(action.name().to_owned()),
        extra: None,
    }
}

fn click(button: MouseButton) -> ProfileAction {
    ProfileAction {
        action_type: ActionType::Click.name().to_owned(),
        shortcut: None,
        text: None,
        button: Some(button.name().to_owned()),
        modifiers: None,
        pad: None,
        extra: None,
    }
}

fn none() -> ProfileAction {
    ProfileAction {
        action_type: ActionType::NoAction.name().to_owned(),
        shortcut: None,
        text: None,
        button: None,
        modifiers: None,
        pad: None,
        extra: None,
    }
}

/// The action a slot has when the profile leaves it out.
#[uniffi::export]
pub fn default_action(slot: ActionSlot) -> ProfileAction {
    match slot {
        ActionSlot::Tap2 => pad(PadAction::Undo),
        ActionSlot::Tap3 => pad(PadAction::Redo),
        ActionSlot::Tap4 => pad(PadAction::ToggleControls),
        ActionSlot::FingerPen1 | ActionSlot::DoubleTap | ActionSlot::EraserEnd => {
            pad(PadAction::EraserToggle)
        }
        ActionSlot::FingerPen2 => pad(PadAction::Hover),
        ActionSlot::FingerPen3 | ActionSlot::Squeeze | ActionSlot::LowerButton => {
            click(MouseButton::Right)
        }
        ActionSlot::UpperButton => click(MouseButton::Middle),
    }
}

/// The profile's action for a slot, or the slot's default when it has none.
#[uniffi::export]
pub fn effective_action(profile: &Profile, slot: ActionSlot) -> ProfileAction {
    let g = profile.gestures.as_ref();
    let p = profile.pen.as_ref();
    let set = match slot {
        ActionSlot::Tap2 => g.and_then(|g| g.tap2.as_ref()),
        ActionSlot::Tap3 => g.and_then(|g| g.tap3.as_ref()),
        ActionSlot::Tap4 => g.and_then(|g| g.tap4.as_ref()),
        ActionSlot::FingerPen1 => g.and_then(|g| g.finger_pen1.as_ref()),
        ActionSlot::FingerPen2 => g.and_then(|g| g.finger_pen2.as_ref()),
        ActionSlot::FingerPen3 => g.and_then(|g| g.finger_pen3.as_ref()),
        ActionSlot::DoubleTap => p.and_then(|p| p.double_tap.as_ref()),
        ActionSlot::Squeeze => p.and_then(|p| p.squeeze.as_ref()),
        ActionSlot::LowerButton => p.and_then(|p| p.lower_button.as_ref()),
        ActionSlot::UpperButton => p.and_then(|p| p.upper_button.as_ref()),
        ActionSlot::EraserEnd => p.and_then(|p| p.eraser_end.as_ref()),
    };
    set.cloned().unwrap_or_else(|| default_action(slot))
}

/// A tap with this many fingers: 2, 3 or 4; anything else does nothing.
#[uniffi::export]
pub fn tap_action(profile: &Profile, fingers: u8) -> ProfileAction {
    match fingers {
        2 => effective_action(profile, ActionSlot::Tap2),
        3 => effective_action(profile, ActionSlot::Tap3),
        4 => effective_action(profile, ActionSlot::Tap4),
        _ => none(),
    }
}

/// What the pen does while this many fingers rest on the screen: none for 0,
/// `fingerPen3` for three or more.
#[uniffi::export]
pub fn finger_pen_action(profile: &Profile, fingers: u8) -> ProfileAction {
    match fingers {
        0 => none(),
        1 => effective_action(profile, ActionSlot::FingerPen1),
        2 => effective_action(profile, ActionSlot::FingerPen2),
        _ => effective_action(profile, ActionSlot::FingerPen3),
    }
}

/// `pen.followSystem`, true when absent.
#[uniffi::export]
pub fn follows_system(profile: &Profile) -> bool {
    profile
        .pen
        .as_ref()
        .and_then(|p| p.follow_system)
        .unwrap_or(true)
}

/// An Apple Pencil gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum PencilGesture {
    DoubleTap,
    Squeeze,
}

/// The iPad's Apple Pencil setting (`UIPencilPreferredAction`), without UIKit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum PencilSystemPreference {
    Ignore,
    SwitchEraser,
    SwitchPrevious,
    ShowColorPalette,
    ShowInkAttributes,
    ShowContextualPalette,
    RunSystemShortcut,
    Unknown,
}

/// What a client does for a Pencil gesture.
///
/// Output only: it is serialized (the JSON method table's results) but never
/// decoded. It is internally tagged (`"type"`), and serde decodes such enums
/// from its own buffered copy of the input, which bypasses
/// `serde_util::strict`'s rule that a JSON array never stands in for an object.
/// A test fails if it gains a `Deserialize` implementation.
// UniFFI enums cannot box a variant; the size difference does not matter here.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, uniffi::Enum)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PencilResponse {
    Nothing,
    /// Swap between the pen and the eraser (the eraser is its own tool).
    EraserSwap,
    /// Back to the tool used before.
    PreviousTool,
    /// The client's palette (Quick Keys) at the Pencil.
    Palette,
    /// Run a profile action (shortcut, click at the Pencil, text…).
    Perform {
        action: ProfileAction,
    },
}

/// Double-tap or squeeze: with `pen.followSystem` (the default) the system
/// setting decides, as Apple's guidelines expect; otherwise the profile's own
/// `doubleTap` / `squeeze` (or their defaults) run.
#[uniffi::export]
pub fn pencil_gesture_response(
    profile: &Profile,
    gesture: PencilGesture,
    system: PencilSystemPreference,
) -> PencilResponse {
    use PencilSystemPreference::*;
    if follows_system(profile) {
        return match system {
            SwitchEraser => PencilResponse::EraserSwap,
            SwitchPrevious => PencilResponse::PreviousTool,
            ShowColorPalette | ShowInkAttributes | ShowContextualPalette => PencilResponse::Palette,
            // The system runs its shortcut itself; "Off" means off.
            RunSystemShortcut | Ignore | Unknown => PencilResponse::Nothing,
        };
    }
    let slot = match gesture {
        PencilGesture::DoubleTap => ActionSlot::DoubleTap,
        PencilGesture::Squeeze => ActionSlot::Squeeze,
    };
    let action = effective_action(profile, slot);
    match ActionType::from_name(&action.action_type) {
        Some(ActionType::Pad) => match action.pad.as_deref().map(PadAction::from_name) {
            Some(Some(PadAction::EraserToggle)) => PencilResponse::EraserSwap,
            Some(Some(PadAction::PreviousTool)) => PencilResponse::PreviousTool,
            // Hover is a chord, not a gesture.
            Some(Some(PadAction::Hover)) | None => PencilResponse::Nothing,
            Some(_) => PencilResponse::Perform { action },
        },
        Some(ActionType::Shortcut | ActionType::Text | ActionType::Click | ActionType::Hold) => {
            PencilResponse::Perform { action }
        }
        // "none", or a newer app's action this version cannot run.
        Some(ActionType::NoAction) | None => PencilResponse::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::decode_profile;

    fn profile(extra: &str) -> Profile {
        decode_profile(format!(
            r#"{{"schema":1,"appKey":"*","name":"A","updatedAt":"2026-01-01T00:00:00Z","groups":[]{extra}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn defaults_fill_what_the_profile_leaves_out() {
        let p = profile("");
        assert_eq!(
            effective_action(&p, ActionSlot::Tap4).pad.as_deref(),
            Some("toggleControls")
        );
        assert_eq!(
            effective_action(&p, ActionSlot::UpperButton)
                .button
                .as_deref(),
            Some("middle")
        );
        assert_eq!(finger_pen_action(&p, 5).button.as_deref(), Some("right"));
        assert_eq!(tap_action(&p, 1).action_type, "none");
        let p =
            profile(r#","gestures":{"tap4":{"type":"none"}},"pen":{"squeeze":{"type":"macro"}}"#);
        assert_eq!(tap_action(&p, 4).action_type, "none");
        assert_eq!(
            effective_action(&p, ActionSlot::Squeeze).action_type,
            "macro"
        );
    }

    #[test]
    fn pencil_follows_the_system_unless_told_not_to() {
        let p = profile("");
        assert_eq!(
            pencil_gesture_response(
                &p,
                PencilGesture::Squeeze,
                PencilSystemPreference::ShowInkAttributes
            ),
            PencilResponse::Palette
        );
        let own = profile(
            r#","pen":{"followSystem":false,"doubleTap":{"type":"pad","pad":"previousTool"},"squeeze":{"type":"macro"}}"#,
        );
        assert_eq!(
            pencil_gesture_response(
                &own,
                PencilGesture::DoubleTap,
                PencilSystemPreference::SwitchEraser
            ),
            PencilResponse::PreviousTool
        );
        assert_eq!(
            pencil_gesture_response(&own, PencilGesture::Squeeze, PencilSystemPreference::Ignore),
            PencilResponse::Nothing
        );
        let defaults = profile(r#","pen":{"followSystem":false}"#);
        assert!(matches!(
            pencil_gesture_response(
                &defaults,
                PencilGesture::Squeeze,
                PencilSystemPreference::Ignore
            ),
            PencilResponse::Perform { .. }
        ));
    }
}
