//! Pad 1.3 JSON wire types (Pad's input protocol v2) with their encoders
//! and decoders.
//!
//! Decoding rules:
//! - Unknown JSON members are ignored, missing optional members default. A JSON
//!   array is never read as an object (see `serde_util::strict`, which every
//!   decoder goes through).
//! - Messages gated by a capability decode **strictly**: unknown enum values,
//!   unknown modifier bits, missing companion fields and non-finite numbers fail,
//!   because the sender only sends them to a peer that advertised the capability
//!   and therefore understands every value (StreamCore's rule).
//! - Host-to-client state decodes **tolerantly** where the spec says so: notice
//!   parameters of the wrong shape are dropped, notices entries without a string
//!   code are skipped (notices that are not a list are none), a malformed profile
//!   in a snapshot is skipped instead of failing the snapshot. `ServerState.source`
//!   that is not an object, and a snapshot whose `profiles` is not a list, are
//!   errors. protocol/input/fixtures/wire-messages.json pins each.
//!
//! Encoding is canonical: absent optional members stay absent; members with a
//! default (`replaceBefore`, `modifiers`, `phase`, `repeat`, `labels`) are written.

use std::collections::HashMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::keys::NamedKey;
use crate::modifiers::LITERAL_MASK;
use crate::profile::Profile;
use crate::serde_util;
use crate::{Result, StreamInputError};

// MARK: - Message types and capabilities

/// The Pad message type numbers this crate has codecs for, plus the 1.2 input
/// messages they sit beside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum PadMessageType {
    /// 0x20, binary PencilPacket.
    Pencil,
    /// 0x21, JSON GestureMessage.
    Gesture,
    /// 0x22, binary KeyPacket.
    Key,
    /// 0x24, JSON TextMessage (`textInput`).
    Text,
    /// 0x25, JSON KeyShortcutMessage (`keyShortcut`).
    KeyShortcut,
    /// 0x30, JSON FrontAppMessage.
    FrontApp,
    /// 0x34, JSON AppListMessage.
    AppList,
    /// 0x48, JSON LayoutInfoMessage (`layoutInfo`).
    LayoutInfo,
    /// 0x49, JSON HostStatsMessage (`hostStats`).
    HostStats,
    /// 0x50, JSON ProfilesSnapshotMessage (`profiles`).
    ProfilesSnapshot,
    /// 0x51, JSON ProfileUpsertMessage (`profiles`).
    ProfileUpsert,
    /// 0x52, JSON ProfileDeleteMessage (`profiles`).
    ProfileDelete,
    /// 0x53, JSON ProfileChangedMessage (`profiles`).
    ProfileChanged,
    /// 0x54, JSON OpenSetupMessage (`openSetup`), from an authenticated tablet.
    OpenSetup,
}

impl PadMessageType {
    pub const ALL: [PadMessageType; 14] = [
        Self::Pencil,
        Self::Gesture,
        Self::Key,
        Self::Text,
        Self::KeyShortcut,
        Self::FrontApp,
        Self::AppList,
        Self::LayoutInfo,
        Self::HostStats,
        Self::ProfilesSnapshot,
        Self::ProfileUpsert,
        Self::ProfileDelete,
        Self::ProfileChanged,
        Self::OpenSetup,
    ];

    /// The type byte.
    pub fn code(self) -> u8 {
        match self {
            Self::Pencil => 0x20,
            Self::Gesture => 0x21,
            Self::Key => 0x22,
            Self::Text => 0x24,
            Self::KeyShortcut => 0x25,
            Self::FrontApp => 0x30,
            Self::AppList => 0x34,
            Self::LayoutInfo => 0x48,
            Self::HostStats => 0x49,
            Self::ProfilesSnapshot => 0x50,
            Self::ProfileUpsert => 0x51,
            Self::ProfileDelete => 0x52,
            Self::ProfileChanged => 0x53,
            Self::OpenSetup => 0x54,
        }
    }
}

/// A message type's byte.
#[uniffi::export]
pub fn pad_message_type_code(message_type: PadMessageType) -> u8 {
    message_type.code()
}

/// The message type for a type byte, if this crate knows it.
#[uniffi::export]
pub fn pad_message_type_from_code(code: u8) -> Option<PadMessageType> {
    PadMessageType::ALL.into_iter().find(|t| t.code() == code)
}

pub const CAPABILITY_HOST_AWARE_KEYS: &str = "hostAwareKeys";
pub const CAPABILITY_TEXT_INPUT: &str = "textInput";
pub const CAPABILITY_KEY_SHORTCUT: &str = "keyShortcut";
pub const CAPABILITY_LAYOUT_INFO: &str = "layoutInfo";
pub const CAPABILITY_TWO_FINGER: &str = "twoFinger";
pub const CAPABILITY_PEN_V2: &str = "penV2";
pub const CAPABILITY_PROFILES: &str = "profiles";
pub const CAPABILITY_NOTICE_CODES: &str = "noticeCodes";
pub const CAPABILITY_HOST_STATS: &str = "hostStats";

