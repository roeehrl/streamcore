//! The injection rules: events in, calls on a [`Backend`] out. Everything a held key, button or
//! contact needs to be let go of again is tracked here, so [`Engine::release_all`] leaves
//! nothing stuck. The engine never waits: paced keys are queued and run from
//! [`Engine::tick`] when their time comes.

use std::collections::VecDeque;

use streamcore_input::keys::{
    HID_LEFT_COMMAND, HID_LEFT_CONTROL, HID_LEFT_OPTION, HID_LEFT_SHIFT, HID_RIGHT_COMMAND,
    HID_RIGHT_CONTROL, HID_RIGHT_OPTION, HID_RIGHT_SHIFT, is_modifier_hid,
};
use streamcore_input::modifiers::{self, COMMAND, CONTROL, LITERAL_MASK, OPTION, SHIFT};
use streamcore_input::{
    GesturePhase, HostKey, HostOs, KeyPacket, KeyPhase, KeyShortcutMessage, KeySpec, PencilPhase,
    PencilSample, PointerKind, StrokeButton, TextMessage,
};

use crate::event::{
    ClickEvent, Feedback, FeedbackSink, InputEvent, PanDragEvent, PenEvent, ScrollEvent,
    TouchEvent, UnavailableKey, ZoomEvent, text_units,
};
use crate::geometry::{self, VirtualScreen, ZoomNotches};
use crate::keys;
use crate::layout::{self, HostKeyState, KeyLayout, KeyStroke, ShortcutPlan, TextStep};
use crate::pacer::{KeyPacer, Pace};
use crate::pan::{PanDrag, PanDragStep};
use crate::pen::{PenFrame, PenInput, PenSession};
use crate::touch::{Contacts, TouchSession};

/// One wheel notch.
const WHEEL_DELTA: i32 = 120;
/// A held key stops repeating after this long, as a safety net.
const REPEAT_LIMIT_MS: u64 = 60_000;

/// One entry of a `SendInput` batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOp {
    /// A key by virtual key and scan code together, so apps reading either see the right key.
    /// `scan_only` (`KEYEVENTF_SCANCODE`): the layout has no key there, so the position is sent.
    Key {
        vk: u16,
        scan: u16,
        extended: bool,
        scan_only: bool,
        up: bool,
    },
    /// One UTF-16 unit as `KEYEVENTF_UNICODE`.
    Unicode {
        unit: u16,
        up: bool,
    },
    /// The pointer to an absolute position, 0 to 65535 across the virtual screen.
    MoveTo {
        x: i32,
        y: i32,
    },
    Button {
        button: StrokeButton,
        down: bool,
    },
    /// The wheel by `delta` (120 a notch); `horizontal` for the sideways wheel.
    Wheel {
        delta: i32,
        horizontal: bool,
    },
}

/// What the engine needs from Windows. The Win32 implementation is `win::Win32Backend`; tests
/// and other hosts can supply their own.
pub trait Backend {
    fn virtual_screen(&self) -> VirtualScreen;
    /// The pointer's position, in desktop pixels.
    fn cursor_position(&self) -> (i32, i32);
    /// One `SendInput` call: the batch goes in as a unit, nothing can land in between.
    fn send(&mut self, batch: &[SendOp]);
    fn pen_available(&self) -> bool;
    /// One pen frame at `position` (relative to the virtual screen's top-left) with `pressure`
    /// (0 to 1024). False when Windows refused it.
    fn inject_pen(&mut self, frame: &PenFrame, position: (i32, i32), pressure: u32) -> bool;
    fn touch_available(&self) -> bool;
    /// Both touch contacts with these pointer flags, in one frame. False when Windows refused it.
    fn inject_touch(&mut self, flags: u32, contacts: Contacts) -> bool;
    /// The keyboard layout of the window that has the focus. Looked up for every shortcut and
    /// text: each window can have its own.
    fn focused_layout(&self) -> Box<dyn KeyLayout + '_>;
    /// `MapVirtualKeyW(MAPVK_VSC_TO_VK_EX)` on the injection thread's layout.
    fn vk_from_scan(&self, scan: u16) -> u16;
    /// `MapVirtualKeyW(MAPVK_VK_TO_VSC)` on the injection thread's layout.
    fn scan_from_vk(&self, vk: u16) -> u16;
    /// Shift, and Ctrl, Alt or the Windows key, as held on the computer right now, and the
    /// focused window's Caps Lock.
    fn keyboard_state(&self) -> HostKeyState;
    /// The system's keyboard delay (0 to 3) and speed (0 to 31) settings.
    fn key_repeat_settings(&self) -> (u32, u32);
}

/// The modifier keys in pressing order, as (modifier bit, left key, right key). Command is the
/// Windows key. Pressed by the engine, a modifier goes down as its left key.
const MODIFIER_KEYS: [(u8, u16, u16); 4] = [
    (CONTROL, HID_LEFT_CONTROL, HID_RIGHT_CONTROL),
    (SHIFT, HID_LEFT_SHIFT, HID_RIGHT_SHIFT),
    (OPTION, HID_LEFT_OPTION, HID_RIGHT_OPTION),
    (COMMAND, HID_LEFT_COMMAND, HID_RIGHT_COMMAND),
];

/// A modifier key's bit in `Keyboard::modifier_keys`: the eight keys' HID usages are consecutive.
fn key_bit(hid: u16) -> u8 {
    1 << (hid - HID_LEFT_CONTROL)
}

/// A shortcut key's identity: a held key is released by the key spec that pressed it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyId(Option<String>, Option<String>, Option<u16>);

impl From<&KeySpec> for KeyId {
    fn from(key: &KeySpec) -> Self {
        Self(key.character.clone(), key.named.clone(), key.hid)
    }
}

/// A key pressed by a shortcut's down and not yet released, with the modifiers its down pressed
/// (released with it) and its auto-repeat.
#[derive(Debug)]
struct HeldShortcut {
    id: KeyId,
    key: KeyStroke,
    pressed: u8,
    repeat: Option<Repeat>,
}

#[derive(Debug, Clone, Copy)]
struct Repeat {
    next: u64,
    period: u64,
    until: u64,
}

#[derive(Debug, Default)]
struct Keyboard {
    /// The modifier keys the client holds down, one bit per key.
    modifier_keys: u8,
    /// Modifiers pressed only because a key event's flags named them (an on-screen keyboard
    /// sends '+' as a Shift-flagged '=' with no Shift key); released with that key.
    synthesized: u8,
    /// Other key positions the client holds down.
    keys: Vec<u16>,
    shortcuts: Vec<HeldShortcut>,
}

impl Keyboard {
    /// The modifiers the client's held modifier keys (left or right) add up to.
    fn held_modifiers(&self) -> u8 {
        MODIFIER_KEYS
            .iter()
            .filter(|(_, left, right)| self.modifier_keys & (key_bit(*left) | key_bit(*right)) != 0)
            .fold(0, |mods, (modifier, ..)| mods | modifier)
    }

    /// Modifiers pressed by held shortcut downs.
    fn shortcut_modifiers(&self) -> u8 {
        self.shortcuts
            .iter()
            .fold(0, |mods, held| mods | held.pressed)
    }

    /// Every modifier that is down because of the client.
    fn all_modifiers(&self) -> u8 {
        self.held_modifiers() | self.synthesized | self.shortcut_modifiers()
    }
}

/// A key event waiting its turn. Key events run strictly in order, and paced ones (text,
/// shortcuts) take time, so they queue here while pointer events go straight through.
#[derive(Debug)]
enum KeyJob {
    Key(KeyPacket),
    Tap(HostKey),
    Shortcut(KeyShortcutMessage),
    Text(TextMessage),
}

pub(crate) struct Engine<B: Backend> {
    backend: B,
    feedback: Box<dyn FeedbackSink>,
    pen: PenSession,
    drawing: bool,
    /// The mouse button a mouse-path stroke holds down, if any.
    stroke_button: Option<StrokeButton>,
    /// Buttons a click's down holds until its up.
    clicked: Vec<StrokeButton>,
    pan: PanDrag,
    zoom: ZoomNotches,
    touch: TouchSession,
    keyboard: Keyboard,
    jobs: VecDeque<KeyJob>,
    /// The paced steps of the job in progress.
    program: VecDeque<(Pace, Vec<SendOp>)>,
    pacer: KeyPacer,
    /// The text units ([`text_units`]) of the text events waiting in `jobs`.
    text_waiting: usize,
    /// The text units of the text being typed; it counts in full until its last key is up.
    text_typing: usize,
}

impl<B: Backend> Engine<B> {
    pub(crate) fn new(backend: B, feedback: Box<dyn FeedbackSink>) -> Self {
        Self {
            backend,
            feedback,
            pen: PenSession::new(),
            drawing: false,
            stroke_button: None,
            clicked: Vec::new(),
            pan: PanDrag::default(),
            zoom: ZoomNotches::default(),
            touch: TouchSession::default(),
            keyboard: Keyboard::default(),
            jobs: VecDeque::new(),
            program: VecDeque::new(),
            pacer: KeyPacer::default(),
            text_waiting: 0,
            text_typing: 0,
        }
    }

