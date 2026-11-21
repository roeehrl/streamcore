//! The events the injector takes and what it reports back. They are plain typed values: decoding
//! a client's messages, and deciding which window they are for, is the host's job.

use std::fmt;

use streamcore_input::{
    GesturePhase, HostKey, KeyPacket, KeyPhase, KeyShortcutMessage, KeySpec, PanDelivery,
    PencilSample, ServerNotice, StrokeButton, TextMessage,
};

use crate::geometry::TargetRect;
use crate::layout::KeyUnavailable;

/// One input event. Points are normalized (0 to 1) across the event's `target` and confined to
/// it (see [`TargetRect`]); key events have no target and go to the window with the keyboard
/// focus.
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
    /// The fingers' centroid, where a drag starts; the pointer's position when `None` (moved
    /// into the target if it is outside). The drag stops at the target's edge.
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

impl InputEvent {
    /// Whether the event may be injected at all: its target has pixels, and every number in it
    /// is finite. A point outside its target is not a reason to refuse; it is clamped.
    pub(crate) fn is_valid(&self) -> bool {
        let finite = |values: &[f32]| values.iter().all(|value| value.is_finite());
        let point = |point: Option<(f32, f32)>| point.is_none_or(|(x, y)| finite(&[x, y]));
        match self {
            Self::Pen(pen) => {
                let s = &pen.sample;
                pen.target.is_valid()
                    && finite(&[s.x, s.y, s.force, s.altitude, s.azimuth, s.roll, s.hover_z])
            }
            Self::Click(click) => click.target.is_valid() && finite(&[click.x, click.y]),
            Self::Scroll(scroll) => {
                scroll.target.is_valid() && scroll.dx.is_finite() && scroll.dy.is_finite()
            }
            Self::Zoom(zoom) => {
                zoom.target.is_valid() && zoom.delta.is_finite() && point(zoom.anchor)
            }
            Self::PanDrag(pan) => {
                pan.target.is_valid()
                    && pan.dx.is_finite()
                    && pan.dy.is_finite()
                    && point(pan.anchor)
            }
            Self::Touch(touch) => {
                touch.target.is_valid() && touch.contacts.iter().all(|c| point(Some(*c)))
            }
            Self::Key(_) | Self::KeyTap(_) | Self::KeyShortcut(_) | Self::Text(_) => true,
        }
    }
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
    /// The lease ended without its holder's doing. Reported once per lease, after everything it
    /// held has been released; not reported when the holder revokes or drops its own lease, or
    /// when the injector itself is dropped.
    LeaseRevoked(RevokeReason),
    /// Touch contacts could not be injected (no touch device, or Windows refused them) and were
    /// lifted; the host may deliver the gesture another way.
    TouchFailed,
}

/// Why a lease ended without its holder's doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeReason {
    /// An event that could not be dropped did not fit the queue
    /// ([`InjectError::Overflow`]).
    Overflow,
    /// Another caller took the lease over ([`Injector::take_over`](crate::Injector::take_over)).
    TakenOver,
    /// The host revoked it ([`Injector::revoke`](crate::Injector::revoke)).
    Revoked,
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
    /// The event carries a number that is not finite (a coordinate, an anchor, a distance, a
    /// force or a tilt), or a target rectangle without pixels. Nothing was queued and the lease
    /// is untouched.
    InvalidEvent,
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
            Self::InvalidEvent => write!(f, "the event has a non-finite number or an empty target"),
            Self::Dropped => write!(f, "the input queue is full; a pointer move was dropped"),
            Self::Overflow => write!(f, "the input queue overflowed; the lease was revoked"),
            Self::Stopped => write!(f, "the injection thread has stopped"),
        }
    }
}

impl std::error::Error for InjectError {}

#[cfg(test)]
mod tests {
    use super::*;
    use streamcore_input::{PencilPhase, PointerKind};