/// Every capability name a 1.3 host or client implements, in spec order.
pub const CAPABILITY_OPEN_SETUP: &str = "openSetup";

pub const CAPABILITIES_1_3: [&str; 10] = [
    CAPABILITY_HOST_AWARE_KEYS,
    CAPABILITY_TEXT_INPUT,
    CAPABILITY_KEY_SHORTCUT,
    CAPABILITY_LAYOUT_INFO,
    CAPABILITY_TWO_FINGER,
    CAPABILITY_PEN_V2,
    CAPABILITY_PROFILES,
    CAPABILITY_NOTICE_CODES,
    CAPABILITY_HOST_STATS,
    CAPABILITY_OPEN_SETUP,
];

/// The capability names a 1.3 app advertises (`Hello.clientCapabilities`,
/// `ServerState.capabilities`).
#[uniffi::export]
pub fn capability_names() -> Vec<String> {
    CAPABILITIES_1_3.iter().map(|s| (*s).to_owned()).collect()
}

pub const NOTICE_ACCESSIBILITY_MISSING: &str = "permission.accessibility.missing";
pub const NOTICE_SCREEN_RECORDING_MISSING: &str = "permission.screenRecording.missing";
pub const NOTICE_NEEDS_RELAUNCH: &str = "permission.needsRelaunch";
pub const NOTICE_CAPTURE_WINDOW_CLOSED: &str = "capture.windowClosed";
pub const NOTICE_CAPTURE_FELL_BACK: &str = "capture.fellBack";
pub const NOTICE_CAPTURE_FAILED: &str = "capture.failed";
pub const NOTICE_KEY_UNAVAILABLE: &str = "key.unavailable";
pub const NOTICE_GESTURE_FALLBACK: &str = "gesture.fallback";
pub const NOTICE_DISPLAY_ASLEEP: &str = "display.asleep";
pub const NOTICE_COMPUTER_LOCKED: &str = "computer.locked";

/// Every notice code in Pad's input protocol v2. Clients translate these; unknown codes
/// fall back to the English `notice` / `errorDescription`.
pub const NOTICE_LOCAL_NETWORK_MISSING: &str = "permission.localNetwork.missing";

pub const NOTICE_CODES: [&str; 11] = [
    NOTICE_ACCESSIBILITY_MISSING,
    NOTICE_SCREEN_RECORDING_MISSING,
    NOTICE_NEEDS_RELAUNCH,
    NOTICE_CAPTURE_WINDOW_CLOSED,
    NOTICE_CAPTURE_FELL_BACK,
    NOTICE_CAPTURE_FAILED,
    NOTICE_KEY_UNAVAILABLE,
    NOTICE_GESTURE_FALLBACK,
    NOTICE_DISPLAY_ASLEEP,
    NOTICE_COMPUTER_LOCKED,
    NOTICE_LOCAL_NETWORK_MISSING,
];

/// Every notice code (`ServerState.notices[].code`).
#[uniffi::export]
pub fn notice_codes() -> Vec<String> {
    NOTICE_CODES.iter().map(|s| (*s).to_owned()).collect()
}

// MARK: - Keys

/// A key named by what it types, not where it sits: `char` (one character,
/// lowercase for letters) or `named`; `hid` is the US position, used only when the
/// character can't be found on the host's layout. A spec with only `hid` names a
/// raw position. Shared by `keyShortcut` and profiles.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, uniffi::Record)]
pub struct KeySpec {
    /// One character (`char` on the wire).
    #[serde(rename = "char", default, skip_serializing_if = "Option::is_none")]
    pub character: Option<String>,
    /// A named key's wire name (see [`NamedKey`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named: Option<String>,
    /// USB HID usage (page 0x07) of the key's US position.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "serde_util::hid_opt::deserialize"
    )]
    pub hid: Option<u16>,
    /// Members this version does not know, as a JSON object's text (kept for round trips).
    #[serde(flatten, with = "serde_util::extra")]
    pub extra: Option<String>,
}

impl KeySpec {
    /// A character key.
    pub fn char(character: impl Into<String>) -> Self {
        Self {
            character: Some(character.into()),
            ..Self::default()
        }
    }

    /// A character key with its US position as fallback.
    pub fn char_with_hid(character: impl Into<String>, hid: u16) -> Self {
        Self {
            character: Some(character.into()),
            hid: Some(hid),
            ..Self::default()
        }
    }

    /// A named key.
    pub fn named(key: NamedKey) -> Self {
        Self {
            named: Some(key.name()),
            ..Self::default()
        }
    }

