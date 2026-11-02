//! Client-side typing routes (Pad's input protocol v2, Keyboard): what a client sends
//! for committed text, a named key or a Quick Key, given the latched and held
//! modifiers and what the host understands.
//!
//! Inputs use the stored convention (`command` = primary, `platformKey` =
//! literal ⌘ / Windows key); outputs are literal for the host.
//!
//! - Text without a shortcut modifier (Mac: ⌘ or Ctrl; Windows: Ctrl, Win or Alt)
//!   goes as one `text` message; Shift or Option alone never makes shortcuts.
//! - With a shortcut modifier, each character goes as a `keyShortcut` tap:
//!   letters lowercase, an uppercase letter adding Shift, newline (`\n`, `\r`,
//!   `\r\n`) as `return` and tab as `tab`.
//! - Hosts without `textInput` get `keyShortcut` taps per character; hosts
//!   without `keyShortcut` either get `KeyPacket`s at US positions (1.2), and
//!   characters with no US position are reported in `unsent`.

use serde::{Deserialize, Serialize};

use crate::keys::{self, NamedKey};
use crate::modifiers::{self, HostOs, SHIFT};
use crate::wire::{
    CAPABILITY_HOST_AWARE_KEYS, CAPABILITY_HOST_STATS, CAPABILITY_KEY_SHORTCUT,
    CAPABILITY_LAYOUT_INFO, CAPABILITY_NOTICE_CODES, CAPABILITY_PEN_V2, CAPABILITY_PROFILES,
    CAPABILITY_TEXT_INPUT, CAPABILITY_TWO_FINGER, KeyPacket, KeyPhase, KeyShortcutMessage, KeySpec,
    TextMessage,
};

/// What the peer advertised, from its capability names (unknown names ignored).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct HostCapabilities {
    pub host_aware_keys: bool,
    pub text_input: bool,
    pub key_shortcut: bool,
    pub layout_info: bool,
    pub two_finger: bool,
    pub pen_v2: bool,
    pub profiles: bool,
    pub notice_codes: bool,
    pub host_stats: bool,
}

impl HostCapabilities {
    /// Everything a 1.3 peer implements.
    pub fn all() -> Self {
        host_capabilities(crate::wire::capability_names())
    }
}

/// Capability names (`ServerState.capabilities`) to flags; unknown names are ignored.
#[uniffi::export]
pub fn host_capabilities(names: Vec<String>) -> HostCapabilities {
    let has = |n: &str| names.iter().any(|x| x == n);
    HostCapabilities {
        host_aware_keys: has(CAPABILITY_HOST_AWARE_KEYS),
        text_input: has(CAPABILITY_TEXT_INPUT),
        key_shortcut: has(CAPABILITY_KEY_SHORTCUT),
        layout_info: has(CAPABILITY_LAYOUT_INFO),
        two_finger: has(CAPABILITY_TWO_FINGER),
        pen_v2: has(CAPABILITY_PEN_V2),
        profiles: has(CAPABILITY_PROFILES),
        notice_codes: has(CAPABILITY_NOTICE_CODES),
        host_stats: has(CAPABILITY_HOST_STATS),
    }
}

/// One message for the client to send, in order.
///
/// Output only: it is serialized (the JSON method table's results) but never
/// decoded. It is internally tagged (`"type"`), and serde decodes such enums
/// from its own buffered copy of the input, which bypasses
/// `serde_util::strict`'s rule that a JSON array never stands in for an object.
/// A test fails if it gains a `Deserialize` implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum OutgoingInput {
    /// `text` (0x24).
    Text { message: TextMessage },
    /// `keyShortcut` (0x25).
    KeyShortcut { message: KeyShortcutMessage },
    /// `KeyPacket` (0x22).
    Key { packet: KeyPacket },
}

/// The messages for one input, plus what could not be sent at all. Output
/// only, like [`OutgoingInput`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct RoutedInput {
    pub messages: Vec<OutgoingInput>,
    /// Characters a 1.2 host cannot receive (no US key position).
    pub unsent: String,
}

/// `KeyLatch.route`: committed text as one `text`, or `keyShortcut` taps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum LatchRoute {
    Text { message: TextMessage },
    Shortcuts { messages: Vec<KeyShortcutMessage> },
}

/// The modifiers to send with a key: requested ∪ latched ∪ held, literal for `host`.
#[uniffi::export]
pub fn key_latch_merge(requested: u8, latched: u8, held: u8, host: HostOs) -> u8 {
    modifiers::resolve_modifiers(requested | latched | held, host)
}

