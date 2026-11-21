//! The injector's inbox: the events posted and not yet taken by the injection thread, with the
//! limits that keep it from growing without end.
//!
//! At most [`MAX_PENDING_OPERATIONS`] events are pending at a time, counting those the thread has
//! taken but not finished (key events wait behind paced text), and at most
//! [`MAX_PENDING_TEXT_UNITS`] UTF-16 units of text. Under pressure pointer moves give way first:
//! from half full, a move replaces (or adds to) the move queued just before it, and when full a
//! move is dropped. Anything else that does not fit overflows, which the injector answers by
//! revoking the lease, discarding what is queued and releasing everything held. A key-up or a
//! button-up is therefore never lost on its own: it is either queued or made moot by the release.

use std::collections::VecDeque;

use streamcore_input::{GesturePhase, PencilPhase};

use crate::event::{InputEvent, text_units};

/// Most events pending at a time.
pub const MAX_PENDING_OPERATIONS: usize = 128;
/// Most UTF-16 units of text pending at a time, deletions before a text included. One text
/// event larger than this is refused whole.
pub const MAX_PENDING_TEXT_UNITS: usize = 4096;
/// From this many pending events on, pointer moves are merged.
const MERGE_FROM: usize = MAX_PENDING_OPERATIONS / 2;

/// What became of a posted event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    Queued,
    /// Folded into the pointer move queued just before it.
    Merged,
    /// A pointer move, dropped because the queue is full.
    Dropped,
    /// It does not fit and may not be dropped.
    Overflow,
}

#[derive(Default)]
pub(crate) struct Inbox {
    events: VecDeque<InputEvent>,
    text_units: usize,
}

impl Inbox {
    /// Admits one event. `busy` is what the injection thread has taken and not finished, as
    /// (events, text units).
    pub(crate) fn admit(&mut self, event: InputEvent, busy: (usize, usize)) -> Admission {
        let units = match &event {
            InputEvent::Text(message) => text_units(message),
            _ => 0,
        };
        let text_pending = self.text_units.saturating_add(busy.1);
        if units > 0 && text_pending.saturating_add(units) > MAX_PENDING_TEXT_UNITS {
            return Admission::Overflow;
        }
        let pending = self.events.len().saturating_add(busy.0);
        if pending >= MERGE_FROM
            && let Some(last) = self.events.back_mut()
            && merge(last, &event)
        {
            return Admission::Merged;
        }
        if pending >= MAX_PENDING_OPERATIONS {
            return if is_pointer_move(&event) {
                Admission::Dropped
            } else {
                Admission::Overflow
            };
        }
        self.text_units += units;
        self.events.push_back(event);
        Admission::Queued
    }

    /// The next event for the injection thread.
    pub(crate) fn take(&mut self) -> Option<InputEvent> {
        let event = self.events.pop_front()?;
        if let InputEvent::Text(message) = &event {
            self.text_units = self.text_units.saturating_sub(text_units(message));
        }
        Some(event)
    }

    /// Discards everything queued.
    pub(crate) fn clear(&mut self) {
        self.events.clear();
        self.text_units = 0;
    }
}

/// An event that only moves something already where it is: losing one changes nothing that a
/// later event does not set again, and nothing is pressed or released by it.
fn is_pointer_move(event: &InputEvent) -> bool {
    match event {
        InputEvent::Pen(pen) => {
            matches!(pen.sample.phase, PencilPhase::Hover | PencilPhase::Moved)
        }
        InputEvent::Touch(touch) => touch.phase == GesturePhase::Changed,
        InputEvent::Scroll(_) => true,
        InputEvent::PanDrag(pan) => pan.phase == GesturePhase::Changed,
        InputEvent::Zoom(zoom) => zoom.phase == GesturePhase::Changed,
        _ => false,
    }
}