    /// The character, when present and non-empty.
    pub fn char_value(&self) -> Option<&str> {
        self.character.as_deref().filter(|s| !s.is_empty())
    }

    /// The named key's wire name, when present and non-empty.
    pub fn named_value(&self) -> Option<&str> {
        self.named.as_deref().filter(|s| !s.is_empty())
    }

    /// The named key, when present and known.
    pub fn named_key(&self) -> Option<NamedKey> {
        self.named_value().and_then(NamedKey::from_name)
    }

    /// The HID usage a host falls back to: `hid`, else a named key's usage.
    pub fn fallback_hid(&self) -> Option<u16> {
        self.hid.or_else(|| self.named_key().map(NamedKey::hid))
    }

    /// Problems with this key. `unknown` collects values a newer version may know
    /// (an unknown named key); everything else goes to `errors`.
    pub(crate) fn check(&self, path: &str, errors: &mut Vec<String>, unknown: &mut Vec<String>) {
        let has_char = self.char_value().is_some();
        let has_named = self.named_value().is_some();
        if has_char && has_named {
            errors.push(format!("{path}: key has both char and named"));
        }
        if !has_char && !has_named && self.hid.is_none() {
            errors.push(format!("{path}: key has neither char, named nor hid"));
        }
        // One character: at most two UTF-16 units (the schema's maxLength 2), so
        // an emoji or a letter with one combining mark fits.
        if let Some(c) = self.char_value()
            && c.encode_utf16().count() > 2
        {
            errors.push(format!(
                "{path}: key char {c:?} is longer than one character"
            ));
        }
        if let Some(name) = self.named_value()
            && NamedKey::from_name(name).is_none()
        {
            unknown.push(format!("{path}: unknown named key {name:?}"));
        }
    }
}

/// `keyShortcut` phase.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, uniffi::Enum,
)]
#[serde(rename_all = "camelCase")]
pub enum KeyPhase {
    /// Press and release.
    #[default]
    Tap,
    Down,
    Up,
}

/// `keyShortcut` (0x25, client → host): a key resolved by character on the host's
/// current layout, with literal modifiers already merged with the client's latches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct KeyShortcutMessage {
    pub key: KeySpec,
    /// Literal modifier bits (0...15).
    #[serde(default, deserialize_with = "serde_util::modifier_bits::deserialize")]
    pub modifiers: u8,
    #[serde(default)]
    pub phase: KeyPhase,
    /// On a `down`: auto-repeat until the matching `up` (`repeat` on the wire,
    /// written only when true).
    #[serde(rename = "repeat", default, skip_serializing_if = "is_false")]
    pub auto_repeat: bool,
}

impl KeyShortcutMessage {
    /// A tap of `key` with literal `modifiers`.
    pub fn tap(key: KeySpec, modifiers: u8) -> Self {
        Self {
            key,
            modifiers,
            phase: KeyPhase::Tap,
            auto_repeat: false,
        }
    }
}

/// `text` (0x24, client → host): committed text, typed on the host as characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct TextMessage {
    pub text: String,
    /// Characters to delete before typing (autocorrect, input-method replacements).
    /// A negative value on the wire reads as 0.
    #[serde(default, deserialize_with = "clamped_count")]
    pub replace_before: u32,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn clamped_count<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<u32, D::Error> {
    let value = i64::deserialize(deserializer)?;
    Ok(u32::try_from(value.max(0)).unwrap_or(u32::MAX))
}

/// `KeyPacket` (0x22): a physical key position. Binary: u8 isDown, u16 HID usage
/// (little endian), u8 literal modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct KeyPacket {
    pub is_down: bool,
    pub hid_usage: u16,
    /// Literal modifier bits (0...15).
    pub modifiers: u8,
}

/// Size of an encoded [`KeyPacket`].
pub const KEY_PACKET_SIZE: usize = 4;

/// Encodes a `KeyPacket`. Fails on modifier bits above 15.
#[uniffi::export]
pub fn encode_key_packet(packet: KeyPacket) -> Result<Vec<u8>> {
    if packet.modifiers & !LITERAL_MASK != 0 {
        return Err(StreamInputError::binary(format!(
            "modifiers {} use bits other than shift, control, option and command",
            packet.modifiers
        )));
    }
    let hid = packet.hid_usage.to_le_bytes();
    Ok(vec![
        u8::from(packet.is_down),
        hid[0],
        hid[1],
        packet.modifiers,
    ])
}

