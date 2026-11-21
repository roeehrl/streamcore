//! A two-finger pan delivered as a mouse drag: what to press, move and release, kept apart from
//! Win32 so the order is tested on every platform.

use streamcore_input::{GesturePhase, PanDelivery};

/// One thing to post for a pan drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanDragStep {
    /// Put the pointer at this point, in desktop pixels.
    MoveTo(i32, i32),
    /// Press the middle mouse button (`middle`) or the left one.
    Press {
        middle: bool,
    },
    Release {
        middle: bool,
    },
    SpaceDown,
    SpaceUp,
}

/// The state of one pan drag: a middle drag holds the middle button, a space drag holds Space and
/// then the left button.
///
/// `Began` moves to the start point and presses; every event moves the pointer by the pan, as
/// absolute positions so Windows' pointer acceleration cannot scale it, with the sub-pixel
/// remainder carried; `Ended` and `Cancelled` release in reverse order.
#[derive(Debug, Default)]
pub struct PanDrag {
    held: Option<Held>,
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Middle,
    Space,
}

impl PanDrag {
    /// Whether a button (and Space) is held.
    pub fn active(&self) -> bool {
        self.held.is_some()
    }

    /// The steps for one pan event. `dx`, `dy`: the pan in pixels. `start`: where a drag starts
    /// (the fingers' centroid, else the pointer). A delivery other than the two drags presses
    /// nothing.
    pub fn plan(
        &mut self,
        phase: GesturePhase,
        via: PanDelivery,
        dx: f64,
        dy: f64,
        start: (i32, i32),
    ) -> Vec<PanDragStep> {
        if matches!(phase, GesturePhase::Ended | GesturePhase::Cancelled) {
            return self.release();
        }
        let mut steps = Vec::new();
        if phase == GesturePhase::Began || self.held.is_none() {
            steps.extend(self.release());
            let held = match via {
                PanDelivery::MiddleDrag => Held::Middle,
                PanDelivery::SpaceDrag => Held::Space,
                PanDelivery::Scroll => return steps,
            };
            self.held = Some(held);
            (self.x, self.y) = (f64::from(start.0), f64::from(start.1));
            steps.push(PanDragStep::MoveTo(start.0, start.1));
            if held == Held::Space {
                steps.push(PanDragStep::SpaceDown);
            }
            steps.push(PanDragStep::Press {
                middle: held == Held::Middle,
            });
        }
        if !dx.is_finite() || !dy.is_finite() || (dx == 0.0 && dy == 0.0) {
            return steps;
        }
        let from = (round(self.x), round(self.y));
        self.x += dx;
        self.y += dy;
        let to = (round(self.x), round(self.y));
        if to != from {
            steps.push(PanDragStep::MoveTo(to.0, to.1));
        }
        steps
    }

    /// Releases whatever is held (the button, then Space); nothing when idle. Also run when the
    /// client goes away, so nothing stays held.
    pub fn release(&mut self) -> Vec<PanDragStep> {
        match self.held.take() {
            None => Vec::new(),
            Some(Held::Middle) => vec![PanDragStep::Release { middle: true }],
            Some(Held::Space) => vec![PanDragStep::Release { middle: false }, PanDragStep::SpaceUp],
        }
    }
}

fn round(value: f64) -> i32 {
    value.round_ties_even() as i32
}

#[cfg(test)]
mod tests {
    use super::PanDragStep::*;
    use super::*;

    #[test]
    fn a_middle_drag_presses_at_the_start_moves_and_releases() {
        let mut pan = PanDrag::default();
        let began = pan.plan(
            GesturePhase::Began,
            PanDelivery::MiddleDrag,
            0.0,
            0.0,
            (100, 200),
        );
        assert_eq!(began, [MoveTo(100, 200), Press { middle: true }]);
        assert!(pan.active());
        let moved = pan.plan(
            GesturePhase::Changed,
            PanDelivery::MiddleDrag,
            10.0,
            -5.0,
            (0, 0),
        );
        assert_eq!(moved, [MoveTo(110, 195)]);
        let ended = pan.plan(
            GesturePhase::Ended,
            PanDelivery::MiddleDrag,
            3.0,
            3.0,
            (0, 0),
        );
        assert_eq!(ended, [Release { middle: true }]);
        assert!(!pan.active());
        assert!(pan.release().is_empty());
    }

    #[test]
    fn a_space_drag_holds_space_around_the_left_button() {
        let mut pan = PanDrag::default();
        let began = pan.plan(
            GesturePhase::Began,
            PanDelivery::SpaceDrag,
            2.0,
            0.0,
            (5, 5),
        );
        assert_eq!(
            began,
            [
                MoveTo(5, 5),
                SpaceDown,
                Press { middle: false },
                MoveTo(7, 5)
            ]
        );
        let cancelled = pan.plan(
            GesturePhase::Cancelled,
            PanDelivery::SpaceDrag,
            0.0,
            0.0,
            (0, 0),
        );
        assert_eq!(cancelled, [Release { middle: false }, SpaceUp]);
    }

    #[test]
    fn sub_pixel_pans_add_up() {
        let mut pan = PanDrag::default();
        pan.plan(
            GesturePhase::Began,
            PanDelivery::MiddleDrag,
            0.0,
            0.0,
            (0, 0),
        );
        let mut moves = Vec::new();
        for _ in 0..10 {
            moves.extend(pan.plan(
                GesturePhase::Changed,
                PanDelivery::MiddleDrag,
                0.3,
                0.0,
                (9, 9),
            ));
        }
        assert_eq!(moves, [MoveTo(1, 0), MoveTo(2, 0), MoveTo(3, 0)]);
    }

    #[test]
    fn a_change_without_a_began_starts_the_drag_and_a_second_began_restarts_it() {
        let mut pan = PanDrag::default();
        let first = pan.plan(
            GesturePhase::Changed,
            PanDelivery::MiddleDrag,
            1.0,
            0.0,
            (10, 10),
        );
        assert_eq!(
            first,
            [MoveTo(10, 10), Press { middle: true }, MoveTo(11, 10)]
        );
        let again = pan.plan(
            GesturePhase::Began,
            PanDelivery::SpaceDrag,
            0.0,
            0.0,
            (20, 20),
        );
        assert_eq!(
            again,
            [
                Release { middle: true },
                MoveTo(20, 20),
                SpaceDown,
                Press { middle: false }
            ]
        );
    }

    #[test]
    fn scroll_delivery_and_bad_numbers_press_or_move_nothing() {
        let mut pan = PanDrag::default();
        assert!(
            pan.plan(GesturePhase::Began, PanDelivery::Scroll, 5.0, 5.0, (1, 1))
                .is_empty()
        );
        assert!(!pan.active());
        pan.plan(
            GesturePhase::Began,
            PanDelivery::MiddleDrag,
            0.0,
            0.0,
            (0, 0),
        );
        assert!(
            pan.plan(
                GesturePhase::Changed,
                PanDelivery::MiddleDrag,
                f64::NAN,
                1.0,
                (0, 0)
            )
            .is_empty()
        );
        assert!(
            pan.plan(
                GesturePhase::Changed,
                PanDelivery::MiddleDrag,
                1.0,
                f64::INFINITY,
                (0, 0)
            )
            .is_empty()
        );
    }
}
