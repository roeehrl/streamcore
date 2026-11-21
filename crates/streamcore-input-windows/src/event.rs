//! The events the injector takes and what it reports back. They are plain typed values: decoding
//! a client's messages, and deciding which window they are for, is the host's job.

use std::fmt;

use streamcore_input::{
    GesturePhase, HostKey, KeyPacket, KeyPhase, KeyShortcutMessage, KeySpec, PanDelivery,
    PencilSample, ServerNotice, StrokeButton, TextMessage,
};

use crate::geometry::TargetRect;
use crate::layout::KeyUnavailable;

/// One input event. Points are normalized (0 to 1) across the event's `target`.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// A pen, eraser, finger or mouse-button sample. A hover moves the pen (or the pointer)
    /// without touching, so it never activates the window under it.
    Pen(PenEvent),
    /// A mouse button at a point.
    Click(ClickEvent),
    /// The mouse wheel.
    Scroll(ScrollEvent),
    /// A pinch, delivered as wheel notches with a modifier held.
    Zoom(ZoomEvent),
    /// A two-finger pan delivered as a mouse drag.
    PanDrag(PanDragEvent),
    /// Two touch contacts.
    Touch(TouchEvent),
    /// A physical key position going down or up, with the modifiers its event flagged.
    Key(KeyPacket),
    /// A key position tapped with literal modifiers, in one batch so nothing lands in between.
    KeyTap(HostKey),
    /// A key named by what it types, resolved on the focused window's keyboard layout.
    KeyShortcut(KeyShortcutMessage),
    /// Committed text.
    Text(TextMessage),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PenEvent {
    pub sample: PencilSample,
    pub target: TargetRect,
    /// A right-button pen stroke is the pen's barrel button. When false it is a right mouse
    /// drag, as clients that know no barrel button mean it.
    pub right_button_is_barrel: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClickEvent {
    pub x: f32,
    pub y: f32,
    pub button: StrokeButton,
    /// Tap, or down and up apart.
    pub phase: KeyPhase,
    pub target: TargetRect,
}

/// Two-finger scroll. `dx`, `dy`: how far the fingers moved, normalized to the target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollEvent {
    pub dx: f64,
    pub dy: f64,
    pub target: TargetRect,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoomEvent {
    /// `Began` and `Ended` (or `Cancelled`) bracket a pinch; anything else carries a change.
    pub phase: GesturePhase,
    /// The scale change since the last event: spreading the fingers (above 0) zooms in.
    pub delta: f64,
    /// Where the fingers are, so the app zooms about that point.
    pub anchor: Option<(f32, f32)>,
    /// The literal modifiers the front app zooms with on the wheel: Ctrl in most apps, Alt in
    /// some (where Ctrl+wheel scrolls sideways). The host knows the front app; the injector does
    /// not look.
    pub modifiers: u8,
    pub target: TargetRect,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanDragEvent {
    pub phase: GesturePhase,
    /// `MiddleDrag` or `SpaceDrag`; `Scroll` presses nothing.
    pub via: PanDelivery,
    /// The pan since the last event, normalized to the target.
    pub dx: f64,
    pub dy: f64,
    /// The fingers' centroid, where a drag starts; the pointer's position when `None`.
    pub anchor: Option<(f32, f32)>,
    pub target: TargetRect,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchEvent {
    /// `Ended` lifts the fingers and `Cancelled` cancels them; anything else puts them down or
    /// moves them.
    pub phase: GesturePhase,
    pub contacts: [(f32, f32); 2],
    pub target: TargetRect,
}

/// A shortcut key that was not pressed, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct UnavailableKey {
    pub key: KeySpec,
    pub reason: KeyUnavailable,
    /// The name of the layout the key was looked for on.
    pub layout: String,
    /// The shared guard's notice for the client, when it refused the key.
    pub notice: Option<ServerNotice>,
}

/// What a text event costs against the injector's text limit: the UTF-16 units it types plus
/// the characters it deletes first, each of which is a key to tap.
pub(crate) fn text_units(message: &TextMessage) -> usize {
    let deleted = usize::try_from(message.replace_before).unwrap_or(usize::MAX);
    message.text.encode_utf16().count().saturating_add(deleted)
}

/// What the injector tells the host. Injection is asynchronous, so outcomes that matter to the
/// host come back this way.
#[derive(Debug, Clone, PartialEq)]
pub enum Feedback {
    /// A shortcut's key is not on the focused window's layout and nothing was pressed.
    KeyUnavailable(Box<UnavailableKey>),
    /// A cancelled stroke had already touched down; the host may have the app undo the mark.
    StrokeCancelled,
    /// A stroke went into contact (true) or lifted (false); a host may hide the captured cursor
    /// while drawing.
    Drawing(bool),
    /// Windows refused a pen frame; the pointer still follows as a mouse move. Reported once,
    /// then not again until a frame has been accepted.
    PenFailed,
    /// Touch contacts could not be injected (no touch device, or Windows refused them) and were
    /// lifted; the host may deliver the gesture another way.
    TouchFailed,
}

/// Receives [`Feedback`] on the injection thread. It must return quickly and must not call back
/// into the injector; post to the host's own queue.
pub trait FeedbackSink: Send + 'static {
    fn feedback(&self, feedback: Feedback);
}

impl<F: Fn(Feedback) + Send + 'static> FeedbackSink for F {
    fn feedback(&self, feedback: Feedback) {
        self(feedback);
    }
}

/// Why an event was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectError {
    /// The lease was revoked, or belongs to another injector.
    StaleLease,
    /// The queue is full and this event was a pointer move (a hover, a move of a stroke or of
    /// touch contacts, a scroll, a pan or a pinch change), so it was dropped. The lease is still
    /// good and nothing is held because of it; the next move carries on.
    Dropped,
    /// The queue is full, or too much text is waiting, and this event could not be dropped
    /// without losing a key-up or a release. The lease has been revoked, everything queued was
    /// discarded and everything held is being released, as after
    /// [`InputLease::revoke`](crate::InputLease::revoke). Nothing of the event was injected: text
    /// is never cut short. The host takes a new lease when its client is ready again.
    Overflow,
    /// The injection thread has stopped.
    Stopped,
}

impl fmt::Display for InjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleLease => write!(f, "the input lease is stale"),
            Self::Dropped => write!(f, "the input queue is full; a pointer move was dropped"),
            Self::Overflow => write!(f, "the input queue overflowed; the lease was revoked"),
            Self::Stopped => write!(f, "the injection thread has stopped"),
        }
    }
}

impl std::error::Error for InjectError {}