/// Decodes a `KeyPacket` strictly: exactly 4 bytes, isDown 0 or 1, modifiers 0...15.
#[uniffi::export]
pub fn decode_key_packet(bytes: Vec<u8>) -> Result<KeyPacket> {
    let [down, lo, hi, modifiers] =
        <[u8; KEY_PACKET_SIZE]>::try_from(bytes.as_slice()).map_err(|_| {
            StreamInputError::binary(format!("key packet is {} bytes, not 4", bytes.len()))
        })?;
    if down > 1 {
        return Err(StreamInputError::binary(format!(
            "key packet isDown is {down}"
        )));
    }
    if modifiers & !LITERAL_MASK != 0 {
        return Err(StreamInputError::binary(format!(
            "key packet modifiers {modifiers} use unknown bits"
        )));
    }
    Ok(KeyPacket {
        is_down: down == 1,
        hid_usage: u16::from_le_bytes([lo, hi]),
        modifiers,
    })
}

// MARK: - Host information

/// `layoutInfo` (0x48, host → client): the host's current keyboard layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct LayoutInfoMessage {
    /// e.g. "com.apple.keylayout.French", or a Windows KLID "0000040C".
    pub id: String,
    /// Localized name; defaults to `id` when absent.
    #[serde(default)]
    pub name: String,
    /// "AZERTY", "QWERTZ", "QWERTY", "Dvorak", "Colemak"; absent for others.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,
    /// The layout's language (BCP 47, e.g. "fr-FR"). Windows sends it; optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// HID usage (decimal string) → [base, Shift] characters.
    #[serde(default, deserialize_with = "tolerant_labels")]
    pub labels: HashMap<String, Vec<String>>,
}

fn tolerant_labels<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<HashMap<String, Vec<String>>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let mut out = HashMap::new();
    if let Some(serde_json::Value::Object(map)) = value {
        for (key, value) in map {
            if let Ok(chars) = serde_json::from_value::<Vec<String>>(value) {
                out.insert(key, chars);
            }
        }
    }
    Ok(out)
}

/// `hostStats` (0x49, host → client, every 2 s while streaming).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct HostStatsMessage {
    /// "hardware" | "software".
    #[serde(default)]
    pub encoder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_name: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_util::f64_opt"
    )]
    pub capture_to_send_ms: Option<f64>,
    #[serde(default, with = "serde_util::f64_plain")]
    pub fps: f64,
    /// "usb" | "wifi" | "other".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Frames dropped since the last report.
    #[serde(default)]
    pub dropped: u64,
}

/// One coded notice in `ServerState.notices`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct ServerNotice {
    pub code: String,
    /// Parameters (always written, `{}` when none); non-string values are kept as
    /// their JSON text, a non-object reads as none.
    #[serde(default, deserialize_with = "tolerant_params")]
    pub params: HashMap<String, String>,
}

fn tolerant_params<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<HashMap<String, String>, D::Error> {
    Ok(
        match Option::<serde_json::Value>::deserialize(deserializer)? {
            Some(serde_json::Value::Object(map)) => map
                .into_iter()
                .map(|(k, v)| match v {
                    serde_json::Value::String(s) => (k, s),
                    other => (k, other.to_string()),
                })
                .collect(),
            _ => HashMap::new(),
        },
    )
}

impl ServerNotice {
    /// A notice with parameters.
    pub fn new(code: &str, params: &[(&str, &str)]) -> Self {
        Self {
            code: code.to_owned(),
            params: params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }
}

/// `ServerState.source`: what is being streamed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct SourceInfo {
    /// "display" | "window" | "app" | "area" | "region"; unknown values are shown as text.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `frontApp` (0x30, host → client).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct FrontAppMessage {
    #[serde(rename = "bundleID", default)]
    pub bundle_id: String,
    #[serde(default)]
    pub name: String,
    /// PNG icon, base64 (Swift `Data` in JSON).
    #[serde(rename = "iconPNG", default, skip_serializing_if = "Option::is_none")]
    pub icon_png: Option<String>,
    /// The profile key for this app (bundle ID on a Mac, lowercase exe on Windows).
    #[serde(rename = "appKey", default, skip_serializing_if = "Option::is_none")]
    pub app_key: Option<String>,
}

/// One entry of `appList`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct AppListEntry {
    #[serde(rename = "bundleID", default)]
    pub bundle_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "iconPNG", default, skip_serializing_if = "Option::is_none")]
    pub icon_png: Option<String>,
    #[serde(rename = "appKey", default, skip_serializing_if = "Option::is_none")]
    pub app_key: Option<String>,
}

/// `appList` (0x34, host → client).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct AppListMessage {
    #[serde(default)]
    pub apps: Vec<AppListEntry>,
}

// MARK: - Gestures

/// `GestureMessage.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum GestureKind {
    /// 1.2: two-finger pan, dx/dy in points.
    Scroll,
    /// 1.2: pinch, `delta` is the scale change.
    Zoom,
    /// 1.2: a named action resolved on the host.
    Action,
    /// `twoFinger`: the whole two-finger state in one message.
    TwoFinger,
    /// `twoFinger`: a mouse click at x/y.
    Click,
}