    pub(crate) fn backend(&self) -> &B {
        &self.backend
    }

    /// Takes one event at `now` (milliseconds on a monotonic clock).
    pub(crate) fn handle(&mut self, event: InputEvent, now: u64) {
        match event {
            InputEvent::Pen(event) => self.pen_event(event),
            InputEvent::Click(event) => self.click(event),
            InputEvent::Scroll(event) => self.scroll(event),
            InputEvent::Zoom(event) => self.zoom_event(event),
            InputEvent::PanDrag(event) => self.pan_drag(event),
            InputEvent::Touch(event) => self.touch_event(event, now),
            InputEvent::Key(packet) => self.jobs.push_back(KeyJob::Key(packet)),
            InputEvent::KeyTap(key) => self.jobs.push_back(KeyJob::Tap(key)),
            InputEvent::KeyShortcut(message) => self.jobs.push_back(KeyJob::Shortcut(message)),
            InputEvent::Text(message) => {
                self.text_waiting += text_units(&message);
                self.jobs.push_back(KeyJob::Text(message));
            }
        }
        self.tick(now);
    }

    /// Runs whatever is due: paced keys, key repeats, the touch refresh and its watchdog.
    pub(crate) fn tick(&mut self, now: u64) {
        self.run_keys(now);
        self.repeat_keys(now);
        let backend = &mut self.backend;
        self.touch.tick(now, &mut |flags, contacts| {
            backend.inject_touch(flags, contacts)
        });
    }

    /// The key work not finished yet, as (operations, text units): the key events waiting their
    /// turn plus the one in progress. The injector counts it against its queue limits.
    pub(crate) fn backlog(&self) -> (usize, usize) {
        (
            self.jobs.len() + usize::from(!self.program.is_empty()),
            self.text_waiting + self.text_typing,
        )
    }

    /// When [`tick`](Self::tick) next has something to do.
    pub(crate) fn next_deadline(&self, now: u64) -> Option<u64> {
        let paced = self
            .program
            .front()
            .map(|(pace, _)| self.pacer.ready_at(*pace, now));
        let repeats = self
            .keyboard
            .shortcuts
            .iter()
            .filter_map(|held| held.repeat.map(|repeat| repeat.next));
        paced
            .into_iter()
            .chain(repeats)
            .chain(self.touch.next_deadline())
            .min()
    }

    /// Lets go of everything: keys and modifiers the client holds, held shortcuts, a key in
    /// mid-tap, touch contacts, the pen (lifted and out of range), mouse buttons and a pan drag.
    /// Keys still waiting to be typed are dropped. Runs when a lease is revoked and when the
    /// injector goes away, so nothing stays stuck on the computer.
    pub(crate) fn release_all(&mut self) {
        let mut ops = Vec::new();
        // A tapped key that is down right now comes up; the rest of the text is not typed.
        if let Some((Pace::TapRelease, release)) = self.program.pop_front() {
            ops.extend(release);
        }
        self.program.clear();
        self.jobs.clear();
        self.text_waiting = 0;
        self.text_typing = 0;
        for held in std::mem::take(&mut self.keyboard.shortcuts) {
            ops.push(stroke_op(held.key, true));
            self.add_modifiers(&mut ops, held.pressed, false);
        }
        for hid in std::mem::take(&mut self.keyboard.keys) {
            self.add_key(&mut ops, hid, false);
        }
        for (_, left, right) in MODIFIER_KEYS {
            for hid in [left, right] {
                if self.keyboard.modifier_keys & key_bit(hid) != 0 {
                    self.add_key(&mut ops, hid, false);
                }
            }
        }
        let synthesized = self.keyboard.synthesized;
        self.add_modifiers(&mut ops, synthesized, false);
        self.keyboard.modifier_keys = 0;
        self.keyboard.synthesized = 0;
        self.send(&ops);

        let backend = &mut self.backend;
        self.touch.end(true, &mut |flags, contacts| {
            backend.inject_touch(flags, contacts)
        });
        for frame in self.pen.release() {
            self.inject_pen(frame);
        }
        self.set_drawing(false);
        let mut buttons = Vec::new();
        for button in self
            .stroke_button
            .take()
            .into_iter()
            .chain(self.clicked.drain(..))
        {
            buttons.push(SendOp::Button {
                button,
                down: false,
            });
        }
        self.send(&buttons);
        let steps = self.pan.release();
        self.post_pan(&steps);
        self.zoom.reset();
    }

    fn send(&mut self, ops: &[SendOp]) {
        if !ops.is_empty() {
            self.backend.send(ops);
        }
    }

    fn set_drawing(&mut self, drawing: bool) {
        if self.drawing != drawing {
            self.drawing = drawing;
            self.feedback.feedback(Feedback::Drawing(drawing));
        }
    }

    // Pen and pointer.

    fn pen_event(&mut self, event: PenEvent) {
        let sample = event.sample;
        if sample.phase == PencilPhase::Cancel {
            self.cancel_stroke();
            return;
        }
        // A hover while a stroke is down is another pointer's (a mouse over the client's canvas):
        // it must not move the mouse under the stroke or touch the pen.
        let stroke_down = self.pen.stroke_down() || self.stroke_button.is_some();
        if stroke_down && matches!(sample.phase, PencilPhase::Hover | PencilPhase::HoverExit) {
            return;
        }
        match sample.phase {
            PencilPhase::Down => self.set_drawing(true),
            PencilPhase::Up | PencilPhase::HoverExit => self.set_drawing(false),
            _ => {}
        }
        let (x, y) = event.target.point(sample.x, sample.y);
        // A right button on the pen is its barrel button when the client means it so. Finger
        // taps and other right or middle buttons go through the mouse; so does everything when
        // there is no pen device, so the pointer and clicks still work.
        let barrel = event.right_button_is_barrel
            && sample.button == StrokeButton::Right
            && sample.kind == PointerKind::Pen;
        if sample.kind == PointerKind::Finger
            || (sample.button != StrokeButton::Left && !barrel)
            || !self.backend.pen_available()
        {
            self.mouse_stroke(&sample, x, y);
            return;
        }
        let (tilt_x, tilt_y) = geometry::tilt_degrees(sample.altitude, sample.azimuth);
        let frames = self.pen.sample(PenInput {
            phase: sample.phase,
            kind: sample.kind,
            barrel,
            x,
            y,
            force: sample.force,
            tilt_x,
            tilt_y,
        });
        for frame in frames {
            self.inject_pen(frame);
        }
    }

    /// The contact was a palm or the system cancelled it. Lift and leave range; if it had
    /// touched down, tell the host so the app's undo can take the mark back.
    fn cancel_stroke(&mut self) {
        self.set_drawing(false);
        let (frames, mut touched) = self.pen.cancel();
        for frame in frames {
            self.inject_pen(frame);
        }
        if let Some(button) = self.stroke_button.take() {
            touched = true;
            self.send(&[SendOp::Button {
                button,
                down: false,
            }]);
        }
        if touched {
            self.feedback.feedback(Feedback::StrokeCancelled);
        }
    }

    fn inject_pen(&mut self, frame: PenFrame) {
        let screen = self.backend.virtual_screen();
        let position = screen.pointer(frame.x, frame.y);
        let pressure = geometry::pen_pressure(frame.force, frame.in_contact);
        if !self.backend.inject_pen(&frame, position, pressure) {
            // The pen path failed mid-stream: at least keep the pointer following for this frame.
            let (x, y) = screen.absolute(frame.x, frame.y);
            self.send(&[SendOp::MoveTo { x, y }]);
        }
    }

    fn move_to(&self, x: i32, y: i32) -> SendOp {
        let (x, y) = self.backend.virtual_screen().absolute(x, y);
        SendOp::MoveTo { x, y }
    }

    /// A finger, a mouse button, or the pen without a pen device: the mouse.
    fn mouse_stroke(&mut self, sample: &PencilSample, x: i32, y: i32) {
        let move_to = self.move_to(x, y);
        match sample.phase {
            PencilPhase::Down => {
                let button = sample.button;
                self.send(&[move_to, SendOp::Button { button, down: true }]);
                self.stroke_button = Some(button);
            }
            PencilPhase::Moved | PencilPhase::Hover => self.send(&[move_to]),
            PencilPhase::Up => {
                // The button that went down comes up, whatever this sample says.
                let button = self.stroke_button.take().unwrap_or(sample.button);
                self.send(&[
                    move_to,
                    SendOp::Button {
                        button,
                        down: false,
                    },
                ]);
            }
            PencilPhase::HoverExit | PencilPhase::Cancel => {}
        }
    }

