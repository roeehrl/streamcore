//! The app profile format (Pad's profile schema): one
//! app's Quick Keys, gestures and pen settings, the same in every Pad app.
//!
//! Decoding keeps everything: enumerations are open strings (a value a newer app
//! writes survives a round trip; [`crate::vocabulary`] gives the typed view, `None`
//! for a value this version does not know), unknown members are kept in `extra`, and fields
//! the schema gives a default for stay optional so a document encodes back to the
//! same JSON. [`profile_defaults`] applies the schema defaults; [`profile_issues`]
//! and [`validate_profile`] check a profile.

use serde::{Deserialize, Serialize};

use crate::keys::NamedKey;
use crate::modifiers::{HostOs, STORED_MASK};
use crate::serde_util;
use crate::time::{self, Instant};
use crate::vocabulary::{
    ActionType, GestureDelivery, KeyColor, MouseButton, PadAction, PanDelivery, TwoFingerMode,
};
use crate::wire::{KeySpec, encode_json};
use crate::{Result, StreamInputError};

/// The profile schema version this crate implements.
pub const PROFILE_SCHEMA: u32 = 1;
/// The app key of the profile for every app without its own.
pub const EVERY_OTHER_APP: &str = "*";
/// `updatedAt` of every built-in preset: any edit wins over it.
pub const PRESET_UPDATED_AT: &str = "2000-01-01T00:00:00Z";
/// `updatedAt` a client gives migrated profiles that carry no edit time: newer
/// than a preset, older than any real edit.
pub const MIGRATED_UPDATED_AT: &str = "2000-01-02T00:00:00Z";

pub const DEFAULT_ROTATE_STEP_DEGREES: f64 = 15.0;
pub const DEFAULT_ZOOM_STEP: f64 = 0.25;

/// One app's profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    /// Always 1 for this version; a profile with another value is refused.
    pub schema: u32,
    /// Bundle ID on a Mac, lowercase executable name on Windows, or "*".
    pub app_key: String,
    /// Other app keys this profile also applies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    pub name: String,
    /// ID of the built-in preset this profile started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// RFC 3339; last writer wins by it. Kept as text so it round-trips exactly.
    pub updated_at: String,
    pub groups: Vec<ProfileGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gestures: Option<ProfileGestures>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pen: Option<ProfilePen>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallbacks: Option<Fallbacks>,
    /// Members this version does not know, as a JSON object's text.
    #[serde(flatten, with = "serde_util::extra")]
    pub extra: Option<String>,
}

/// A named group of Quick Keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
pub struct ProfileGroup {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub keys: Vec<QuickKey>,
    #[serde(flatten, with = "serde_util::extra")]
    pub extra: Option<String>,
}

/// One Quick Key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
pub struct QuickKey {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Icon name from Pad's shared icon set; absent = text only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// neutral | red | blue | green | amber | purple (open; [`KeyColor`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub action: ProfileAction,
    /// Auto-repeat while held (`repeat`, default false).
    #[serde(rename = "repeat", default, skip_serializing_if = "Option::is_none")]
    pub auto_repeat: Option<bool>,
    #[serde(flatten, with = "serde_util::extra")]
    pub extra: Option<String>,
}

/// A key and stored modifiers (`command` = the primary modifier unless
/// `platformKey` is set), optionally with another key on one host OS
/// (`hosts`): an app whose shortcut differs between the Mac and Windows by
/// more than ⌘ → Ctrl says "on a Mac: this, on Windows: that". `key` and
/// `modifiers` stay the shortcut for a host without its own, and are what a
/// version before `hosts` uses (it keeps `hosts` as an unknown member).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct Shortcut {
    pub key: KeySpec,
    #[serde(default, deserialize_with = "serde_util::modifier_bits::deserialize")]
    pub modifiers: u8,
    #[serde(flatten, with = "serde_util::extra")]
    pub extra: Option<String>,
    /// Per-host keys; absent = the same everywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[uniffi(default = None)]
    pub hosts: Option<HostShortcuts>,
}

/// One host's own key for a [`Shortcut`] (stored modifiers, as in profiles).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct HostShortcut {
    pub key: KeySpec,
    #[serde(default, deserialize_with = "serde_util::modifier_bits::deserialize")]
    pub modifiers: u8,
    #[serde(flatten, with = "serde_util::extra")]
    pub extra: Option<String>,
}

