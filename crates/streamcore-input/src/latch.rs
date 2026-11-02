//! `LatchState`: the client's modifier bar. Modifiers are latched (tapped on the
//! bar: they stay until tapped again, or until the next key when one-shot) or
//! held (pressed while a Quick Key or gesture holds them). Every outgoing key
//! event merges them in, and on disconnect the state releases every key it
//! still holds down on the host.
//!
//! Values are in the stored convention; outputs are literal for the host.

use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::keys;
use crate::modifiers::{HostOs, STORED_MASK};
use crate::typing::{self, HostCapabilities, OutgoingInput, RoutedInput};
use crate::wire::{KeyPacket, KeyPhase, KeyShortcutMessage, KeySpec};

/// Key-ups to send when the connection ends or the controls are dismissed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseAll {
    /// `keyShortcut` ups (no modifiers) for every key sent `down` and not yet up.
    pub key_shortcuts: Vec<KeyShortcutMessage>,
    /// `KeyPacket` ups (no modifiers) for every physical key still down.
    pub key_packets: Vec<KeyPacket>,
}

#[derive(Debug, Default)]
struct State {
    latched: u8,
    one_shot: bool,
    /// Hold counts per stored bit (shift, control, option, command, platformKey),
    /// so two holds of the same modifier need two releases.
    holds: [u32; 5],
    shortcuts_down: Vec<KeySpec>,
    keys_down: Vec<u16>,
}

impl State {
    fn held(&self) -> u8 {
        (0..5)
            .filter(|&i| self.holds[i] > 0)
            .fold(0, |acc, i| acc | (1 << i))
    }

    fn consume(&mut self) {
        if self.one_shot {
            self.latched = 0;
        }
    }

    fn track(&mut self, routed: &RoutedInput) {
        for message in &routed.messages {
            match message {
                OutgoingInput::KeyShortcut { message } => match message.phase {
                    KeyPhase::Down => self.shortcuts_down.push(message.key.clone()),
                    KeyPhase::Up => {
                        if let Some(i) = self.shortcuts_down.iter().position(|k| *k == message.key)
                        {
                            self.shortcuts_down.remove(i);
                        }
                    }
                    KeyPhase::Tap => {}
                },
                OutgoingInput::Key { packet } => self.track_packet(packet),
                OutgoingInput::Text { .. } => {}
            }
        }
    }

    fn track_packet(&mut self, packet: &KeyPacket) {
        if packet.is_down {
            if !self.keys_down.contains(&packet.hid_usage) {
                self.keys_down.push(packet.hid_usage);
            }
        } else {
            self.keys_down.retain(|h| *h != packet.hid_usage);
        }
    }
}

/// The client's latched and held modifiers. Thread-safe; no callbacks.
#[derive(Debug, Default, uniffi::Object)]
pub struct LatchState {
    state: Mutex<State>,
}

impl LatchState {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[uniffi::export]
impl LatchState {
    /// No latches, nothing held, latches stay until toggled off.
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// One-shot latches clear after the next key or shortcut text.
    pub fn set_one_shot(&self, one_shot: bool) {
        self.lock().one_shot = one_shot;
    }

    pub fn one_shot(&self) -> bool {
        self.lock().one_shot
    }

    /// Toggles latches: if every bit in `modifiers` is latched they are released,
    /// otherwise they are all latched. Returns the latched set.
    pub fn toggle(&self, modifiers: u8) -> u8 {
        let mut s = self.lock();
        let bits = modifiers & STORED_MASK;
        if bits != 0 && s.latched & bits == bits {
            s.latched &= !bits;
        } else {
            s.latched |= bits;
        }
        s.latched
    }

    /// Replaces the latched set.
    pub fn set_latched(&self, modifiers: u8) {
        self.lock().latched = modifiers & STORED_MASK;
    }

    /// Holds modifiers (a Quick Key or gesture is pressed). Returns the held set.
    pub fn hold(&self, modifiers: u8) -> u8 {
        let mut s = self.lock();
        for i in 0..5 {
            if modifiers & (1 << i) != 0 {
                s.holds[i] = s.holds[i].saturating_add(1);
            }
        }
        s.held()
    }

    /// Releases one hold of each modifier. Returns the held set.
    pub fn release(&self, modifiers: u8) -> u8 {
        let mut s = self.lock();
        for i in 0..5 {
            if modifiers & (1 << i) != 0 {
                s.holds[i] = s.holds[i].saturating_sub(1);
            }
        }
        s.held()
    }

    pub fn latched(&self) -> u8 {
        self.lock().latched
    }

    pub fn held(&self) -> u8 {
        self.lock().held()
    }

    /// Latched ∪ held, stored convention.
    pub fn active(&self) -> u8 {
        let s = self.lock();
        s.latched | s.held()
    }

    /// `requested` ∪ latched ∪ held, literal for `host` (`KeyLatch.merge`).
    pub fn merge(&self, requested: u8, host: HostOs) -> u8 {
        let s = self.lock();
        typing::key_latch_merge(requested, s.latched, s.held(), host)
    }

    /// Routes committed text with the current modifiers (see `route_text`).
    pub fn route_text(
        &self,
        text: String,
        replace_before: u32,
        host: HostOs,
        capabilities: HostCapabilities,
    ) -> RoutedInput {
        let mut s = self.lock();
        let routed = typing::route_text(
            text,
            replace_before,
            s.latched,
            s.held(),
            host,
            capabilities,
        );
        if routed
            .messages
            .iter()
            .any(|m| !matches!(m, OutgoingInput::Text { .. }))
        {
            s.consume();
        }
        s.track(&routed);
        routed
    }