    fn click(&mut self, event: ClickEvent) {
        let (x, y) = event.target.point(event.x, event.y);
        let button = event.button;
        let mut ops = vec![self.move_to(x, y)];
        if event.phase != KeyPhase::Up {
            ops.push(SendOp::Button { button, down: true });
        }
        if event.phase != KeyPhase::Down {
            ops.push(SendOp::Button {
                button,
                down: false,
            });
        }
        self.clicked.retain(|held| *held != button);
        if event.phase == KeyPhase::Down {
            self.clicked.push(button);
        }
        self.send(&ops);
    }

    /// Fingers moving right drag the content right, which is a scroll to the left on Windows (a
    /// negative horizontal wheel), the same natural direction as vertical.
    fn scroll(&mut self, event: ScrollEvent) {
        let (dx, dy) = event.target.delta(event.dx, event.dy);
        let (dx, dy) = (geometry::round(dx), geometry::round(dy));
        let mut ops = Vec::new();
        if dy != 0 {
            ops.push(SendOp::Wheel {
                delta: dy,
                horizontal: false,
            });
        }
        if dx != 0 {
            ops.push(SendOp::Wheel {
                delta: dx.saturating_neg(),
                horizontal: true,
            });
        }
        self.send(&ops);
    }

    fn zoom_event(&mut self, event: ZoomEvent) {
        if event.phase != GesturePhase::Changed {
            self.zoom.reset();
            return;
        }
        let notches = self.zoom.add(event.delta);
        if notches == 0 {
            return;
        }
        let mut ops = Vec::new();
        // The wheel goes to the window under the pointer and apps zoom about it, so put the
        // pointer under the fingers.
        if let Some((x, y)) = event.anchor {
            let (x, y) = event.target.point(x, y);
            ops.push(self.move_to(x, y));
        }
        let press = event.modifiers & LITERAL_MASK & !self.keyboard.all_modifiers();
        self.add_modifiers(&mut ops, press, true);
        for _ in 0..notches.abs() {
            ops.push(SendOp::Wheel {
                delta: WHEEL_DELTA * notches.signum(),
                horizontal: false,
            });
        }
        // Alt pressed and released with no key in between opens the menu bar.
        if press & OPTION != 0 {
            for up in [false, true] {
                ops.push(SendOp::Key {
                    vk: keys::VK_MASK,
                    scan: 0,
                    extended: false,
                    scan_only: false,
                    up,
                });
            }
        }
        self.add_modifiers(&mut ops, press, false);
        self.send(&ops);
    }

    fn pan_drag(&mut self, event: PanDragEvent) {
        let start = match event.anchor {
            Some((x, y)) => event.target.point(x, y),
            None => self.backend.cursor_position(),
        };
        let (dx, dy) = event.target.delta(event.dx, event.dy);
        let steps = self.pan.plan(event.phase, event.via, dx, dy, start);
        self.post_pan(&steps);
    }

    fn post_pan(&mut self, steps: &[PanDragStep]) {
        let button = |middle| {
            if middle {
                StrokeButton::Middle
            } else {
                StrokeButton::Left
            }
        };
        for step in steps {
            let mut ops = Vec::new();
            match *step {
                PanDragStep::MoveTo(x, y) => ops.push(self.move_to(x, y)),
                PanDragStep::Press { middle } => ops.push(SendOp::Button {
                    button: button(middle),
                    down: true,
                }),
                PanDragStep::Release { middle } => ops.push(SendOp::Button {
                    button: button(middle),
                    down: false,
                }),
                PanDragStep::SpaceDown => self.add_key(&mut ops, keys::HID_SPACE, true),
                PanDragStep::SpaceUp => self.add_key(&mut ops, keys::HID_SPACE, false),
            }
            self.send(&ops);
        }
    }

    fn touch_event(&mut self, event: TouchEvent, now: u64) {
        let backend = &mut self.backend;
        match event.phase {
            GesturePhase::Ended => {
                self.touch.end(false, &mut |flags, contacts| {
                    backend.inject_touch(flags, contacts)
                });
            }
            GesturePhase::Cancelled => {
                self.touch.end(true, &mut |flags, contacts| {
                    backend.inject_touch(flags, contacts)
                });
            }
            _ => {
                let screen = backend.virtual_screen();
                let contacts = event.contacts.map(|(x, y)| {
                    let (x, y) = event.target.point(x, y);
                    screen.pointer(x, y)
                });
                let delivered = backend.touch_available()
                    && self.touch.touch(contacts, now, &mut |flags, contacts| {
                        backend.inject_touch(flags, contacts)
                    });
                if !delivered {
                    self.feedback.feedback(Feedback::TouchFailed);
                }
            }
        }
    }

    // Keys.

    /// Runs the key queue up to `now`: the steps of the job in progress as their time comes,
    /// then the next job.
    fn run_keys(&mut self, now: u64) {
        loop {
            if let Some((pace, _)) = self.program.front() {
                if self.pacer.ready_at(*pace, now) > now {
                    return;
                }
                if let Some((pace, ops)) = self.program.pop_front() {
                    self.send(&ops);
                    self.pacer.ran(pace, now);
                }
            } else {
                // The job in progress has finished.
                self.text_typing = 0;
                match self.jobs.pop_front() {
                    Some(KeyJob::Key(packet)) => self.key(packet),
                    Some(KeyJob::Tap(key)) => self.key_tap(key),
                    Some(KeyJob::Shortcut(message)) => self.key_shortcut(message, now),
                    Some(KeyJob::Text(message)) => {
                        let units = text_units(&message);
                        self.text_waiting = self.text_waiting.saturating_sub(units);
                        self.text_typing = units;
                        self.type_text(&message.text, message.replace_before);
                    }
                    None => return,
                }
            }
        }
    }

    /// A key position from the client. Modifier keys go down and up as themselves; other keys
    /// bring along any flagged modifier that is not already held. Releasing every flagged
    /// modifier on each key-up let go of a Shift or Ctrl the user was still holding.
    fn key(&mut self, packet: KeyPacket) {
        let hid = packet.hid_usage;
        let mut ops = Vec::new();
        if is_modifier_hid(hid) {
            if packet.is_down {
                self.keyboard.modifier_keys |= key_bit(hid);
            } else {
                self.keyboard.modifier_keys &= !key_bit(hid);
            }
            // Held for real now: its own key-up releases it.
            self.keyboard.synthesized &= !self.keyboard.held_modifiers();
            self.add_key(&mut ops, hid, packet.is_down);
        } else if packet.is_down {
            let press = packet.modifiers
                & LITERAL_MASK
                & !self.keyboard.held_modifiers()
                & !self.keyboard.synthesized;
            self.add_modifiers(&mut ops, press, true);
            self.keyboard.synthesized |= press;
            self.add_key(&mut ops, hid, true);
            if !self.keyboard.keys.contains(&hid) {
                self.keyboard.keys.push(hid);
            }
        } else {
            self.add_key(&mut ops, hid, false);
            let synthesized = std::mem::take(&mut self.keyboard.synthesized);
            self.add_modifiers(&mut ops, synthesized, false);
            self.keyboard.keys.retain(|held| *held != hid);
        }
        self.send(&ops);
    }

    /// Taps a key position the way a keyboard does: modifiers down, key down and up, modifiers
    /// up in reverse, in one batch. Held modifiers stay held.
    fn key_tap(&mut self, key: HostKey) {
        let press = key.modifiers & LITERAL_MASK & !self.keyboard.all_modifiers();
        let mut ops = Vec::new();
        self.add_modifiers(&mut ops, press, true);
        self.add_key(&mut ops, key.hid_usage, true);
        self.add_key(&mut ops, key.hid_usage, false);
        self.add_modifiers(&mut ops, press, false);
        self.send(&ops);
    }