/// `shortcut.hosts`: the keys for a Mac and for Windows, each optional.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, uniffi::Record)]
pub struct HostShortcuts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<HostShortcut>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<HostShortcut>,
    #[serde(flatten, with = "serde_util::extra")]
    pub extra: Option<String>,
}

impl HostShortcuts {
    /// The override for `host`, if any.
    pub fn for_host(&self, host: HostOs) -> Option<&HostShortcut> {
        match host {
            HostOs::Mac => self.mac.as_ref(),
            HostOs::Windows => self.windows.as_ref(),
        }
    }

    fn slot(&mut self, host: HostOs) -> &mut Option<HostShortcut> {
        match host {
            HostOs::Mac => &mut self.mac,
            HostOs::Windows => &mut self.windows,
        }
    }
}

impl Shortcut {
    /// A shortcut with no unknown members and no per-host keys.
    pub fn new(key: KeySpec, modifiers: u8) -> Self {
        Self {
            key,
            modifiers,
            extra: None,
            hosts: None,
        }
    }

    /// Whether some host has its own key.
    pub fn has_host_overrides(&self) -> bool {
        self.hosts
            .as_ref()
            .is_some_and(|h| h.mac.is_some() || h.windows.is_some())
    }

    /// The shortcut as `host` uses it: its own key when it has one, else `key`
    /// and `modifiers`. The result has no `hosts`.
    pub fn for_host(&self, host: HostOs) -> Shortcut {
        match self.hosts.as_ref().and_then(|h| h.for_host(host)) {
            Some(o) => Shortcut {
                key: o.key.clone(),
                modifiers: o.modifiers,
                extra: self.extra.clone(),
                hosts: None,
            },
            None => Shortcut {
                hosts: None,
                ..self.clone()
            },
        }
    }

    /// This shortcut after the person changed what it does on `host` to
    /// `edited` (as shown by [`Shortcut::for_host`]): `host`'s own key is
    /// replaced when it has one, else `key` and `modifiers`. The other host's
    /// key and unknown members are kept.
    pub fn with_host_edit(&self, edited: &Shortcut, host: HostOs) -> Shortcut {
        let mut out = self.clone();
        let own = out.hosts.as_mut().map(|h| h.slot(host));
        match own {
            Some(slot @ Some(_)) => {
                if let Some(o) = slot.as_mut() {
                    o.key = edited.key.clone();
                    o.modifiers = edited.modifiers;
                }
            }
            _ => {
                out.key = edited.key.clone();
                out.modifiers = edited.modifiers;
            }
        }
        out
    }
}

/// The shortcut as `host` uses it (its `hosts` entry for that OS, else `key`
/// and `modifiers`), with no `hosts`. Apps send and show this.
#[uniffi::export]
pub fn shortcut_for_host(shortcut: Shortcut, host: HostOs) -> Shortcut {
    shortcut.for_host(host)
}

/// Whether the shortcut has its own key on a Mac or on Windows.
#[uniffi::export]
pub fn shortcut_has_host_overrides(shortcut: &Shortcut) -> bool {
    shortcut.has_host_overrides()
}

/// `shortcut` after an editor changed what it does on `host` to `edited`
/// (see [`Shortcut::with_host_edit`]); keeps the other host's key.
#[uniffi::export]
pub fn shortcut_with_host_edit(shortcut: Shortcut, edited: Shortcut, host: HostOs) -> Shortcut {
    shortcut.with_host_edit(&edited, host)
}

/// What a Quick Key, gesture or pen button does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
pub struct ProfileAction {
    /// shortcut | text | click | hold | pad | none (open; `type` on the wire;
    /// [`ActionType`]).
    #[serde(rename = "type", default)]
    pub action_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<Shortcut>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// left | right | middle (open; [`MouseButton`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub button: Option<String>,
    /// For hold: the stored modifiers held while the key or gesture is held.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "serde_util::modifier_bits_opt::deserialize"
    )]
    pub modifiers: Option<u8>,
    /// A Pad action (open; [`PadAction`]): eraserToggle, previousTool, hover,
    /// toggleControls, keyboard, fit, zoomViewReset, undo, redo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pad: Option<String>,
    #[serde(flatten, with = "serde_util::extra")]
    pub extra: Option<String>,
}