    /// Routes a key press (named key, Quick Key shortcut) with the current
    /// modifiers merged in, and remembers keys left down.
    pub fn route_key(
        &self,
        key: KeySpec,
        phase: KeyPhase,
        auto_repeat: bool,
        requested: u8,
        host: HostOs,
        capabilities: HostCapabilities,
    ) -> RoutedInput {
        let mut s = self.lock();
        let routed = typing::route_key(
            key,
            phase,
            auto_repeat,
            requested,
            s.latched,
            s.held(),
            host,
            capabilities,
        );
        if phase != KeyPhase::Up {
            s.consume();
        }
        s.track(&routed);
        routed
    }

    /// A physical key from a hardware keyboard on the tablet ("Keys" mode): the
    /// packet's modifiers are the keyboard's (Mac convention, ⌘ = `command`);
    /// latched and held modifiers are merged in, then
    /// [`hardware_key_for_host`](keys::hardware_key_for_host) maps the key for
    /// the host (on Windows ⌘ acts as Ctrl).
    pub fn key_packet(&self, packet: KeyPacket, host: HostOs) -> KeyPacket {
        let mut s = self.lock();
        let stored = packet.modifiers | s.latched | s.held();
        let key = keys::hardware_key_for_host(packet.hid_usage, stored, host);
        let out = KeyPacket {
            is_down: packet.is_down,
            hid_usage: key.hid_usage,
            modifiers: key.modifiers,
        };
        if packet.is_down && !keys::is_modifier_hid(packet.hid_usage) {
            s.consume();
        }
        s.track_packet(&out);
        out
    }

    /// Releases everything: clears latches and holds and returns key-ups for
    /// every key still down on the host. Call on disconnect.
    pub fn release_all(&self) -> ReleaseAll {
        let mut s = self.lock();
        let out = ReleaseAll {
            key_shortcuts: s
                .shortcuts_down
                .drain(..)
                .map(|key| KeyShortcutMessage {
                    key,
                    modifiers: 0,
                    phase: KeyPhase::Up,
                    auto_repeat: false,
                })
                .collect(),
            key_packets: s
                .keys_down
                .drain(..)
                .map(|hid| KeyPacket {
                    is_down: false,
                    hid_usage: hid,
                    modifiers: 0,
                })
                .collect(),
        };
        let one_shot = s.one_shot;
        *s = State {
            one_shot,
            ..State::default()
        };
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::NamedKey;
    use crate::modifiers::{COMMAND, CONTROL, SHIFT};

    #[test]
    fn toggles_and_holds() {
        let l = LatchState::new();
        assert_eq!(l.toggle(COMMAND), COMMAND);
        assert_eq!(l.toggle(COMMAND | SHIFT), COMMAND | SHIFT);
        assert_eq!(l.toggle(COMMAND | SHIFT), 0);
        assert_eq!(l.hold(SHIFT), SHIFT);
        assert_eq!(l.hold(SHIFT), SHIFT);
        assert_eq!(l.release(SHIFT), SHIFT);
        assert_eq!(l.release(SHIFT), 0);
        assert_eq!(l.release(SHIFT), 0);
        l.set_latched(0xFF);
        assert_eq!(l.latched(), 31);
        assert_eq!(l.merge(0, HostOs::Windows), 15);
    }

    #[test]
    fn one_shot_clears_after_a_key() {
        let l = LatchState::new();
        l.set_one_shot(true);
        l.toggle(COMMAND);
        let r = l.route_text("ab".into(), 0, HostOs::Mac, HostCapabilities::all());
        assert_eq!(r.messages.len(), 2);
        assert_eq!(l.latched(), 0);
        l.toggle(SHIFT);
        let r = l.route_text("ab".into(), 0, HostOs::Mac, HostCapabilities::all());
        assert_eq!(r.messages.len(), 1);
        assert_eq!(
            l.latched(),
            SHIFT,
            "plain text does not use up a Shift latch"
        );
    }

    #[test]
    fn releases_what_is_down() {
        let l = LatchState::new();
        l.toggle(COMMAND);
        let caps = HostCapabilities::all();
        l.route_key(
            KeySpec::named(NamedKey::Left),
            KeyPhase::Down,
            true,
            0,
            HostOs::Windows,
            caps,
        );
        l.route_key(
            KeySpec::char("x"),
            KeyPhase::Down,
            false,
            0,
            HostOs::Windows,
            caps,
        );
        l.route_key(
            KeySpec::char("x"),
            KeyPhase::Up,
            false,
            0,
            HostOs::Windows,
            caps,
        );
        let p = l.key_packet(
            KeyPacket {
                is_down: true,
                hid_usage: keys::HID_LEFT_COMMAND,
                modifiers: COMMAND,
            },
            HostOs::Windows,
        );
        assert_eq!(p.hid_usage, keys::HID_LEFT_CONTROL);
        assert_eq!(p.modifiers, CONTROL);
        let all = l.release_all();
        assert_eq!(all.key_shortcuts.len(), 1);
        assert_eq!(all.key_shortcuts[0].key.named.as_deref(), Some("left"));
        assert_eq!(all.key_shortcuts[0].phase, KeyPhase::Up);
        assert_eq!(all.key_packets.len(), 1);
        assert_eq!(all.key_packets[0].hid_usage, keys::HID_LEFT_CONTROL);
        assert_eq!(l.latched(), 0);
        assert_eq!(l.release_all(), ReleaseAll::default());
    }
}