/// `GestureMessage.phase`: began/changed/ended for scroll and zoom, plus
/// cancelled for twoFinger; tap/down/up for click.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum GesturePhase {
    Began,
    Changed,
    Ended,
    Cancelled,
    Tap,
    Down,
    Up,
}

/// `GestureMessage.action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum GestureAction {
    Undo,
    Redo,
}

/// A mouse button (one of the profile vocabularies).
pub use crate::vocabulary::MouseButton;

/// `gesture` (0x21, client → host).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
pub struct GestureMessage {
    pub kind: GestureKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<GesturePhase>,
    /// twoFinger: magnification change since the last message (0 = none).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_util::f64_opt"
    )]
    pub scale: Option<f64>,
    /// twoFinger: rotation change in degrees since the last message, counter-clockwise positive.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_util::f64_opt"
    )]
    pub rotation: Option<f64>,
    /// Pan in points of the tablet view.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_util::f64_opt"
    )]
    pub dx: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_util::f64_opt"
    )]
    pub dy: Option<f64>,
    /// zoom (1.2): scale delta.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_util::f64_opt"
    )]
    pub delta: Option<f64>,
    /// Normalized position (centroid for twoFinger) within the streamed content.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_util::f64_opt"
    )]
    pub x: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_util::f64_opt"
    )]
    pub y: Option<f64>,
    /// twoFinger: the two touch points, normalized: [x1, y1, x2, y2].
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_util::f64_vec_opt"
    )]
    pub contacts: Option<Vec<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<GestureAction>,
    /// click: which button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub button: Option<MouseButton>,
}

impl GestureMessage {
    /// An empty message of `kind`.
    pub fn new(kind: GestureKind) -> Self {
        Self {
            kind,
            phase: None,
            scale: None,
            rotation: None,
            dx: None,
            dy: None,
            delta: None,
            x: None,
            y: None,
            contacts: None,
            action: None,
            button: None,
        }
    }
}

fn check_finite(name: &str, value: Option<f64>) -> Result<()> {
    match value {
        Some(v) if !v.is_finite() => Err(StreamInputError::message(format!(
            "gesture {name} is not finite"
        ))),
        _ => Ok(()),
    }
}

fn check_unit(name: &str, value: Option<f64>, required: bool) -> Result<()> {
    match value {
        None if required => Err(StreamInputError::message(format!("gesture needs {name}"))),
        Some(v) if !(0.0..=1.0).contains(&v) => Err(StreamInputError::message(format!(
            "gesture {name} {v} is outside 0...1"
        ))),
        _ => Ok(()),
    }
}

/// Strict validation of a gesture: phases valid for the kind, companion fields
/// present, finite numbers, normalized positions in 0...1, four contacts.
#[uniffi::export]
pub fn validate_gesture(message: &GestureMessage) -> Result<()> {
    let m = message;
    for (name, value) in [
        ("scale", m.scale),
        ("rotation", m.rotation),
        ("dx", m.dx),
        ("dy", m.dy),
        ("delta", m.delta),
    ] {
        check_finite(name, value)?;
    }
    let phase_ok = |allowed: &[GesturePhase], required: bool| -> Result<()> {
        match m.phase {
            None if required => Err(StreamInputError::message(format!(
                "{:?} gesture needs a phase",
                m.kind
            ))),
            Some(p) if !allowed.contains(&p) => Err(StreamInputError::message(format!(
                "phase {p:?} is not valid for a {:?} gesture",
                m.kind
            ))),
            _ => Ok(()),
        }
    };
    use GesturePhase::*;
    match m.kind {
        GestureKind::Scroll | GestureKind::Zoom => {
            phase_ok(&[Began, Changed, Ended], false)?;
            check_unit("x", m.x, false)?;
            check_unit("y", m.y, false)?;
        }
        GestureKind::Action => {
            if m.action.is_none() {
                return Err(StreamInputError::message("action gesture needs an action"));
            }
        }
        GestureKind::TwoFinger => {
            phase_ok(&[Began, Changed, Ended, Cancelled], true)?;
            check_unit("x", m.x, false)?;
            check_unit("y", m.y, false)?;
            if let Some(contacts) = &m.contacts {
                if contacts.len() != 4 {
                    return Err(StreamInputError::message(format!(
                        "twoFinger contacts has {} values, not 4",
                        contacts.len()
                    )));
                }
                for c in contacts {
                    check_unit("contact", Some(*c), true)?;
                }
            }
        }
        GestureKind::Click => {
            phase_ok(&[Tap, Down, Up], true)?;
            if m.button.is_none() {
                return Err(StreamInputError::message("click gesture needs a button"));
            }
            check_unit("x", m.x, true)?;
            check_unit("y", m.y, true)?;
        }
    }
    Ok(())
}