/// Gesture settings.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct ProfileGestures {
    /// sendToApp (default) | zoomView (open; [`TwoFingerMode`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub two_finger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_zoom_locked: Option<bool>,
    /// native (default) | shortcuts | scroll (open; [`GestureDelivery`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
    /// scroll (default) | middleDrag | spaceDrag (open; [`PanDelivery`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[uniffi(default = None)]
    pub pan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tap2: Option<ProfileAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tap3: Option<ProfileAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tap4: Option<ProfileAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finger_pen1: Option<ProfileAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finger_pen2: Option<ProfileAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finger_pen3: Option<ProfileAction>,
    #[serde(flatten, with = "serde_util::extra")]
    pub extra: Option<String>,
}

/// Pen settings.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePen {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_system: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub double_tap: Option<ProfileAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squeeze: Option<ProfileAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barrel_roll_rotates_brush: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lower_button: Option<ProfileAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper_button: Option<ProfileAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eraser_end: Option<ProfileAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buttons_while_hovering: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_cancelled_strokes: Option<bool>,
    #[serde(flatten, with = "serde_util::extra")]
    pub extra: Option<String>,
}

/// Shortcuts the host uses when gestures are delivered as key presses
/// (`gestures.delivery = shortcuts`), and the app's redo.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct Fallbacks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom_in: Option<Shortcut>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom_out: Option<Shortcut>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotate_left: Option<Shortcut>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotate_right: Option<Shortcut>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_util::f64_opt"
    )]
    pub rotate_step_degrees: Option<f64>,
    /// Magnification change per zoom shortcut.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_util::f64_opt"
    )]
    pub zoom_step: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redo: Option<Shortcut>,
    #[serde(flatten, with = "serde_util::extra")]
    pub extra: Option<String>,
}

impl Fallbacks {
    /// `rotateStepDegrees`, or 15 when absent or not positive.
    pub fn rotate_step_or_default(&self) -> f64 {
        positive_or(self.rotate_step_degrees, DEFAULT_ROTATE_STEP_DEGREES)
    }

    /// `zoomStep`, or 0.25 when absent or not positive.
    pub fn zoom_step_or_default(&self) -> f64 {
        positive_or(self.zoom_step, DEFAULT_ZOOM_STEP)
    }
}

pub(crate) fn positive_or(value: Option<f64>, default: f64) -> f64 {
    match value {
        Some(v) if v.is_finite() && v > 0.0 => v,
        _ => default,
    }
}

/// A profile's settings after the schema defaults; unknown enum values count as absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDefaults {
    pub delivery: String,
    pub pan: String,
    pub two_finger: String,
    pub view_zoom_locked: bool,
    pub follow_system: bool,
    pub undo_cancelled_strokes: bool,
    pub barrel_roll_rotates_brush: bool,
    pub buttons_while_hovering: bool,
    pub rotate_step_degrees: f64,
    pub zoom_step: f64,
}

/// A known value's name, or `default`'s when the value is absent or unknown.
fn known_or<T: Copy>(
    value: Option<&String>,
    parse: fn(&str) -> Option<T>,
    default: T,
    name: fn(T) -> &'static str,
) -> String {
    name(value.and_then(|v| parse(v)).unwrap_or(default)).to_owned()
}

/// The effective settings of a profile, with schema defaults applied.
#[uniffi::export]
pub fn profile_defaults(profile: &Profile) -> ProfileDefaults {
    let g = profile.gestures.clone().unwrap_or_default();
    let p = profile.pen.clone().unwrap_or_default();
    let f = profile.fallbacks.clone().unwrap_or_default();
    ProfileDefaults {
        delivery: known_or(
            g.delivery.as_ref(),
            GestureDelivery::from_name,
            GestureDelivery::Native,
            GestureDelivery::name,
        ),
        pan: known_or(
            g.pan.as_ref(),
            PanDelivery::from_name,
            PanDelivery::Scroll,
            PanDelivery::name,
        ),
        two_finger: known_or(
            g.two_finger.as_ref(),
            TwoFingerMode::from_name,
            TwoFingerMode::SendToApp,
            TwoFingerMode::name,
        ),
        view_zoom_locked: g.view_zoom_locked.unwrap_or(false),
        follow_system: p.follow_system.unwrap_or(true),
        undo_cancelled_strokes: p.undo_cancelled_strokes.unwrap_or(true),
        barrel_roll_rotates_brush: p.barrel_roll_rotates_brush.unwrap_or(true),
        buttons_while_hovering: p.buttons_while_hovering.unwrap_or(true),
        rotate_step_degrees: f.rotate_step_or_default(),
        zoom_step: f.zoom_step_or_default(),
    }
}