    const TARGET: TargetRect = TargetRect {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    };
    const EMPTY: TargetRect = TargetRect {
        x: 0,
        y: 0,
        width: 100,
        height: 0,
    };
    const BAD: [f32; 3] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];

    fn pen(change: impl FnOnce(&mut PenEvent)) -> InputEvent {
        let mut event = PenEvent {
            sample: PencilSample {
                phase: PencilPhase::Moved,
                kind: PointerKind::Pen,
                button: StrokeButton::Left,
                x: 0.5,
                y: 0.5,
                force: 0.5,
                altitude: 1.0,
                azimuth: 0.0,
                roll: 0.0,
                hover_z: 0.0,
                timestamp: 0.0,
            },
            target: TARGET,
            right_button_is_barrel: false,
        };
        change(&mut event);
        InputEvent::Pen(event)
    }

    #[test]
    fn points_outside_the_target_are_valid_and_get_clamped() {
        assert!(pen(|_| {}).is_valid());
        assert!(pen(|e| (e.sample.x, e.sample.y) = (3.0, -1.0)).is_valid());
    }

    #[test]
    fn a_pen_sample_with_a_number_that_is_not_finite_is_refused() {
        for bad in BAD {
            assert!(!pen(|e| e.sample.x = bad).is_valid());
            assert!(!pen(|e| e.sample.y = bad).is_valid());
            assert!(!pen(|e| e.sample.force = bad).is_valid());
            assert!(!pen(|e| e.sample.altitude = bad).is_valid());
            assert!(!pen(|e| e.sample.azimuth = bad).is_valid());
            assert!(!pen(|e| e.sample.roll = bad).is_valid());
            assert!(!pen(|e| e.sample.hover_z = bad).is_valid());
        }
        assert!(!pen(|e| e.target = EMPTY).is_valid());
    }

    #[test]
    fn a_click_is_checked() {
        let click = |x, y, target| {
            InputEvent::Click(ClickEvent {
                x,
                y,
                button: StrokeButton::Left,
                phase: KeyPhase::Tap,
                target,
            })
        };
        assert!(click(-5.0, 5.0, TARGET).is_valid());
        assert!(!click(0.5, 0.5, EMPTY).is_valid());
        for bad in BAD {
            assert!(!click(bad, 0.5, TARGET).is_valid());
            assert!(!click(0.5, bad, TARGET).is_valid());
        }
    }

    #[test]
    fn touch_contacts_are_checked() {
        let touch = |contacts, target| {
            InputEvent::Touch(TouchEvent {
                phase: GesturePhase::Changed,
                contacts,
                target,
            })
        };
        assert!(touch([(0.0, 0.0), (9.0, -9.0)], TARGET).is_valid());
        assert!(!touch([(0.0, 0.0), (1.0, 1.0)], EMPTY).is_valid());
        for bad in BAD {
            assert!(!touch([(bad, 0.0), (1.0, 1.0)], TARGET).is_valid());
            assert!(!touch([(0.0, 0.0), (1.0, bad)], TARGET).is_valid());
        }
    }

    #[test]
    fn scroll_zoom_and_pan_are_checked() {
        let scroll = |dx, dy, target| InputEvent::Scroll(ScrollEvent { dx, dy, target });
        let zoom = |delta, anchor, target| {
            InputEvent::Zoom(ZoomEvent {
                phase: GesturePhase::Changed,
                delta,
                anchor,
                modifiers: 0,
                target,
            })
        };
        let pan = |dx, dy, anchor, target| {
            InputEvent::PanDrag(PanDragEvent {
                phase: GesturePhase::Changed,
                via: PanDelivery::MiddleDrag,
                dx,
                dy,
                anchor,
                target,
            })
        };
        assert!(scroll(5.0, -5.0, TARGET).is_valid());
        assert!(zoom(0.5, None, TARGET).is_valid());
        assert!(zoom(0.5, Some((2.0, -2.0)), TARGET).is_valid());
        assert!(pan(5.0, 5.0, None, TARGET).is_valid());
        assert!(!scroll(0.0, 0.0, EMPTY).is_valid());
        assert!(!zoom(0.5, None, EMPTY).is_valid());
        assert!(!pan(0.0, 0.0, None, EMPTY).is_valid());
        for bad in BAD {
            let wide = f64::from(bad);
            assert!(!scroll(wide, 0.0, TARGET).is_valid());
            assert!(!scroll(0.0, wide, TARGET).is_valid());
            assert!(!zoom(wide, None, TARGET).is_valid());
            assert!(!zoom(0.5, Some((bad, 0.5)), TARGET).is_valid());
            assert!(!zoom(0.5, Some((0.5, bad)), TARGET).is_valid());
            assert!(!pan(wide, 0.0, None, TARGET).is_valid());
            assert!(!pan(0.0, wide, None, TARGET).is_valid());
            assert!(!pan(0.0, 0.0, Some((bad, 0.5)), TARGET).is_valid());
            assert!(!pan(0.0, 0.0, Some((0.5, bad)), TARGET).is_valid());
        }
    }
}