/// Strict validation of a `keyShortcut`: one of char/named (or a bare hid), a
/// known named key, one character, literal modifiers 0...15.
#[uniffi::export]
pub fn validate_key_shortcut(message: &KeyShortcutMessage) -> Result<()> {
    if message.modifiers & !LITERAL_MASK != 0 {
        return Err(StreamInputError::message(format!(
            "modifiers {} out of range for literal modifiers (0...15)",
            message.modifiers
        )));
    }
    let mut errors = Vec::new();
    let mut unknown = Vec::new();
    message.key.check("key", &mut errors, &mut unknown);
    errors.extend(unknown);
    match errors.into_iter().next() {
        Some(e) => Err(StreamInputError::message(e)),
        None => Ok(()),
    }
}

// MARK: - Profiles

/// `profilesSnapshot` (0x50): host → client after helloAck and on request. A
/// client sends an empty one (no profiles, or an empty body) to ask for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
#[serde(from = "SnapshotRepr")]
pub struct ProfilesSnapshotMessage {
    pub schema: u32,
    pub revision: u64,
    pub profiles: Vec<Profile>,
}

#[derive(Deserialize)]
struct SnapshotRepr {
    #[serde(default = "schema_one")]
    schema: u32,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    profiles: Vec<serde_json::Value>,
}

fn schema_one() -> u32 {
    1
}

impl From<SnapshotRepr> for ProfilesSnapshotMessage {
    fn from(repr: SnapshotRepr) -> Self {
        Self {
            schema: repr.schema,
            revision: repr.revision,
            // One malformed profile is skipped rather than failing the snapshot.
            profiles: repr
                .profiles
                .into_iter()
                .filter_map(|v| serde_util::strict::from_value(v).ok())
                .collect(),
        }
    }
}

impl ProfilesSnapshotMessage {
    /// An empty snapshot: what a client sends to ask for one.
    pub fn request() -> Self {
        Self {
            schema: 1,
            revision: 0,
            profiles: Vec::new(),
        }
    }
}

/// Whether a `profilesSnapshot` body received by a host is a request for the
/// snapshot: an empty body, `{}`, or a snapshot without profiles.
#[uniffi::export]
pub fn is_profiles_snapshot_request(body: String) -> bool {
    if body.trim().is_empty() {
        return true;
    }
    decode_profiles_snapshot(body).is_ok_and(|s| s.profiles.is_empty())
}

/// `profileUpsert` (0x51, client → host).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
pub struct ProfileUpsertMessage {
    pub profile: Profile,
}

/// `profileDelete` (0x52, client → host): forget the user's profile for an app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDeleteMessage {
    pub app_key: String,
}

/// `profileChanged` (0x53, host → client): exactly one of `profile` or `deleted`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
pub struct ProfileChangedMessage {
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<String>,
}

// MARK: - Codecs

fn decode_json<T: DeserializeOwned>(json: &str) -> Result<T> {
    serde_util::strict::from_str(json).map_err(|e| {
        if e.is_data() {
            StreamInputError::message(e.to_string())
        } else {
            StreamInputError::json(e)
        }
    })
}

pub(crate) fn encode_json<T: Serialize>(value: &T) -> String {
    // The wire types contain only strings, numbers, bools, maps and sequences;
    // serialization cannot fail, but never panic across the FFI boundary.
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}

macro_rules! encoder {
    ($ty:ty, $encode:ident, $doc:literal) => {
        /// Encodes the message as canonical JSON.
        #[uniffi::export]
        pub fn $encode(message: &$ty) -> String {
            encode_json(message)
        }
    };
}

macro_rules! codec {
    ($ty:ty, $encode:ident, $decode:ident, $doc:literal) => {
        encoder!($ty, $encode, $doc);
        /// Decodes the message (unknown members ignored, missing optional members default).
        #[uniffi::export]
        pub fn $decode(json: String) -> Result<$ty> {
            decode_json(&json)
        }
    };
}

/// Ask the host to show its setup checklist. It carries no OS action or arbitrary path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct OpenSetupMessage {}

codec!(
    OpenSetupMessage,
    encode_open_setup,
    decode_open_setup,
    "an `openSetup` body"
);