impl Profile {
    /// Whether this is the profile for every other app.
    pub fn is_every_other_app(&self) -> bool {
        self.app_key.trim() == EVERY_OTHER_APP
    }

    /// The app key and aliases, as written.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.app_key).chain(self.aliases.iter().flatten())
    }

    /// The app key and aliases, normalized, without empty ones.
    pub fn normalized_keys(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for key in self.keys() {
            let k = normalize_app_key(key);
            if !k.is_empty() && !out.contains(&k) {
                out.push(k);
            }
        }
        out
    }

    /// Whether this profile applies to `app_key` (its key or an alias;
    /// case-insensitive; a Windows path reduces to its executable name).
    pub fn applies_to(&self, app_key: &str) -> bool {
        let wanted = normalize_app_key(app_key);
        !wanted.is_empty() && self.keys().any(|k| normalize_app_key(k) == wanted)
    }

    /// `updatedAt` as an instant, if readable.
    pub fn updated_instant(&self) -> Option<Instant> {
        time::parse_rfc3339(&self.updated_at)
    }

    /// Last writer wins: whether this profile was saved after `other`. A profile
    /// with an unreadable date never wins over one with a readable date.
    pub fn is_newer_than(&self, other: &Profile) -> bool {
        match (self.updated_instant(), other.updated_instant()) {
            (Some(mine), Some(theirs)) => mine > theirs,
            (Some(_), None) => true,
            _ => false,
        }
    }

    /// Every Quick Key, in group order.
    pub fn quick_keys(&self) -> impl Iterator<Item = &QuickKey> {
        self.groups.iter().flat_map(|g| g.keys.iter())
    }
}

/// An app key as profiles compare it: trimmed, a Windows path reduced to its
/// executable name, lowercase.
#[uniffi::export]
pub fn normalize_app_key(app_key: &str) -> String {
    let mut key = app_key.trim();
    let lower = key.to_ascii_lowercase();
    if key.contains('\\') || (key.contains('/') && lower.ends_with(".exe")) {
        key = key.rsplit(['\\', '/']).next().unwrap_or(key);
    }
    key.trim().to_lowercase()
}

/// Whether `profile` applies to `app_key` (its key or an alias).
#[uniffi::export]
pub fn profile_applies_to(profile: &Profile, app_key: &str) -> bool {
    profile.applies_to(app_key)
}

/// Whether two profiles claim the same app: both "*", or a shared key or alias.
#[uniffi::export]
pub fn profiles_overlap(a: &Profile, b: &Profile) -> bool {
    if a.is_every_other_app() || b.is_every_other_app() {
        return a.is_every_other_app() && b.is_every_other_app();
    }
    let keys = b.normalized_keys();
    a.normalized_keys().iter().any(|k| keys.contains(k))
}

/// Last writer wins: whether `a` was saved after `b`.
#[uniffi::export]
pub fn profile_is_newer(a: &Profile, b: &Profile) -> bool {
    a.is_newer_than(b)
}

/// One problem with a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct ProfileIssue {
    /// Where, e.g. "groups[0].keys[2].action".
    pub path: String,
    pub message: String,
    /// A value a newer version may know (open enumerations, named keys). A host
    /// keeps such profiles; strict validation refuses them.
    pub unknown_value: bool,
}

struct Checker {
    issues: Vec<ProfileIssue>,
}

impl Checker {
    fn error(&mut self, path: &str, message: String) {
        self.issues.push(ProfileIssue {
            path: path.to_owned(),
            message,
            unknown_value: false,
        });
    }

    fn unknown(&mut self, path: &str, message: String) {
        self.issues.push(ProfileIssue {
            path: path.to_owned(),
            message,
            unknown_value: true,
        });
    }

    fn open<T>(
        &mut self,
        path: &str,
        what: &str,
        value: Option<&String>,
        parse: fn(&str) -> Option<T>,
    ) {
        if let Some(v) = value
            && parse(v).is_none()
        {
            self.unknown(path, format!("unknown {what} {v:?}"));
        }
    }

    fn modifiers(&mut self, path: &str, modifiers: u8) {
        if modifiers & !STORED_MASK != 0 {
            self.error(path, format!("modifiers {modifiers} out of range 0...31"));
        }
    }