/// One `keyShortcut` tap per character (see the module rules).
pub(crate) fn shortcut_taps(text: &str, literal: u8) -> Vec<KeyShortcutMessage> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let message = match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                KeyShortcutMessage::tap(KeySpec::named(NamedKey::Return), literal)
            }
            '\n' => KeyShortcutMessage::tap(KeySpec::named(NamedKey::Return), literal),
            '\t' => KeyShortcutMessage::tap(KeySpec::named(NamedKey::Tab), literal),
            _ => {
                let lower: String = c.to_lowercase().collect();
                let upper_letter = lower != c.to_string() && c.is_uppercase();
                let mods = if upper_letter {
                    literal | SHIFT
                } else {
                    literal
                };
                KeyShortcutMessage::tap(KeySpec::char(lower), mods)
            }
        };
        out.push(message);
    }
    out
}

/// `KeyLatch.route(text:latched:held:host:)`: committed text while modifiers are
/// latched or held, for a 1.3 host.
#[uniffi::export]
pub fn key_latch_route(text: String, latched: u8, held: u8, host: HostOs) -> LatchRoute {
    let literal = key_latch_merge(0, latched, held, host);
    if modifiers::makes_shortcuts(literal, host) {
        LatchRoute::Shortcuts {
            messages: shortcut_taps(&text, literal),
        }
    } else {
        LatchRoute::Text {
            message: TextMessage {
                text,
                replace_before: 0,
            },
        }
    }
}

fn packets(hid: u16, literal: u8, phase: KeyPhase, out: &mut Vec<OutgoingInput>) {
    let mut push = |is_down| {
        out.push(OutgoingInput::Key {
            packet: KeyPacket {
                is_down,
                hid_usage: hid,
                modifiers: literal,
            },
        });
    };
    match phase {
        KeyPhase::Tap => {
            push(true);
            push(false);
        }
        KeyPhase::Down => push(true),
        KeyPhase::Up => push(false),
    }
}

/// The 1.2 route for a key: its US position as `KeyPacket`s. `false` when the
/// key has no position.
fn key_as_packets(
    key: &KeySpec,
    literal: u8,
    phase: KeyPhase,
    out: &mut Vec<OutgoingInput>,
) -> bool {
    let position = if let Some(c) = key.char_value() {
        let mut chars = c.chars();
        match (chars.next(), chars.next()) {
            (Some(ch), None) => keys::us_key(ch).map(|k| (k.hid, k.shift)),
            _ => None,
        }
        .or_else(|| key.hid.map(|h| (h, false)))
    } else {
        key.fallback_hid().map(|h| (h, false))
    };
    match position {
        Some((hid, shift)) => {
            packets(hid, literal | if shift { SHIFT } else { 0 }, phase, out);
            true
        }
        None => false,
    }
}

/// Routes committed text (with `replace_before` characters to delete first).
#[uniffi::export]
pub fn route_text(
    text: String,
    replace_before: u32,
    latched: u8,
    held: u8,
    host: HostOs,
    capabilities: HostCapabilities,
) -> RoutedInput {
    let literal = key_latch_merge(0, latched, held, host);
    let shortcuts = modifiers::makes_shortcuts(literal, host);
    let mut routed = RoutedInput::default();
    if !shortcuts && capabilities.text_input {
        if !text.is_empty() || replace_before > 0 {
            routed.messages.push(OutgoingInput::Text {
                message: TextMessage {
                    text,
                    replace_before,
                },
            });
        }
        return routed;
    }
    for _ in 0..replace_before {
        let backspace = KeySpec::named(NamedKey::Backspace);
        if capabilities.key_shortcut {
            routed.messages.push(OutgoingInput::KeyShortcut {
                message: KeyShortcutMessage::tap(backspace, 0),
            });
        } else {
            packets(
                NamedKey::Backspace.hid(),
                0,
                KeyPhase::Tap,
                &mut routed.messages,
            );
        }
    }
    for tap in shortcut_taps(&text, literal) {
        if capabilities.key_shortcut {
            routed
                .messages
                .push(OutgoingInput::KeyShortcut { message: tap });
        } else if !key_as_packets(&tap.key, tap.modifiers, KeyPhase::Tap, &mut routed.messages) {
            routed
                .unsent
                .push_str(tap.key.char_value().unwrap_or_default());
        }
    }
    routed
}