codec!(TextMessage, encode_text, decode_text, "a `text` body");
codec!(
    HostStatsMessage,
    encode_host_stats,
    decode_host_stats,
    "a `hostStats` body"
);
codec!(
    ServerNotice,
    encode_notice,
    decode_notice,
    "a `ServerState.notices` item"
);
codec!(
    SourceInfo,
    encode_source,
    decode_source,
    "`ServerState.source`"
);
codec!(
    FrontAppMessage,
    encode_front_app,
    decode_front_app,
    "a `frontApp` body"
);
codec!(
    AppListMessage,
    encode_app_list,
    decode_app_list,
    "an `appList` body"
);
codec!(
    ProfileUpsertMessage,
    encode_profile_upsert,
    decode_profile_upsert,
    "a `profileUpsert` body"
);
codec!(
    ProfileDeleteMessage,
    encode_profile_delete,
    decode_profile_delete,
    "a `profileDelete` body"
);
encoder!(LayoutInfoMessage, encode_layout_info, "a `layoutInfo` body");
encoder!(
    KeyShortcutMessage,
    encode_key_shortcut,
    "a `keyShortcut` body"
);
encoder!(GestureMessage, encode_gesture, "a `gesture` body");
encoder!(
    ProfilesSnapshotMessage,
    encode_profiles_snapshot,
    "a `profilesSnapshot` body"
);
encoder!(
    ProfileChangedMessage,
    encode_profile_changed,
    "a `profileChanged` body"
);

/// Encodes `ServerState.notices`.
#[uniffi::export]
pub fn encode_server_notices(notices: Vec<ServerNotice>) -> String {
    encode_json(&notices)
}

/// Decodes `ServerState.notices` tolerantly: entries without a code are skipped,
/// and anything but an array is no notices.
#[uniffi::export]
pub fn decode_server_notices(json: String) -> Result<Vec<ServerNotice>> {
    let value: serde_json::Value = serde_json::from_str(&json).map_err(StreamInputError::json)?;
    Ok(match value {
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter_map(|v| serde_util::strict::from_value(v).ok())
            .collect(),
        _ => Vec::new(),
    })
}

/// Decodes a `layoutInfo` body; a missing name defaults to the id.
#[uniffi::export]
pub fn decode_layout_info(json: String) -> Result<LayoutInfoMessage> {
    let mut message: LayoutInfoMessage = decode_json(&json)?;
    if message.name.is_empty() {
        message.name = message.id.clone();
    }
    Ok(message)
}

/// Decodes a `keyShortcut` body strictly (see [`validate_key_shortcut`]).
#[uniffi::export]
pub fn decode_key_shortcut(json: String) -> Result<KeyShortcutMessage> {
    let message: KeyShortcutMessage = decode_json(&json)?;
    validate_key_shortcut(&message)?;
    Ok(message)
}

/// Decodes a `gesture` body strictly (see [`validate_gesture`]).
#[uniffi::export]
pub fn decode_gesture(json: String) -> Result<GestureMessage> {
    let message: GestureMessage = decode_json(&json)?;
    validate_gesture(&message)?;
    Ok(message)
}

/// Decodes a `profilesSnapshot` body, skipping malformed profiles. An empty body
/// is a request (an empty snapshot).
#[uniffi::export]
pub fn decode_profiles_snapshot(json: String) -> Result<ProfilesSnapshotMessage> {
    if json.trim().is_empty() {
        return Ok(ProfilesSnapshotMessage::request());
    }
    decode_json(&json)
}