    fn key(&mut self, path: &str, key: &KeySpec) {
        let mut errors = Vec::new();
        let mut unknown = Vec::new();
        key.check(path, &mut errors, &mut unknown);
        for e in errors {
            self.error(path, e);
        }
        for u in unknown {
            self.unknown(path, u);
        }
    }

    fn shortcut(&mut self, path: &str, shortcut: &Shortcut) {
        self.key(&format!("{path}.key"), &shortcut.key);
        self.modifiers(&format!("{path}.modifiers"), shortcut.modifiers);
        if let Some(hosts) = &shortcut.hosts {
            for (name, own) in [("mac", &hosts.mac), ("windows", &hosts.windows)] {
                if let Some(o) = own {
                    self.key(&format!("{path}.hosts.{name}.key"), &o.key);
                    self.modifiers(&format!("{path}.hosts.{name}.modifiers"), o.modifiers);
                }
            }
        }
    }

    fn action(&mut self, path: &str, action: &ProfileAction) {
        if let Some(s) = &action.shortcut {
            self.shortcut(&format!("{path}.shortcut"), s);
        }
        if let Some(m) = action.modifiers {
            self.modifiers(&format!("{path}.modifiers"), m);
        }
        self.open(
            path,
            "button",
            action.button.as_ref(),
            MouseButton::from_name,
        );
        self.open(
            path,
            "pad action",
            action.pad.as_ref(),
            PadAction::from_name,
        );
        match ActionType::from_name(&action.action_type) {
            Some(ActionType::Shortcut) if action.shortcut.is_none() => {
                self.error(path, "shortcut action without shortcut".to_owned());
            }
            Some(ActionType::Text) if action.text.is_none() => {
                self.error(path, "text action without text".to_owned());
            }
            Some(ActionType::Click) if action.button.is_none() => {
                self.error(path, "click action without button".to_owned());
            }
            Some(ActionType::Hold) if action.modifiers.is_none() && action.shortcut.is_none() => {
                self.error(path, "hold action without modifiers or shortcut".to_owned());
            }
            Some(ActionType::Pad) if action.pad.is_none() => {
                self.error(path, "pad action without pad".to_owned());
            }
            Some(_) => {}
            None if action.action_type.is_empty() => {
                self.error(path, "action without type".to_owned());
            }
            None => {
                let other = &action.action_type;
                self.unknown(path, format!("unknown action type {other:?}"));
            }
        }
    }
}

/// Every problem with a profile: errors a host must refuse, and unknown values
/// (marked `unknown_value`) that a host keeps for forward compatibility.
#[uniffi::export]
pub fn profile_issues(profile: &Profile) -> Vec<ProfileIssue> {
    let mut c = Checker { issues: Vec::new() };
    if profile.schema != PROFILE_SCHEMA {
        c.error(
            "schema",
            format!("schema {} is not {PROFILE_SCHEMA}", profile.schema),
        );
    }
    if profile.app_key.trim().is_empty() {
        c.error("appKey", "appKey is empty".to_owned());
    }
    if time::parse_rfc3339(&profile.updated_at).is_none() {
        c.error(
            "updatedAt",
            format!(
                "updatedAt {:?} is not an RFC 3339 date-time",
                profile.updated_at
            ),
        );
    }
    for (gi, group) in profile.groups.iter().enumerate() {
        for (ki, key) in group.keys.iter().enumerate() {
            let path = format!("groups[{gi}].keys[{ki}]");
            c.open(&path, "color", key.color.as_ref(), KeyColor::from_name);
            c.action(&format!("{path}.action"), &key.action);
        }
    }
    if let Some(g) = &profile.gestures {
        c.open(
            "gestures.twoFinger",
            "twoFinger mode",
            g.two_finger.as_ref(),
            TwoFingerMode::from_name,
        );
        c.open(
            "gestures.delivery",
            "delivery",
            g.delivery.as_ref(),
            GestureDelivery::from_name,
        );
        c.open(
            "gestures.pan",
            "pan",
            g.pan.as_ref(),
            PanDelivery::from_name,
        );
        for (name, action) in [
            ("tap2", &g.tap2),
            ("tap3", &g.tap3),
            ("tap4", &g.tap4),
            ("fingerPen1", &g.finger_pen1),
            ("fingerPen2", &g.finger_pen2),
            ("fingerPen3", &g.finger_pen3),
        ] {
            if let Some(a) = action {
                c.action(&format!("gestures.{name}"), a);
            }
        }
    }
    if let Some(p) = &profile.pen {
        for (name, action) in [
            ("doubleTap", &p.double_tap),
            ("squeeze", &p.squeeze),
            ("lowerButton", &p.lower_button),
            ("upperButton", &p.upper_button),
            ("eraserEnd", &p.eraser_end),
        ] {
            if let Some(a) = action {
                c.action(&format!("pen.{name}"), a);
            }
        }
    }
    if let Some(f) = &profile.fallbacks {
        for (name, shortcut) in [
            ("zoomIn", &f.zoom_in),
            ("zoomOut", &f.zoom_out),
            ("rotateLeft", &f.rotate_left),
            ("rotateRight", &f.rotate_right),
            ("redo", &f.redo),
        ] {
            if let Some(s) = shortcut {
                c.shortcut(&format!("fallbacks.{name}"), s);
            }
        }
    }
    c.issues
}