/// Routes one key press (a named key, a Quick Key's shortcut, a character key):
/// always a key event, never text. `requested` are the key's own stored
/// modifiers. An uppercase letter is sent lowercase with Shift.
#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub fn route_key(
    key: KeySpec,
    phase: KeyPhase,
    auto_repeat: bool,
    requested: u8,
    latched: u8,
    held: u8,
    host: HostOs,
    capabilities: HostCapabilities,
) -> RoutedInput {
    let mut literal = key_latch_merge(requested, latched, held, host);
    let mut key = key;
    if let Some(c) = key.char_value() {
        let lower = c.to_lowercase();
        if lower != c && c.chars().all(char::is_uppercase) {
            literal |= SHIFT;
            key.character = Some(lower);
        }
    }
    let mut routed = RoutedInput::default();
    if capabilities.key_shortcut {
        routed.messages.push(OutgoingInput::KeyShortcut {
            message: KeyShortcutMessage {
                key,
                modifiers: literal,
                phase,
                auto_repeat: auto_repeat && phase == KeyPhase::Down,
            },
        });
    } else if !key_as_packets(&key, literal, phase, &mut routed.messages) {
        routed
            .unsent
            .push_str(key.char_value().or(key.named_value()).unwrap_or_default());
    }
    routed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modifiers::{COMMAND, CONTROL, OPTION, PLATFORM_KEY};

    fn all() -> HostCapabilities {
        HostCapabilities::all()
    }

    #[test]
    fn plain_text_and_option_stay_text() {
        let r = key_latch_route("Été".into(), 0, 0, HostOs::Mac);
        assert!(matches!(r, LatchRoute::Text { .. }));
        let r = key_latch_route("å".into(), OPTION, 0, HostOs::Mac);
        assert!(matches!(r, LatchRoute::Text { .. }));
        let r = key_latch_route("f".into(), OPTION, 0, HostOs::Windows);
        assert!(matches!(r, LatchRoute::Shortcuts { .. }));
    }

    #[test]
    fn shortcuts_per_character() {
        let LatchRoute::Shortcuts { messages } =
            key_latch_route("zS\r\n\t".into(), COMMAND, 0, HostOs::Mac)
        else {
            panic!()
        };
        let got: Vec<(Option<String>, Option<String>, u8)> = messages
            .into_iter()
            .map(|m| (m.key.character, m.key.named, m.modifiers))
            .collect();
        assert_eq!(
            got,
            vec![
                (Some("z".into()), None, 8),
                (Some("s".into()), None, 9),
                (None, Some("return".into()), 8),
                (None, Some("tab".into()), 8),
            ]
        );
        assert_eq!(
            key_latch_merge(COMMAND, PLATFORM_KEY, OPTION, HostOs::Windows),
            14
        );
        assert_eq!(key_latch_merge(CONTROL, COMMAND, 0, HostOs::Windows), 2);
    }

    #[test]
    fn fallbacks_for_older_hosts() {
        let caps = host_capabilities(vec!["hostAwareKeys".into()]);
        let r = route_text("aB é".into(), 1, 0, 0, HostOs::Mac, caps);
        assert_eq!(r.unsent, "é");
        // backspace tap + a, B (shift), space = 4 taps = 8 packets
        assert_eq!(r.messages.len(), 8);
        assert_eq!(
            r.messages[3],
            OutgoingInput::Key {
                packet: KeyPacket {
                    is_down: false,
                    hid_usage: 0x04,
                    modifiers: 0
                }
            }
        );
        assert_eq!(
            r.messages[4],
            OutgoingInput::Key {
                packet: KeyPacket {
                    is_down: true,
                    hid_usage: 0x05,
                    modifiers: 1
                }
            }
        );
        let caps = host_capabilities(vec!["keyShortcut".into()]);
        let r = route_text("a".into(), 0, 0, 0, HostOs::Mac, caps);
        assert!(matches!(r.messages[0], OutgoingInput::KeyShortcut { .. }));
        let r = route_text(String::new(), 0, 0, 0, HostOs::Mac, all());
        assert!(r.messages.is_empty());
        let r = route_text(String::new(), 2, 0, 0, HostOs::Mac, all());
        assert_eq!(r.messages.len(), 1);
    }

    #[test]
    fn keys_route_as_keys() {
        let r = route_key(
            KeySpec::char("E"),
            KeyPhase::Tap,
            false,
            0,
            0,
            0,
            HostOs::Mac,
            all(),
        );
        let OutgoingInput::KeyShortcut { message } = &r.messages[0] else {
            panic!()
        };
        assert_eq!(message.key.character.as_deref(), Some("e"));
        assert_eq!(message.modifiers, SHIFT);
        let r = route_key(
            KeySpec::named(NamedKey::Left),
            KeyPhase::Down,
            true,
            COMMAND,
            OPTION,
            0,
            HostOs::Windows,
            HostCapabilities::default(),
        );
        assert_eq!(
            r.messages,
            vec![OutgoingInput::Key {
                packet: KeyPacket {
                    is_down: true,
                    hid_usage: 0x50,
                    modifiers: CONTROL | OPTION
                }
            }]
        );
    }
}