    /// A shortcut: the key is resolved on the layout of the window that gets it, then tapped,
    /// pressed or released with the literal modifiers plus whatever Shift the layout needs.
    fn key_shortcut(&mut self, message: KeyShortcutMessage, now: u64) {
        let id = KeyId::from(&message.key);
        if message.phase == KeyPhase::Up {
            // An up with nothing held follows a down that pressed nothing (unavailable, or typed
            // as text).
            if let Some(index) = self
                .keyboard
                .shortcuts
                .iter()
                .position(|held| held.id == id)
            {
                let held = self.keyboard.shortcuts.remove(index);
                let mut ops = vec![stroke_op(held.key, true)];
                self.add_modifiers(&mut ops, held.pressed, false);
                self.program.push_back((Pace::Release, ops));
            }
            return;
        }
        let (plan, layout_name) = {
            let layout = self.backend.focused_layout();
            let plan =
                layout::plan_shortcut(&message.key, message.modifiers, &*layout, message.phase);
            // Only a refused key needs the name, which is not free to look up.
            let name = matches!(plan, ShortcutPlan::Unavailable { .. }).then(|| layout.name());
            (plan, name.unwrap_or_default())
        };
        let (key, modifiers) = match plan {
            ShortcutPlan::Key { key, modifiers } => (key, modifiers),
            ShortcutPlan::Text(text) => return self.type_text(&text, 0),
            ShortcutPlan::Unavailable { reason, notice } => {
                let unavailable = UnavailableKey {
                    key: message.key,
                    reason,
                    layout: layout_name,
                    notice,
                };
                return self
                    .feedback
                    .feedback(Feedback::KeyUnavailable(Box::new(unavailable)));
            }
        };
        let press = modifiers & !self.keyboard.all_modifiers();
        let mut down = Vec::new();
        self.add_modifiers(&mut down, press, true);
        down.push(stroke_op(key, false));
        if message.phase == KeyPhase::Down {
            // A second down of a key whose up was lost takes over the modifiers the first one
            // pressed, so its up releases them.
            let mut pressed = press;
            self.keyboard.shortcuts.retain(|held| {
                if held.id == id {
                    pressed |= held.pressed;
                }
                held.id != id
            });
            let repeat = message.auto_repeat.then(|| {
                let (delay, speed) = self.backend.key_repeat_settings();
                let (delay, period) = geometry::key_repeat_timing(delay, speed);
                Repeat {
                    next: now + delay,
                    period,
                    until: now + REPEAT_LIMIT_MS,
                }
            });
            self.keyboard.shortcuts.push(HeldShortcut {
                id,
                key,
                pressed,
                repeat,
            });
            self.program.push_back((Pace::Press, down));
        } else {
            // At the pace typed text goes, so it stays in order with the text around it.
            let mut up = vec![stroke_op(key, true)];
            self.add_modifiers(&mut up, press, false);
            self.program.push_back((Pace::Press, down));
            self.program.push_back((Pace::TapRelease, up));
        }
    }

    /// Auto-repeats held keys at the system's keyboard delay and rate, as a real key does. A
    /// repeat stops with its up, on release, or after a minute.
    fn repeat_keys(&mut self, now: u64) {
        let mut ops = Vec::new();
        for held in &mut self.keyboard.shortcuts {
            let Some(repeat) = &mut held.repeat else {
                continue;
            };
            if now >= repeat.until {
                held.repeat = None;
            } else if now >= repeat.next {
                ops.push(stroke_op(held.key, false));
                repeat.next = now + repeat.period;
            }
        }
        self.send(&ops);
    }

    /// Queues text one character at a time: each goes down, stays down, comes up, and the next
    /// waits (see [`crate::pacer`] for why a burst loses characters).
    fn type_text(&mut self, text: &str, replace_before: u32) {
        let held = self.keyboard.all_modifiers();
        let computer = self.backend.keyboard_state();
        let state = HostKeyState {
            shift_held: held & SHIFT != 0 || computer.shift_held,
            shortcut_mods_held: modifiers::makes_shortcuts(held & LITERAL_MASK, HostOs::Windows)
                || computer.shortcut_mods_held,
            caps_lock: computer.caps_lock,
        };
        let steps = {
            let layout = self.backend.focused_layout();
            layout::plan_text(text, replace_before, &*layout, state)
        };
        for step in steps {
            match step {
                TextStep::Key { key, shift } => {
                    let (mut down, mut up) = (Vec::new(), Vec::new());
                    if shift {
                        self.add_key(&mut down, HID_LEFT_SHIFT, true);
                    }
                    down.push(stroke_op(key, false));
                    up.push(stroke_op(key, true));
                    if shift {
                        self.add_key(&mut up, HID_LEFT_SHIFT, false);
                    }
                    self.program.push_back((Pace::Press, down));
                    self.program.push_back((Pace::TapRelease, up));
                }
                // A surrogate pair goes as its two units, each its own key event.
                TextStep::Packet(units) => {
                    for unit in units {
                        self.program
                            .push_back((Pace::Press, vec![SendOp::Unicode { unit, up: false }]));
                        self.program.push_back((
                            Pace::TapRelease,
                            vec![SendOp::Unicode { unit, up: true }],
                        ));
                    }
                }
            }
        }
    }

    fn add_modifiers(&self, ops: &mut Vec<SendOp>, mods: u8, down: bool) {
        let mut press = |(modifier, left, _): (u8, u16, u16)| {
            if mods & modifier != 0 {
                self.add_key(ops, left, down);
            }
        };
        if down {
            MODIFIER_KEYS.into_iter().for_each(&mut press);
        } else {
            MODIFIER_KEYS.into_iter().rev().for_each(&mut press);
        }
    }

    /// A key position as `SendInput` needs it: the virtual key, its scan code (apps built on
    /// some toolkits read it, and misread keys or miss the keypad without it) and the extended
    /// flag a real keyboard sets on its E0-prefixed keys. Nothing for a usage with no key.
    fn add_key(&self, ops: &mut Vec<SendOp>, hid: u16, down: bool) {
        let positional = keys::positional_scan(hid);
        let (vk, scan, extended) = if positional != 0 {
            (self.backend.vk_from_scan(positional), positional, false)
        } else {
            let vk = keys::hid_to_vk(hid);
            if vk == 0 {
                return;
            }
            let scan = match hid {
                0x46 => 0x37, // Print Screen (E0 37)
                0x58 => 0x1C, // keypad Enter (E0 1C)
                HID_RIGHT_SHIFT => 0x36,
                _ => self.backend.scan_from_vk(vk),
            };
            // Keypad Enter and the right Ctrl and Alt share their virtual key with a key that is
            // not extended.
            let extended = matches!(hid, 0x58 | HID_RIGHT_CONTROL | HID_RIGHT_OPTION)
                || keys::is_extended_vk(vk);
            (vk, scan, extended)
        };
        ops.push(SendOp::Key {
            vk,
            scan,
            extended,
            scan_only: vk == 0,
            up: !down,
        });
    }
}