/// Checks a profile. Errors always fail; unknown values fail only when `strict`.
/// The error message names the first problem.
#[uniffi::export]
pub fn validate_profile(profile: &Profile, strict: bool) -> Result<()> {
    match profile_issues(profile)
        .into_iter()
        .find(|i| strict || !i.unknown_value)
    {
        Some(issue) => Err(StreamInputError::profile(format!(
            "{}: {}",
            issue.path, issue.message
        ))),
        None => Ok(()),
    }
}

/// Decodes one profile (a `.padprofile` file or a schema document). Fails only
/// when the JSON does not have the profile's shape; use [`validate_profile`]
/// for the schema rules.
#[uniffi::export]
pub fn decode_profile(json: String) -> Result<Profile> {
    serde_util::strict::from_str(&json).map_err(|e| {
        if e.is_data() {
            StreamInputError::profile(e.to_string())
        } else {
            StreamInputError::json(e)
        }
    })
}

/// Encodes a profile as JSON (compact).
#[uniffi::export]
pub fn encode_profile(profile: &Profile) -> String {
    encode_json(profile)
}

/// A shortcut action for a named key, for building profiles in code.
pub fn named_shortcut(key: NamedKey, modifiers: u8) -> Shortcut {
    Shortcut::new(KeySpec::named(key), modifiers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> Profile {
        decode_profile(
            r#"{"schema":1,"appKey":"org.krita","aliases":["C:\\Apps\\KRITA.EXE"],"name":"K","updatedAt":"2026-01-01T00:00:00Z","groups":[]}"#.into(),
        )
        .unwrap()
    }

    #[test]
    fn round_trips_unknown_members_and_values() {
        let text = r#"{"schema":1,"appKey":"x","name":"X","updatedAt":"2026-01-01T00:00:00Z","groups":[{"id":"g","name":"G","order":2,"keys":[{"id":"k","name":"K","color":"teal","action":{"type":"macro","future":true,"shortcut":{"key":{"char":"a","layoutHint":"azerty"},"modifiers":8,"note":"kept"}}}]}],"fallbacks":{"rotateStepDegrees":15,"zoomStep":0.5,"curve":"linear"},"sync":{"a":1}}"#;
        let p = decode_profile(text.into()).unwrap();
        let back: serde_json::Value = serde_json::from_str(&encode_profile(&p)).unwrap();
        let orig: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(back, orig);
        let issues = profile_issues(&p);
        assert!(issues.iter().all(|i| i.unknown_value), "{issues:?}");
        assert_eq!(issues.len(), 2);
        assert!(validate_profile(&p, false).is_ok());
        assert!(validate_profile(&p, true).is_err());
    }

    #[test]
    fn app_keys_normalize() {
        let p = minimal();
        assert!(p.applies_to("ORG.KRITA"));
        assert!(p.applies_to("krita.exe"));
        assert!(p.applies_to("D:/Other/krita.exe"));
        assert!(!p.applies_to(""));
        assert_eq!(
            normalize_app_key(" C:\\Program Files\\Adobe\\Photoshop.exe "),
            "photoshop.exe"
        );
        assert_eq!(
            normalize_app_key("com.adobe.Photoshop"),
            "com.adobe.photoshop"
        );
    }

    #[test]
    fn refuses_bad_profiles() {
        let mut p = minimal();
        p.schema = 2;
        assert!(
            validate_profile(&p, false)
                .unwrap_err()
                .to_string()
                .contains("schema")
        );
        let mut p = minimal();
        p.updated_at = "yesterday".into();
        assert!(validate_profile(&p, false).is_err());
        assert!(
            decode_profile(r#"{"schema":1,"appKey":"x","name":"X","updatedAt":"2026-01-01T00:00:00Z","groups":[],"fallbacks":{"zoomIn":{"key":{"char":"="},"modifiers":-1}}}"#.into())
                .unwrap_err()
                .to_string()
                .contains("range")
        );
    }

    #[test]
    fn per_host_shortcuts() {
        let text = r#"{"key":{"char":"+","hid":46},"modifiers":8,"hosts":{"windows":{"key":{"char":"=","hid":46},"modifiers":8,"note":1}},"x":2}"#;
        let s: Shortcut = serde_util::strict::from_str(text).unwrap();
        let back: serde_json::Value = serde_json::from_str(&encode_json(&s)).unwrap();
        assert_eq!(
            back,
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
        assert!(s.has_host_overrides());
        let win = s.for_host(HostOs::Windows);
        assert_eq!(win.key.char_value(), Some("="));
        assert!(win.hosts.is_none());
        assert_eq!(win.extra, s.extra);
        let mac = s.for_host(HostOs::Mac);
        assert_eq!(mac.key.char_value(), Some("+"));
        // Editing on Windows changes the Windows key; on a Mac, the shared one.
        let edited = Shortcut::new(KeySpec::char("]"), 9);
        let w = s.with_host_edit(&edited, HostOs::Windows);
        assert_eq!(w.key.char_value(), Some("+"));
        assert_eq!(
            w.for_host(HostOs::Windows),
            Shortcut {
                extra: s.extra.clone(),
                ..edited.clone()
            }
        );
        assert_eq!(
            w.hosts.as_ref().unwrap().windows.as_ref().unwrap().extra,
            Some(r#"{"note":1}"#.into())
        );
        let m = s.with_host_edit(&edited, HostOs::Mac);
        assert_eq!(m.key.char_value(), Some("]"));
        assert_eq!(m.modifiers, 9);
        assert_eq!(m.hosts, s.hosts);
        assert!(!Shortcut::new(KeySpec::char("a"), 0).has_host_overrides());
        let empty = Shortcut {
            hosts: Some(HostShortcuts::default()),
            ..Shortcut::new(KeySpec::char("a"), 0)
        };
        assert!(!empty.has_host_overrides());
        assert_eq!(
            empty
                .with_host_edit(&edited, HostOs::Windows)
                .key
                .char_value(),
            Some("]")
        );
    }

    #[test]
    fn host_overrides_are_validated() {
        let p = decode_profile(r#"{"schema":1,"appKey":"x","name":"X","updatedAt":"2026-01-01T00:00:00Z","groups":[],"fallbacks":{"zoomIn":{"key":{"char":"+"},"modifiers":8,"hosts":{"mac":{"key":{"named":"warp"},"modifiers":40}}}}}"#.into()).unwrap();
        let issues = profile_issues(&p);
        assert!(
            issues
                .iter()
                .any(|i| i.path == "fallbacks.zoomIn.hosts.mac.modifiers" && !i.unknown_value),
            "{issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|i| i.path.starts_with("fallbacks.zoomIn.hosts.mac.key") && i.unknown_value),
            "{issues:?}"
        );
    }

    #[test]
    fn defaults_and_overlap() {
        let mut p = minimal();
        p.fallbacks = Some(Fallbacks {
            zoom_step: Some(-1.0),
            ..Fallbacks::default()
        });
        let d = profile_defaults(&p);
        assert_eq!(d.zoom_step, 0.25);
        assert_eq!(d.delivery, "native");
        assert_eq!(d.pan, "scroll");
        p.gestures = Some(ProfileGestures {
            pan: Some("spaceDrag".into()),
            ..ProfileGestures::default()
        });
        assert_eq!(profile_defaults(&p).pan, "spaceDrag");
        p.gestures = Some(ProfileGestures {
            pan: Some("orbit".into()),
            ..ProfileGestures::default()
        });
        assert_eq!(profile_defaults(&p).pan, "scroll");
        assert!(
            profile_issues(&p)
                .iter()
                .any(|i| i.path == "gestures.pan" && i.unknown_value)
        );
        let mut other = minimal();
        other.app_key = "krita.exe".into();
        other.aliases = None;
        assert!(profiles_overlap(&p, &other));
        other.app_key = "*".into();
        assert!(!profiles_overlap(&p, &other));
    }
}