/// Folds `next` into `last`, the event queued just before it, when both are the same kind of
/// pointer move: a newer position replaces the older one, and distances add up.
fn merge(last: &mut InputEvent, next: &InputEvent) -> bool {
    if !is_pointer_move(last) || !is_pointer_move(next) {
        return false;
    }
    match (last, next) {
        (InputEvent::Pen(last), InputEvent::Pen(next)) => {
            let same = last.sample.phase == next.sample.phase
                && last.sample.kind == next.sample.kind
                && last.sample.button == next.sample.button
                && last.right_button_is_barrel == next.right_button_is_barrel;
            if same {
                *last = *next;
            }
            same
        }
        (InputEvent::Touch(last), InputEvent::Touch(next)) => {
            *last = *next;
            true
        }
        (InputEvent::Scroll(last), InputEvent::Scroll(next)) if last.target == next.target => {
            last.dx += next.dx;
            last.dy += next.dy;
            true
        }
        (InputEvent::PanDrag(last), InputEvent::PanDrag(next))
            if last.via == next.via && last.target == next.target =>
        {
            last.dx += next.dx;
            last.dy += next.dy;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{PanDragEvent, PenEvent, ScrollEvent, TouchEvent, ZoomEvent};
    use crate::geometry::TargetRect;
    use streamcore_input::{
        KeyPacket, PanDelivery, PencilSample, PointerKind, StrokeButton, TextMessage,
    };

    const TARGET: TargetRect = TargetRect {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    };

    fn pen(phase: PencilPhase, x: f32) -> InputEvent {
        InputEvent::Pen(PenEvent {
            sample: PencilSample {
                phase,
                kind: PointerKind::Pen,
                button: StrokeButton::Left,
                x,
                y: 0.0,
                force: 0.5,
                altitude: 0.0,
                azimuth: 0.0,
                roll: 0.0,
                hover_z: 0.0,
                timestamp: 0.0,
            },
            target: TARGET,
            right_button_is_barrel: false,
        })
    }

    fn key(is_down: bool) -> InputEvent {
        InputEvent::Key(KeyPacket {
            is_down,
            hid_usage: 0x04,
            modifiers: 0,
        })
    }

    fn text(units: usize, replace_before: u32) -> InputEvent {
        InputEvent::Text(TextMessage {
            text: "a".repeat(units),
            replace_before,
        })
    }

    fn scroll(dy: f64) -> InputEvent {
        InputEvent::Scroll(ScrollEvent {
            dx: 0.0,
            dy,
            target: TARGET,
        })
    }

    const IDLE: (usize, usize) = (0, 0);

    #[test]
    fn events_queue_in_order_while_there_is_room() {
        let mut inbox = Inbox::default();
        for x in [0.1, 0.2, 0.3] {
            assert_eq!(
                inbox.admit(pen(PencilPhase::Moved, x), IDLE),
                Admission::Queued
            );
        }
        // Below half full nothing is merged: a stroke keeps every sample.
        assert_eq!(inbox.take(), Some(pen(PencilPhase::Moved, 0.1)));
        assert_eq!(inbox.take(), Some(pen(PencilPhase::Moved, 0.2)));
        assert_eq!(inbox.take(), Some(pen(PencilPhase::Moved, 0.3)));
        assert_eq!(inbox.take(), None);
    }

    #[test]
    fn from_half_full_a_move_replaces_the_move_before_it() {
        let mut inbox = Inbox::default();
        for _ in 0..MERGE_FROM - 1 {
            inbox.admit(key(true), IDLE);
        }
        assert_eq!(
            inbox.admit(pen(PencilPhase::Moved, 0.1), IDLE),
            Admission::Queued
        );
        assert_eq!(
            inbox.admit(pen(PencilPhase::Moved, 0.2), IDLE),
            Admission::Merged
        );
        // A different kind of sample is not a move of the same thing, and an up is never merged.
        assert_eq!(
            inbox.admit(pen(PencilPhase::Hover, 0.3), IDLE),
            Admission::Queued
        );
        assert_eq!(
            inbox.admit(pen(PencilPhase::Up, 0.4), IDLE),
            Admission::Queued
        );
        assert_eq!(
            inbox.admit(pen(PencilPhase::Up, 0.5), IDLE),
            Admission::Queued
        );
        let queued: Vec<_> = std::iter::from_fn(|| inbox.take())
            .skip(MERGE_FROM - 1)
            .collect();
        assert_eq!(
            queued,
            [
                pen(PencilPhase::Moved, 0.2),
                pen(PencilPhase::Hover, 0.3),
                pen(PencilPhase::Up, 0.4),
                pen(PencilPhase::Up, 0.5),
            ]
        );
    }

    #[test]
    fn merged_distances_add_up_and_positions_are_replaced() {
        let busy = (MERGE_FROM, 0);
        let mut inbox = Inbox::default();
        inbox.admit(scroll(0.25), busy);
        assert_eq!(inbox.admit(scroll(0.5), busy), Admission::Merged);
        assert_eq!(inbox.take(), Some(scroll(0.75)));

        let pan = |dx| {
            InputEvent::PanDrag(PanDragEvent {
                phase: GesturePhase::Changed,
                via: PanDelivery::MiddleDrag,
                dx,
                dy: 0.0,
                anchor: None,
                target: TARGET,
            })
        };
        inbox.admit(pan(1.0), busy);
        assert_eq!(inbox.admit(pan(2.0), busy), Admission::Merged);
        assert_eq!(inbox.take(), Some(pan(3.0)));

        let touch = |phase, x| {
            InputEvent::Touch(TouchEvent {
                phase,
                contacts: [(x, 0.0), (x, 1.0)],
                target: TARGET,
            })
        };
        inbox.admit(touch(GesturePhase::Began, 0.1), busy);
        assert_eq!(
            inbox.admit(touch(GesturePhase::Changed, 0.2), busy),
            Admission::Queued
        );
        assert_eq!(
            inbox.admit(touch(GesturePhase::Changed, 0.3), busy),
            Admission::Merged
        );
        assert_eq!(
            inbox.admit(touch(GesturePhase::Ended, 0.4), busy),
            Admission::Queued
        );
        assert_eq!(inbox.take(), Some(touch(GesturePhase::Began, 0.1)));
        assert_eq!(inbox.take(), Some(touch(GesturePhase::Changed, 0.3)));
        assert_eq!(inbox.take(), Some(touch(GesturePhase::Ended, 0.4)));
    }

    #[test]
    fn a_full_queue_drops_moves_and_overflows_on_anything_else() {
        let mut inbox = Inbox::default();
        for _ in 0..MAX_PENDING_OPERATIONS {
            assert_eq!(inbox.admit(key(true), IDLE), Admission::Queued);
        }
        assert_eq!(
            inbox.admit(pen(PencilPhase::Hover, 0.1), IDLE),
            Admission::Dropped
        );
        assert_eq!(
            inbox.admit(pen(PencilPhase::Moved, 0.1), IDLE),
            Admission::Dropped
        );
        assert_eq!(inbox.admit(scroll(0.1), IDLE), Admission::Dropped);
        let zoom = InputEvent::Zoom(ZoomEvent {
            phase: GesturePhase::Changed,
            delta: 0.1,
            anchor: None,
            modifiers: 0,
            target: TARGET,
        });
        assert_eq!(inbox.admit(zoom, IDLE), Admission::Dropped);
        // A key-up, a pen-up and a pen-down are never dropped quietly.
        assert_eq!(inbox.admit(key(false), IDLE), Admission::Overflow);
        assert_eq!(
            inbox.admit(pen(PencilPhase::Up, 0.1), IDLE),
            Admission::Overflow
        );
        assert_eq!(
            inbox.admit(pen(PencilPhase::Down, 0.1), IDLE),
            Admission::Overflow
        );
        assert_eq!(inbox.admit(text(1, 0), IDLE), Admission::Overflow);
        // One taken makes room for one.
        inbox.take();
        assert_eq!(inbox.admit(key(false), IDLE), Admission::Queued);
    }

    #[test]
    fn what_the_thread_has_not_finished_counts() {
        let mut inbox = Inbox::default();
        let busy = (MAX_PENDING_OPERATIONS - 1, 0);
        assert_eq!(inbox.admit(key(true), busy), Admission::Queued);
        assert_eq!(inbox.admit(key(false), busy), Admission::Overflow);
        assert_eq!(
            inbox.admit(key(false), (MAX_PENDING_OPERATIONS, 0)),
            Admission::Overflow
        );
    }

    #[test]
    fn text_is_bounded_and_never_cut_short() {
        let mut inbox = Inbox::default();
        assert_eq!(
            inbox.admit(text(MAX_PENDING_TEXT_UNITS + 1, 0), IDLE),
            Admission::Overflow
        );
        assert_eq!(
            inbox.admit(text(MAX_PENDING_TEXT_UNITS, 0), IDLE),
            Admission::Queued
        );
        assert_eq!(inbox.admit(text(1, 0), IDLE), Admission::Overflow);
        // Taken, but still being typed: it goes on counting through `busy`.
        inbox.take();
        assert_eq!(
            inbox.admit(text(1, 0), (1, MAX_PENDING_TEXT_UNITS)),
            Admission::Overflow
        );
        assert_eq!(
            inbox.admit(text(1, 0), (1, MAX_PENDING_TEXT_UNITS - 1)),
            Admission::Queued
        );
        // Deletions are keys to tap too, and an emoji is two units.
        inbox.clear();
        assert_eq!(inbox.admit(text(0, 4097), IDLE), Admission::Overflow);
        assert_eq!(inbox.admit(text(96, 4000), IDLE), Admission::Queued);
        inbox.clear();
        let emoji = InputEvent::Text(TextMessage {
            text: "\u{1F600}".into(),
            replace_before: 0,
        });
        assert_eq!(
            inbox.admit(text(MAX_PENDING_TEXT_UNITS - 1, 0), IDLE),
            Admission::Queued
        );
        assert_eq!(inbox.admit(emoji, IDLE), Admission::Overflow);
        assert_eq!(inbox.admit(text(1, 0), IDLE), Admission::Queued);
        // Other events are not held back by waiting text.
        assert_eq!(inbox.admit(key(true), IDLE), Admission::Queued);
    }

    #[test]
    fn clearing_forgets_the_text_too() {
        let mut inbox = Inbox::default();
        inbox.admit(text(MAX_PENDING_TEXT_UNITS, 0), IDLE);
        inbox.clear();
        assert_eq!(inbox.take(), None);
        assert_eq!(
            inbox.admit(text(MAX_PENDING_TEXT_UNITS, 0), IDLE),
            Admission::Queued
        );
    }
}