fn stroke_op(key: KeyStroke, up: bool) -> SendOp {
    SendOp::Key {
        vk: key.vk,
        scan: key.scan,
        extended: key.extended,
        scan_only: false,
        up,
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::layout::testing::FakeLayout;
    use std::sync::{Arc, Mutex};

    /// What a [`FakeBackend`] was asked to do.
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum Call {
        Send(Vec<SendOp>),
        Pen {
            position: (i32, i32),
            pressure: u32,
            pointer_flags: u32,
            pen_flags: u32,
        },
        Touch(u32, Contacts),
    }

    pub(crate) type Calls = Arc<Mutex<Vec<Call>>>;

    /// A computer with a US keyboard and one monitor to the left of the primary.
    pub(crate) struct FakeBackend {
        pub calls: Calls,
        pub pen: bool,
        pub touch: bool,
        pub pen_refused: bool,
        pub touch_refused: bool,
        pub layout: fn() -> FakeLayout,
        pub state: HostKeyState,
    }

    impl FakeBackend {
        pub(crate) fn new(calls: Calls) -> Self {
            Self {
                calls,
                pen: true,
                touch: true,
                pen_refused: false,
                touch_refused: false,
                layout: FakeLayout::us,
                state: HostKeyState::default(),
            }
        }

        fn record(&self, call: Call) {
            self.calls.lock().expect("calls").push(call);
        }
    }

    pub(crate) const SCREEN: VirtualScreen = VirtualScreen {
        x: -1000,
        y: 0,
        width: 3000,
        height: 1000,
    };

    impl Backend for FakeBackend {
        fn virtual_screen(&self) -> VirtualScreen {
            SCREEN
        }

        fn cursor_position(&self) -> (i32, i32) {
            (500, 600)
        }

        fn send(&mut self, batch: &[SendOp]) {
            self.record(Call::Send(batch.to_vec()));
        }

        fn pen_available(&self) -> bool {
            self.pen
        }

        fn inject_pen(&mut self, frame: &PenFrame, position: (i32, i32), pressure: u32) -> bool {
            self.record(Call::Pen {
                position,
                pressure,
                pointer_flags: frame.pointer_flags,
                pen_flags: frame.pen_flags,
            });
            !self.pen_refused
        }

        fn touch_available(&self) -> bool {
            self.touch
        }

        fn inject_touch(&mut self, flags: u32, contacts: Contacts) -> bool {
            self.record(Call::Touch(flags, contacts));
            !self.touch_refused
        }

        fn focused_layout(&self) -> Box<dyn KeyLayout + '_> {
            Box::new((self.layout)())
        }

        fn vk_from_scan(&self, scan: u16) -> u16 {
            // The US layout has no key at the ISO and JIS positions.
            (0x04..0xE8)
                .find(|hid| keys::positional_scan(*hid) == 0 && keys::hid_to_scan(*hid) == scan)
                .map_or(0, keys::hid_to_vk)
        }

        fn scan_from_vk(&self, vk: u16) -> u16 {
            (0x04..0xE8)
                .find(|hid| keys::hid_to_vk(*hid) == vk)
                .map_or(0, |hid| keys::hid_to_scan(hid) & 0xFF)
        }

        fn keyboard_state(&self) -> HostKeyState {
            self.state
        }

        fn key_repeat_settings(&self) -> (u32, u32) {
            (0, 31)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Call, Calls, FakeBackend, SCREEN};
    use super::*;
    use crate::geometry::TargetRect;
    use crate::layout::KeyUnavailable;
    use crate::layout::testing::FakeLayout;
    use crate::pen::{
        PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_INVERTED, POINTER_FLAG_DOWN,
        POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP,
    };
    use std::sync::{Arc, Mutex};
    use streamcore_input::{NamedKey, PanDelivery};

    const TARGET: TargetRect = TargetRect {
        x: 100,
        y: 200,
        width: 1000,
        height: 500,
    };

    struct Rig {
        engine: Engine<FakeBackend>,
        calls: Calls,
        feedback: Arc<Mutex<Vec<Feedback>>>,
    }

    impl Rig {
        fn new() -> Self {
            Self::with(|_| {})
        }

        fn with(configure: impl FnOnce(&mut FakeBackend)) -> Self {
            let calls = Calls::default();
            let feedback = Arc::new(Mutex::new(Vec::new()));
            let mut backend = FakeBackend::new(calls.clone());
            configure(&mut backend);
            let sink = feedback.clone();
            let engine = Engine::new(
                backend,
                Box::new(move |f| sink.lock().expect("feedback").push(f)),
            );
            Self {
                engine,
                calls,
                feedback,
            }
        }

        /// Everything the backend was asked to do since the last look.
        fn calls(&self) -> Vec<Call> {
            std::mem::take(&mut *self.calls.lock().expect("calls"))
        }

        /// The `SendInput` batches since the last look; panics on a pen or touch call.
        fn sent(&self) -> Vec<Vec<SendOp>> {
            self.calls()
                .into_iter()
                .map(|call| match call {
                    Call::Send(ops) => ops,
                    other => panic!("not a SendInput batch: {other:?}"),
                })
                .collect()
        }

        fn feedback(&self) -> Vec<Feedback> {
            std::mem::take(&mut *self.feedback.lock().expect("feedback"))
        }

        fn pen(&mut self, phase: PencilPhase, kind: PointerKind, button: StrokeButton) {
            self.pen_with(phase, kind, button, false);
        }

        fn pen_with(
            &mut self,
            phase: PencilPhase,
            kind: PointerKind,
            button: StrokeButton,
            barrel: bool,
        ) {
            let sample = PencilSample {
                phase,
                kind,
                button,
                x: 0.5,
                y: 0.5,
                force: 0.5,
                altitude: 0.0,
                azimuth: 0.0,
                roll: 0.0,
                hover_z: 0.0,
                timestamp: 0.0,
            };
            self.engine.handle(
                InputEvent::Pen(PenEvent {
                    sample,
                    target: TARGET,
                    right_button_is_barrel: barrel,
                }),
                0,
            );
        }

        fn key(&mut self, hid_usage: u16, is_down: bool, modifiers: u8, now: u64) {
            let packet = KeyPacket {
                is_down,
                hid_usage,
                modifiers,
            };
            self.engine.handle(InputEvent::Key(packet), now);
        }

        fn shortcut(&mut self, key: KeySpec, modifiers: u8, phase: KeyPhase, now: u64) {
            let message = KeyShortcutMessage {
                key,
                modifiers,
                phase,
                auto_repeat: false,
            };
            self.engine.handle(InputEvent::KeyShortcut(message), now);
        }

        fn text(&mut self, text: &str, now: u64) {
            let message = TextMessage {
                text: text.into(),
                replace_before: 0,
            };
            self.engine.handle(InputEvent::Text(message), now);
        }
    }

    fn key(vk: u16, scan: u16, up: bool) -> SendOp {
        SendOp::Key {
            vk,
            scan,
            extended: false,
            scan_only: false,
            up,
        }
    }

    fn ctrl(up: bool) -> SendOp {
        key(keys::VK_CONTROL, 0x1D, up)
    }

    fn shift(up: bool) -> SendOp {
        key(keys::VK_SHIFT, 0x2A, up)
    }

    fn alt(up: bool) -> SendOp {
        key(keys::VK_MENU, 0x38, up)
    }

    fn button(button: StrokeButton, down: bool) -> SendOp {
        SendOp::Button { button, down }
    }

    fn wheel(delta: i32) -> SendOp {
        SendOp::Wheel {
            delta,
            horizontal: false,
        }
    }

    /// The centre of `TARGET`, (600, 450) on the desktop, as an absolute move.
    fn centre() -> SendOp {
        let (x, y) = SCREEN.absolute(600, 450);
        SendOp::MoveTo { x, y }
    }

    const Z: u16 = 0x5A;
    const Z_SCAN: u16 = 0x2C;
    const HID_Z: u16 = 0x1D;
    const HID_A: u16 = 0x04;

    // Pen.

    #[test]
    fn a_pen_stroke_goes_to_the_pen_device_from_the_virtual_origin() {
        let mut rig = Rig::new();
        rig.pen(PencilPhase::Down, PointerKind::Pen, StrokeButton::Left);
        let calls = rig.calls();
        assert_eq!(calls.len(), 2, "into range, then contact");
        let Call::Pen {
            position,
            pressure,
            pointer_flags,
            pen_flags,
        } = calls[1]
        else {
            panic!("not a pen frame: {:?}", calls[1]);
        };
        // (600, 450) on the desktop, with the virtual screen starting at x = -1000.
        assert_eq!(position, (1600, 450));
        assert_eq!(pressure, 512);
        assert_ne!(pointer_flags & POINTER_FLAG_DOWN, 0);
        assert_eq!(pen_flags, 0);
        rig.pen(PencilPhase::Up, PointerKind::Pen, StrokeButton::Left);
        assert!(
            matches!(rig.calls()[0], Call::Pen { pressure: 0, pointer_flags, .. } if pointer_flags & POINTER_FLAG_UP != 0)
        );
        assert_eq!(
            rig.feedback(),
            [Feedback::Drawing(true), Feedback::Drawing(false)]
        );
    }

    #[test]
    fn a_hover_moves_the_pen_without_touching_or_clicking() {
        let mut rig = Rig::new();
        rig.pen(PencilPhase::Hover, PointerKind::Pen, StrokeButton::Left);
        let calls = rig.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            calls[0],
            Call::Pen { pressure: 0, pointer_flags, .. }
                if pointer_flags & POINTER_FLAG_INRANGE != 0 && pointer_flags & POINTER_FLAG_INCONTACT == 0
        ));
        // Without a pen device a hover is a pointer move, and still no button.
        let mut rig = Rig::with(|backend| backend.pen = false);
        rig.pen(PencilPhase::Hover, PointerKind::Pen, StrokeButton::Left);
        assert_eq!(rig.sent(), [[centre()]]);
        assert!(rig.feedback().is_empty());
    }

    #[test]
    fn the_eraser_and_the_barrel_button_are_pen_flags() {
        let mut rig = Rig::new();
        rig.pen(PencilPhase::Down, PointerKind::Eraser, StrokeButton::Left);
        assert!(
            matches!(rig.calls()[1], Call::Pen { pen_flags, .. } if pen_flags == PEN_FLAG_INVERTED | PEN_FLAG_ERASER)
        );
        let mut rig = Rig::new();
        rig.pen_with(
            PencilPhase::Down,
            PointerKind::Pen,
            StrokeButton::Right,
            true,
        );
        assert!(matches!(
            rig.calls()[1],
            Call::Pen {
                pen_flags: PEN_FLAG_BARREL,
                ..
            }
        ));
    }

    #[test]
    fn fingers_other_buttons_and_a_missing_pen_device_use_the_mouse() {
        let stroke = |rig: &mut Rig, kind, button| {
            for phase in [PencilPhase::Down, PencilPhase::Moved, PencilPhase::Up] {
                rig.pen(phase, kind, button);
            }
            rig.sent()
        };
        let mouse = |b| {
            vec![
                vec![centre(), button(b, true)],
                vec![centre()],
                vec![centre(), button(b, false)],
            ]
        };
        let mut rig = Rig::new();
        assert_eq!(
            stroke(&mut rig, PointerKind::Finger, StrokeButton::Left),
            mouse(StrokeButton::Left)
        );
        // A right-button pen stroke from a client that knows no barrel button is a right drag.
        assert_eq!(
            stroke(&mut rig, PointerKind::Pen, StrokeButton::Right),
            mouse(StrokeButton::Right)
        );
        assert_eq!(
            stroke(&mut rig, PointerKind::Pen, StrokeButton::Middle),
            mouse(StrokeButton::Middle)
        );
        let mut rig = Rig::with(|backend| backend.pen = false);
        assert_eq!(
            stroke(&mut rig, PointerKind::Pen, StrokeButton::Left),
            mouse(StrokeButton::Left)
        );
    }

    #[test]
    fn a_hover_during_a_stroke_is_another_pointers_and_is_dropped() {
        let mut rig = Rig::new();
        rig.pen(PencilPhase::Down, PointerKind::Pen, StrokeButton::Left);
        rig.calls();
        rig.pen(PencilPhase::Hover, PointerKind::Pen, StrokeButton::Left);
        rig.pen(PencilPhase::HoverExit, PointerKind::Pen, StrokeButton::Left);
        assert!(rig.calls().is_empty());
        let mut rig = Rig::new();
        rig.pen(PencilPhase::Down, PointerKind::Finger, StrokeButton::Left);
        rig.calls();
        rig.pen(PencilPhase::Hover, PointerKind::Pen, StrokeButton::Left);
        assert!(
            rig.calls().is_empty(),
            "the mouse must not move under a mouse stroke either"
        );
        assert_eq!(rig.feedback(), [Feedback::Drawing(true)]);
    }

    #[test]
    fn a_cancelled_stroke_lifts_and_is_reported_when_it_had_touched() {
        let mut rig = Rig::new();
        rig.pen(PencilPhase::Down, PointerKind::Pen, StrokeButton::Left);
        rig.calls();
        rig.pen(PencilPhase::Cancel, PointerKind::Pen, StrokeButton::Left);
        assert_eq!(rig.calls().len(), 2, "up, then out of range");
        assert_eq!(
            rig.feedback(),
            [
                Feedback::Drawing(true),
                Feedback::Drawing(false),
                Feedback::StrokeCancelled
            ]
        );
        // A hover that is cancelled had made no mark.
        rig.pen(PencilPhase::Hover, PointerKind::Pen, StrokeButton::Left);
        rig.pen(PencilPhase::Cancel, PointerKind::Pen, StrokeButton::Left);
        assert!(rig.feedback().is_empty());
        // A mouse stroke lets its button go.
        rig.pen(PencilPhase::Down, PointerKind::Finger, StrokeButton::Left);
        rig.calls();
        rig.pen(PencilPhase::Cancel, PointerKind::Finger, StrokeButton::Left);
        assert_eq!(rig.sent(), [[button(StrokeButton::Left, false)]]);
        assert_eq!(rig.feedback().last(), Some(&Feedback::StrokeCancelled));
    }

    #[test]
    fn a_refused_pen_frame_still_moves_the_pointer() {
        let mut rig = Rig::with(|backend| backend.pen_refused = true);
        rig.pen(PencilPhase::Hover, PointerKind::Pen, StrokeButton::Left);
        let calls = rig.calls();
        assert!(matches!(calls[0], Call::Pen { .. }));
        assert_eq!(calls[1], Call::Send(vec![centre()]));
    }

    // Mouse.

    #[test]
    fn clicks_tap_or_hold() {
        let mut rig = Rig::new();
        let click = |rig: &mut Rig, button, phase| {
            let event = ClickEvent {
                x: 0.5,
                y: 0.5,
                button,
                phase,
                target: TARGET,
            };
            rig.engine.handle(InputEvent::Click(event), 0);
            rig.sent()
        };
        let right = StrokeButton::Right;
        assert_eq!(
            click(&mut rig, right, KeyPhase::Tap),
            [[centre(), button(right, true), button(right, false)]]
        );
        assert_eq!(
            click(&mut rig, right, KeyPhase::Down),
            [[centre(), button(right, true)]]
        );
        assert_eq!(
            click(&mut rig, right, KeyPhase::Up),
            [[centre(), button(right, false)]]
        );
        // A button whose up never comes is let go on release.
        click(&mut rig, StrokeButton::Middle, KeyPhase::Down);
        rig.engine.release_all();
        assert_eq!(rig.sent(), [[button(StrokeButton::Middle, false)]]);
    }

    #[test]
    fn scrolling_follows_the_fingers() {
        let mut rig = Rig::new();
        let event = ScrollEvent {
            dx: 0.01,
            dy: -0.1,
            target: TARGET,
        };
        rig.engine.handle(InputEvent::Scroll(event), 0);
        // Fingers right by 10 pixels scroll left; fingers up by 50 scroll down.
        assert_eq!(
            rig.sent(),
            [[
                wheel(-50),
                SendOp::Wheel {
                    delta: -10,
                    horizontal: true
                }
            ]]
        );
        let still = ScrollEvent {
            dx: 0.0,
            dy: 0.0001,
            target: TARGET,
        };
        rig.engine.handle(InputEvent::Scroll(still), 0);
        assert!(rig.calls().is_empty());
    }

    fn zoom(rig: &mut Rig, phase: GesturePhase, delta: f64, modifiers: u8) -> Vec<Vec<SendOp>> {
        let event = ZoomEvent {
            phase,
            delta,
            anchor: Some((0.5, 0.5)),
            modifiers,
            target: TARGET,
        };
        rig.engine.handle(InputEvent::Zoom(event), 0);
        rig.sent()
    }

    #[test]
    fn a_pinch_is_wheel_notches_with_the_zoom_modifier_under_the_fingers() {
        let mut rig = Rig::new();
        assert!(zoom(&mut rig, GesturePhase::Began, 0.0, CONTROL).is_empty());
        assert!(zoom(&mut rig, GesturePhase::Changed, 0.1, CONTROL).is_empty());
        assert_eq!(
            zoom(&mut rig, GesturePhase::Changed, 0.3, CONTROL),
            [[centre(), ctrl(false), wheel(120), ctrl(true)]]
        );
        assert_eq!(
            zoom(&mut rig, GesturePhase::Changed, -0.5, CONTROL),
            [[centre(), ctrl(false), wheel(-120), wheel(-120), ctrl(true)]]
        );
        // An end forgets the remainder.
        zoom(&mut rig, GesturePhase::Changed, 0.2, CONTROL);
        zoom(&mut rig, GesturePhase::Ended, 0.0, CONTROL);
        assert!(zoom(&mut rig, GesturePhase::Changed, 0.2, CONTROL).is_empty());
    }

    #[test]
    fn an_alt_zoom_never_leaves_alt_alone() {
        let mut rig = Rig::new();
        let mask = |up| key(keys::VK_MASK, 0, up);
        assert_eq!(
            zoom(&mut rig, GesturePhase::Changed, 0.5, OPTION),
            [[
                centre(),
                alt(false),
                wheel(120),
                mask(false),
                mask(true),
                alt(true)
            ]]
        );
    }

    #[test]
    fn a_zoom_does_not_press_a_modifier_the_client_holds() {
        let mut rig = Rig::new();
        rig.key(HID_LEFT_CONTROL, true, 0, 0);
        rig.calls();
        assert_eq!(
            zoom(&mut rig, GesturePhase::Changed, 0.5, CONTROL),
            [[centre(), wheel(120)]]
        );
    }

    fn pan(
        rig: &mut Rig,
        phase: GesturePhase,
        via: PanDelivery,
        anchor: Option<(f32, f32)>,
    ) -> Vec<Vec<SendOp>> {
        let event = PanDragEvent {
            phase,
            via,
            dx: 0.01,
            dy: 0.0,
            anchor,
            target: TARGET,
        };
        rig.engine.handle(InputEvent::PanDrag(event), 0);
        rig.sent()
    }

    #[test]
    fn a_pan_drags_with_the_middle_button_or_with_space() {
        let moved = |x, y| {
            let (x, y) = SCREEN.absolute(x, y);
            SendOp::MoveTo { x, y }
        };
        let middle = StrokeButton::Middle;
        let mut rig = Rig::new();
        assert_eq!(
            pan(
                &mut rig,
                GesturePhase::Began,
                PanDelivery::MiddleDrag,
                Some((0.5, 0.5))
            ),
            [
                vec![centre()],
                vec![button(middle, true)],
                vec![moved(610, 450)]
            ]
        );
        assert_eq!(
            pan(
                &mut rig,
                GesturePhase::Changed,
                PanDelivery::MiddleDrag,
                None
            ),
            [[moved(620, 450)]]
        );
        assert_eq!(
            pan(&mut rig, GesturePhase::Ended, PanDelivery::MiddleDrag, None),
            [[button(middle, false)]]
        );
        // Without a centroid the drag starts where the pointer is.
        let space = |up| key(keys::VK_SPACE, 0x39, up);
        let left = StrokeButton::Left;
        assert_eq!(
            pan(&mut rig, GesturePhase::Began, PanDelivery::SpaceDrag, None),
            [
                vec![moved(500, 600)],
                vec![space(false)],
                vec![button(left, true)],
                vec![moved(510, 600)]
            ]
        );
        rig.engine.release_all();
        assert_eq!(rig.sent(), [[button(left, false)], [space(true)]]);
    }

    // Touch.

    fn touch(rig: &mut Rig, phase: GesturePhase, now: u64) {
        let event = TouchEvent {
            phase,
            contacts: [(0.0, 0.0), (1.0, 1.0)],
            target: TARGET,
        };
        rig.engine.handle(InputEvent::Touch(event), now);
    }

    #[test]
    fn two_contacts_go_down_are_refreshed_and_lift() {
        let mut rig = Rig::new();
        let contacts = [(1100, 200), (2100, 700)];
        let held = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
        touch(&mut rig, GesturePhase::Began, 0);
        assert_eq!(
            rig.calls(),
            [Call::Touch(held | POINTER_FLAG_DOWN, contacts)]
        );
        assert_eq!(rig.engine.next_deadline(0), Some(40));
        rig.engine.tick(40);
        assert_eq!(
            rig.calls(),
            [Call::Touch(
                held | crate::pen::POINTER_FLAG_UPDATE,
                contacts
            )]
        );
        touch(&mut rig, GesturePhase::Ended, 50);
        assert_eq!(rig.calls(), [Call::Touch(POINTER_FLAG_UP, contacts)]);
        assert_eq!(rig.engine.next_deadline(50), None);
        assert!(rig.feedback().is_empty());
    }

    #[test]
    fn touch_that_cannot_be_injected_is_reported() {
        let mut rig = Rig::with(|backend| backend.touch = false);
        touch(&mut rig, GesturePhase::Began, 0);
        assert!(rig.calls().is_empty());
        assert_eq!(rig.feedback(), [Feedback::TouchFailed]);
        let mut rig = Rig::with(|backend| backend.touch_refused = true);
        touch(&mut rig, GesturePhase::Changed, 0);
        assert_eq!(rig.feedback(), [Feedback::TouchFailed]);
        assert_eq!(rig.engine.next_deadline(0), None);
    }

    // Keys.

    #[test]
    fn modifier_keys_go_down_and_up_as_themselves() {
        let mut rig = Rig::new();
        rig.key(HID_RIGHT_CONTROL, true, 0, 0);
        rig.key(HID_A, true, CONTROL, 0);
        rig.key(HID_A, false, CONTROL, 0);
        rig.key(HID_RIGHT_CONTROL, false, 0, 0);
        let right_ctrl = |up| SendOp::Key {
            vk: keys::VK_CONTROL,
            scan: 0x1D,
            extended: true,
            scan_only: false,
            up,
        };
        assert_eq!(
            rig.sent(),
            [
                vec![right_ctrl(false)],
                vec![key(0x41, 0x1E, false)],
                vec![key(0x41, 0x1E, true)],
                vec![right_ctrl(true)],
            ]
        );
    }

    #[test]
    fn a_flagged_modifier_is_pressed_for_the_key_and_released_with_it() {
        // An on-screen keyboard sends '+' as a Shift-flagged '=' with no Shift key.
        let mut rig = Rig::new();
        rig.key(0x2E, true, SHIFT, 0);
        rig.key(0x2E, false, SHIFT, 0);
        assert_eq!(
            rig.sent(),
            [
                vec![shift(false), key(0xBB, 0x0D, false)],
                vec![key(0xBB, 0x0D, true), shift(true)]
            ]
        );
    }

    #[test]
    fn a_held_modifier_is_not_released_by_another_keys_up() {
        let mut rig = Rig::new();
        rig.key(HID_LEFT_SHIFT, true, 0, 0);
        rig.key(HID_A, true, SHIFT, 0);
        rig.key(HID_A, false, SHIFT, 0);
        assert_eq!(
            rig.sent(),
            [
                vec![shift(false)],
                vec![key(0x41, 0x1E, false)],
                vec![key(0x41, 0x1E, true)]
            ]
        );
        // A flagged modifier whose own key then goes down belongs to that key from then on.
        let mut rig = Rig::new();
        rig.key(HID_A, true, SHIFT, 0);
        rig.key(HID_LEFT_SHIFT, true, 0, 0);
        rig.key(HID_A, false, SHIFT, 0);
        assert_eq!(rig.sent()[2], [key(0x41, 0x1E, true)]);
    }

    #[test]
    fn layout_dependent_positions_and_special_scan_codes() {
        let mut rig = Rig::new();
        // The ISO key beside the left Shift: the US layout has no key there, so the position.
        rig.key(0x64, true, 0, 0);
        // Keypad Enter, right Shift, an arrow, and a usage with no key at all.
        rig.key(0x58, true, 0, 0);
        rig.key(HID_RIGHT_SHIFT, true, 0, 0);
        rig.key(0x50, true, 0, 0);
        rig.key(0x03, true, 0, 0);
        let op = |vk, scan, extended, scan_only| SendOp::Key {
            vk,
            scan,
            extended,
            scan_only,
            up: false,
        };
        assert_eq!(
            rig.sent(),
            [
                [op(0, 0x56, false, true)],
                [op(keys::VK_RETURN, 0x1C, true, false)],
                [op(keys::VK_SHIFT, 0x36, false, false)],
                [op(keys::VK_LEFT, 0x4B, true, false)],
            ]
        );
    }

    #[test]
    fn a_key_tap_is_one_batch_and_leaves_held_modifiers_alone() {
        let mut rig = Rig::new();
        let tap = HostKey {
            hid_usage: HID_Z,
            modifiers: CONTROL | SHIFT,
        };
        rig.engine.handle(InputEvent::KeyTap(tap), 0);
        assert_eq!(
            rig.sent(),
            [[
                ctrl(false),
                shift(false),
                key(Z, Z_SCAN, false),
                key(Z, Z_SCAN, true),
                shift(true),
                ctrl(true)
            ]]
        );
        rig.key(HID_LEFT_CONTROL, true, 0, 0);
        rig.calls();
        rig.engine.handle(InputEvent::KeyTap(tap), 0);
        assert_eq!(
            rig.sent(),
            [[
                shift(false),
                key(Z, Z_SCAN, false),
                key(Z, Z_SCAN, true),
                shift(true)
            ]]
        );
    }

    #[test]
    fn a_shortcut_tap_is_held_briefly_and_the_next_key_waits() {
        let mut rig = Rig::new();
        rig.shortcut(
            KeySpec::char_with_hid("z", HID_Z),
            CONTROL,
            KeyPhase::Tap,
            100,
        );
        rig.shortcut(
            KeySpec::char_with_hid("z", HID_Z),
            CONTROL | SHIFT,
            KeyPhase::Tap,
            100,
        );
        assert_eq!(rig.sent(), [[ctrl(false), key(Z, Z_SCAN, false)]]);
        assert_eq!(rig.engine.next_deadline(100), Some(110));
        rig.engine.tick(109);
        assert!(rig.calls().is_empty());
        rig.engine.tick(110);
        assert_eq!(rig.sent(), [[key(Z, Z_SCAN, true), ctrl(true)]]);
        assert_eq!(rig.engine.next_deadline(110), Some(120));
        rig.engine.tick(120);
        assert_eq!(
            rig.sent(),
            [[ctrl(false), shift(false), key(Z, Z_SCAN, false)]]
        );
        rig.engine.tick(130);
        assert_eq!(
            rig.sent(),
            [[key(Z, Z_SCAN, true), shift(true), ctrl(true)]]
        );
        assert_eq!(rig.engine.next_deadline(130), None);
    }

    #[test]
    fn a_held_shortcut_keeps_its_modifiers_until_its_up() {
        let mut rig = Rig::new();
        let space = KeySpec::named(NamedKey::Space);
        let space_key = |up| key(keys::VK_SPACE, 0x39, up);
        rig.shortcut(space.clone(), CONTROL, KeyPhase::Down, 0);
        assert_eq!(rig.sent(), [[ctrl(false), space_key(false)]]);
        // Another shortcut with Ctrl does not press Ctrl again, or release it.
        rig.shortcut(KeySpec::char("z"), CONTROL, KeyPhase::Tap, 50);
        rig.engine.tick(60);
        assert_eq!(
            rig.sent(),
            [[key(Z, Z_SCAN, false)], [key(Z, Z_SCAN, true)]]
        );
        rig.shortcut(space.clone(), CONTROL, KeyPhase::Up, 100);
        assert_eq!(rig.sent(), [[space_key(true), ctrl(true)]]);
        // An up with nothing held does nothing.
        rig.shortcut(space, CONTROL, KeyPhase::Up, 200);
        assert!(rig.calls().is_empty());
    }

    #[test]
    fn a_second_down_of_a_held_shortcut_still_releases_its_modifiers() {
        let mut rig = Rig::new();
        let space = KeySpec::named(NamedKey::Space);
        let space_key = |up| key(keys::VK_SPACE, 0x39, up);
        rig.shortcut(space.clone(), CONTROL, KeyPhase::Down, 0);
        rig.shortcut(space.clone(), CONTROL, KeyPhase::Down, 100);
        assert_eq!(
            rig.sent(),
            [vec![ctrl(false), space_key(false)], vec![space_key(false)]]
        );
        rig.shortcut(space, CONTROL, KeyPhase::Up, 200);
        assert_eq!(rig.sent(), [[space_key(true), ctrl(true)]]);
    }

    #[test]
    fn a_held_shortcut_repeats_like_a_real_key() {
        let mut rig = Rig::new();
        let left = KeySpec::named(NamedKey::Left);
        let message = KeyShortcutMessage {
            key: left.clone(),
            modifiers: 0,
            phase: KeyPhase::Down,
            auto_repeat: true,
        };
        rig.engine.handle(InputEvent::KeyShortcut(message), 0);
        let down = SendOp::Key {
            vk: keys::VK_LEFT,
            scan: 0x4B,
            extended: true,
            scan_only: false,
            up: false,
        };
        assert_eq!(rig.sent(), [[down]]);
        // The fake computer repeats after 250 ms, then every 33 ms.
        assert_eq!(rig.engine.next_deadline(0), Some(250));
        rig.engine.tick(249);
        assert!(rig.calls().is_empty());
        rig.engine.tick(250);
        rig.engine.tick(283);
        assert_eq!(rig.sent(), [[down], [down]]);
        // After a minute it stops by itself.
        rig.engine.tick(60_000);
        assert!(rig.calls().is_empty());
        assert_eq!(rig.engine.next_deadline(60_000), None);
        rig.shortcut(left, 0, KeyPhase::Up, 60_001);
        assert_eq!(rig.sent().len(), 1);
    }

    #[test]
    fn a_key_the_layout_lacks_is_reported_and_not_pressed() {
        let mut rig = Rig::new();
        rig.shortcut(KeySpec::char("é"), CONTROL, KeyPhase::Tap, 0);
        assert!(rig.calls().is_empty());
        let feedback = rig.feedback();
        let [Feedback::KeyUnavailable(unavailable)] = feedback.as_slice() else {
            panic!("not one unavailable key: {feedback:?}");
        };
        assert_eq!(unavailable.key, KeySpec::char("é"));
        assert_eq!(unavailable.layout, "US");
        assert_eq!(
            unavailable.reason,
            KeyUnavailable::NotOnLayout {
                character: "é".into(),
                would_close: false
            }
        );
        assert_eq!(
            unavailable.notice.as_ref().map(|n| n.code.as_str()),
            Some("key.unavailable")
        );
    }

    #[test]
    fn text_is_typed_one_character_at_a_time() {
        let mut rig = Rig::new();
        rig.text("Hé", 0);
        let h = |up| key(0x48, 0x23, up);
        assert_eq!(rig.sent(), [[shift(false), h(false)]]);
        rig.engine.tick(10);
        assert_eq!(rig.sent(), [[h(true), shift(true)]]);
        rig.engine.tick(19);
        assert!(rig.calls().is_empty());
        rig.engine.tick(20);
        assert_eq!(
            rig.sent(),
            [[SendOp::Unicode {
                unit: 0xE9,
                up: false
            }]]
        );
        rig.engine.tick(30);
        assert_eq!(
            rig.sent(),
            [[SendOp::Unicode {
                unit: 0xE9,
                up: true
            }]]
        );
        assert_eq!(rig.engine.next_deadline(30), None);
    }

    #[test]
    fn keys_stay_in_order_behind_text_while_the_pen_goes_straight_through() {
        let mut rig = Rig::new();
        rig.text("a", 0);
        rig.shortcut(KeySpec::named(NamedKey::Return), 0, KeyPhase::Tap, 0);
        rig.key(HID_A, true, 0, 0);
        rig.calls();
        // A pen sample in the middle of the typing is not held up.
        rig.pen(PencilPhase::Hover, PointerKind::Pen, StrokeButton::Left);
        assert!(matches!(rig.calls().as_slice(), [Call::Pen { .. }]));
        let a = |up| key(0x41, 0x1E, up);
        let enter = |up| key(keys::VK_RETURN, 0x1C, up);
        let mut sent = Vec::new();
        for now in [10, 20, 30] {
            rig.engine.tick(now);
            sent.extend(rig.sent());
        }
        // The Return waits for the text, and the key position for the Return.
        assert_eq!(
            sent,
            [
                vec![a(true)],
                vec![enter(false)],
                vec![enter(true)],
                vec![a(false)]
            ]
        );
    }

    #[test]
    fn text_becomes_characters_while_a_shortcut_modifier_is_held() {
        let mut rig = Rig::new();
        rig.key(HID_LEFT_CONTROL, true, 0, 0);
        rig.calls();
        rig.text("a", 0);
        assert_eq!(
            rig.sent(),
            [[SendOp::Unicode {
                unit: 0x61,
                up: false
            }]]
        );
        // The same when Ctrl is held on the computer itself.
        let mut rig = Rig::with(|backend| backend.state.shortcut_mods_held = true);
        rig.text("a", 0);
        assert_eq!(
            rig.sent(),
            [[SendOp::Unicode {
                unit: 0x61,
                up: false
            }]]
        );
    }

    #[test]
    fn an_alt_gr_shortcut_without_modifiers_is_typed() {
        let mut rig = Rig::with(|backend| {
            backend.layout = || FakeLayout::new("AltGr", &[('@', 0x30, 0x0B, 6)]);
        });
        rig.shortcut(KeySpec::char("@"), 0, KeyPhase::Tap, 0);
        assert_eq!(
            rig.sent(),
            [[SendOp::Unicode {
                unit: 0x40,
                up: false
            }]]
        );
    }

    #[test]
    fn the_backlog_counts_key_work_until_it_is_done() {
        let mut rig = Rig::new();
        assert_eq!(rig.engine.backlog(), (0, 0));
        rig.text("ab", 0);
        rig.text("cde", 0);
        rig.key(HID_A, true, 0, 0);
        // "ab" is being typed and counts in full; "cde" and the key wait behind it.
        assert_eq!(rig.engine.backlog(), (3, 5));
        rig.engine.tick(10);
        rig.engine.tick(20);
        assert_eq!(rig.engine.backlog(), (3, 5));
        // Its last key is up: the next text starts, and the key still waits.
        rig.engine.tick(30);
        assert_eq!(rig.engine.backlog(), (2, 3));
        for now in (50..=100).step_by(10) {
            rig.engine.tick(now);
        }
        assert_eq!(rig.engine.backlog(), (0, 0));
        rig.text("abc", 200);
        rig.engine.release_all();
        assert_eq!(rig.engine.backlog(), (0, 0));
    }

    // Release.

    #[test]
    fn release_lets_go_of_everything_held() {
        let mut rig = Rig::new();
        rig.key(HID_LEFT_SHIFT, true, 0, 0);
        rig.key(HID_A, true, CONTROL, 0);
        rig.shortcut(KeySpec::named(NamedKey::Space), OPTION, KeyPhase::Down, 0);
        touch(&mut rig, GesturePhase::Began, 0);
        rig.pen(PencilPhase::Down, PointerKind::Pen, StrokeButton::Left);
        rig.calls();
        rig.feedback();
        rig.engine.release_all();
        let calls = rig.calls();
        assert_eq!(
            calls[0],
            Call::Send(vec![
                key(keys::VK_SPACE, 0x39, true),
                alt(true),
                key(0x41, 0x1E, true),
                shift(true),
                ctrl(true),
            ])
        );
        assert!(
            matches!(calls[1], Call::Touch(flags, _) if flags == POINTER_FLAG_UP | crate::pen::POINTER_FLAG_CANCELED)
        );
        assert!(
            matches!(calls[2], Call::Pen { pointer_flags, .. } if pointer_flags & POINTER_FLAG_UP != 0)
        );
        assert!(
            matches!(calls[3], Call::Pen { pointer_flags, .. } if pointer_flags & POINTER_FLAG_INRANGE == 0)
        );
        assert_eq!(calls.len(), 4);
        assert_eq!(rig.feedback(), [Feedback::Drawing(false)]);
        assert_eq!(rig.engine.next_deadline(0), None);
        // Nothing is held any more.
        rig.engine.release_all();
        assert!(rig.calls().is_empty());
    }

    #[test]
    fn release_in_the_middle_of_text_lifts_the_key_and_drops_the_rest() {
        let mut rig = Rig::new();
        rig.text("Abc", 0);
        rig.shortcut(KeySpec::char("z"), CONTROL, KeyPhase::Tap, 0);
        rig.calls();
        rig.engine.release_all();
        assert_eq!(rig.sent(), [[key(0x41, 0x1E, true), shift(true)]]);
        rig.engine.tick(1000);
        assert!(rig.calls().is_empty());
        assert_eq!(rig.engine.next_deadline(1000), None);
    }

    #[test]
    fn release_lets_go_of_a_mouse_stroke() {
        let mut rig = Rig::new();
        rig.pen(PencilPhase::Down, PointerKind::Finger, StrokeButton::Left);
        rig.calls();
        rig.engine.release_all();
        assert_eq!(rig.sent(), [[button(StrokeButton::Left, false)]]);
    }
}