/// Decodes a `profileChanged` body: exactly one of `profile` or `deleted`.
#[uniffi::export]
pub fn decode_profile_changed(json: String) -> Result<ProfileChangedMessage> {
    let message: ProfileChangedMessage = decode_json(&json)?;
    if message.profile.is_some() == message.deleted.is_some() {
        return Err(StreamInputError::message(
            "profileChanged needs exactly one of profile or deleted",
        ));
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_request_is_an_object_and_preserves_the_existing_codes() {
        assert_eq!(PadMessageType::OpenSetup.code(), 0x54);
        assert_eq!(PadMessageType::ProfileChanged.code(), 0x53);
        assert_eq!(encode_open_setup(&OpenSetupMessage {}), "{}");
        assert!(decode_open_setup("{}".into()).is_ok());
        for text in ["[]", "null", "1", "\"setup\""] {
            assert!(decode_open_setup(text.into()).is_err());
        }
    }

    #[test]
    fn key_shortcut_canonical_form() {
        let m = decode_key_shortcut(r#"{"key":{"char":"z"},"modifiers":8}"#.into()).unwrap();
        assert_eq!(m.phase, KeyPhase::Tap);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&encode_key_shortcut(&m)).unwrap(),
            serde_json::json!({"key":{"char":"z"},"modifiers":8,"phase":"tap"})
        );
    }

    #[test]
    fn key_shortcut_is_strict() {
        for bad in [
            r#"{"key":{"char":"z"},"modifiers":16}"#,
            r#"{"key":{"char":"z"},"modifiers":300}"#,
            r#"{"key":{"char":"z"},"phase":"hold"}"#,
            r#"{"key":{"named":"f25"}}"#,
            r#"{"key":{"char":"a","named":"tab"}}"#,
            r#"{"key":{}}"#,
            r#"{"key":{"char":"abc"}}"#,
            r#"{"modifiers":8}"#,
            r#"[1]"#,
            r#"{"key":{"char":"z"}"#,
        ] {
            assert!(decode_key_shortcut(bad.into()).is_err(), "{bad}");
        }
        assert!(decode_key_shortcut(r#"{"key":{"hid":4}}"#.into()).is_ok());
        assert!(decode_key_shortcut(r#"{"key":{"char":"😀"}}"#.into()).is_ok());
        assert!(decode_key_shortcut(r#"{"key":{"char":"é"},"future":1}"#.into()).is_ok());
    }

    #[test]
    fn json_errors_are_classified() {
        assert!(matches!(
            decode_text("{".into()),
            Err(StreamInputError::InvalidJson { .. })
        ));
        assert!(matches!(
            decode_text(r#"{"text":1}"#.into()),
            Err(StreamInputError::InvalidMessage { .. })
        ));
        assert_eq!(
            decode_text(r#"{"text":"a","replaceBefore":-3}"#.into())
                .unwrap()
                .replace_before,
            0
        );
    }

    #[test]
    fn gestures_validate_per_kind() {
        let ok = r#"{"kind":"twoFinger","phase":"cancelled","x":0.5,"y":0.5}"#;
        assert!(decode_gesture(ok.into()).is_ok());
        for bad in [
            r#"{"kind":"twoFinger","phase":"tap","x":0.5,"y":0.5}"#,
            r#"{"kind":"twoFinger","x":0.5,"y":0.5}"#,
            r#"{"kind":"twoFinger","phase":"began","x":1.5,"y":0.5}"#,
            r#"{"kind":"twoFinger","phase":"began","scale":1e999}"#,
            r#"{"kind":"twoFinger","phase":"began","x":0.5,"y":0.5,"contacts":[0,0,1]}"#,
            r#"{"kind":"click","phase":"tap","x":0.5,"y":0.5}"#,
            r#"{"kind":"click","phase":"began","button":"left","x":0.5,"y":0.5}"#,
            r#"{"kind":"scroll","phase":"cancelled"}"#,
            r#"{"kind":"action"}"#,
            r#"{"kind":"swipe"}"#,
            r#"{"kind":"click","phase":"tap","button":"back","x":0.5,"y":0.5}"#,
        ] {
            assert!(decode_gesture(bad.into()).is_err(), "{bad}");
        }
    }

    #[test]
    fn key_packet_codec() {
        let p = KeyPacket {
            is_down: true,
            hid_usage: 0x1D,
            modifiers: 9,
        };
        let bytes = encode_key_packet(p).unwrap();
        assert_eq!(bytes, vec![1, 0x1D, 0, 9]);
        assert_eq!(decode_key_packet(bytes).unwrap(), p);
        assert!(decode_key_packet(vec![2, 0, 0, 0]).is_err());
        assert!(decode_key_packet(vec![1, 0, 0, 16]).is_err());
        assert!(decode_key_packet(vec![1, 0, 0]).is_err());
        assert!(decode_key_packet(vec![1, 0, 0, 0, 0]).is_err());
        assert!(encode_key_packet(KeyPacket { modifiers: 16, ..p }).is_err());
    }

    #[test]
    fn notices_and_layouts_are_tolerant() {
        let n = decode_notice(r#"{"code":"key.unavailable","params":{"char":"é","n":3}}"#.into())
            .unwrap();
        assert_eq!(n.params["n"], "3");
        let n = decode_notice(r#"{"code":"x","params":[1]}"#.into()).unwrap();
        assert!(n.params.is_empty());
        let list = decode_server_notices(r#"[{"code":"a"},{"nocode":1},3]"#.into()).unwrap();
        assert_eq!(list.len(), 1);
        let l = decode_layout_info(r#"{"id":"k","labels":{"4":["a","A"],"5":7}}"#.into()).unwrap();
        assert_eq!(l.name, "k");
        assert_eq!(l.labels.len(), 1);
    }

    #[test]
    fn snapshot_requests_and_skips() {
        assert!(is_profiles_snapshot_request(String::new()));
        assert!(is_profiles_snapshot_request("{}".into()));
        let s = decode_profiles_snapshot(
            r#"{"revision":3,"profiles":[{"bad":1},{"schema":1,"appKey":"*","name":"A","updatedAt":"2026-01-01T00:00:00Z","groups":[]}]}"#.into(),
        )
        .unwrap();
        assert_eq!(s.revision, 3);
        assert_eq!(s.profiles.len(), 1);
        assert!(decode_profile_changed(r#"{"revision":1}"#.into()).is_err());
    }

    #[test]
    fn message_codes() {
        assert_eq!(PadMessageType::KeyShortcut.code(), 0x25);
        assert_eq!(
            pad_message_type_from_code(0x53),
            Some(PadMessageType::ProfileChanged)
        );
        assert_eq!(pad_message_type_from_code(0x99), None);
        assert_eq!(capability_names().len(), 10);
        assert_eq!(notice_codes().len(), 11);
    }
}
