//! The Windows Ink pen as a stream host drives it: pen samples in, pointer frames out. Kept
//! apart from the injection so the rules are tested on every platform.
//!
//! - The pen is in range before it touches, as a real pen is: a contact that comes without a
//!   hover (a tablet without pen hover, a finger drawing) first enters range at the contact
//!   point. Windows apps take the tool from the proximity session, so without this an eraser
//!   stroke reached painting apps as the pen and painted.
//! - The eraser is its own tool: INVERTED for its whole session (hover included), ERASER while it
//!   touches. Tools change only out of contact; the old tool leaves range (an update without
//!   INRANGE) and the new one comes in.
//! - A contact that never hovered gets no hover exit from the client, so it leaves range when it
//!   lifts. Otherwise the next stroke would jump there in range, which some apps drop.
//! - The client's stroke owns the pen from its down to its up or cancel. A hover (or hover exit)
//!   in between comes from another pointer, such as a mouse over the client's canvas, and is
//!   dropped: it used to lift the eraser, bring the pen into range, and let the stroke's next
//!   move touch down again without an eraser range session, so the rest of the eraser stroke
//!   painted. A down while a stroke is still down (its up was lost) lifts that stroke first, as
//!   its own tool.

use streamcore_input::{PencilPhase, PointerKind};

pub const POINTER_FLAG_INRANGE: u32 = 0x2;
pub const POINTER_FLAG_INCONTACT: u32 = 0x4;
pub const POINTER_FLAG_FIRSTBUTTON: u32 = 0x10;
pub const POINTER_FLAG_PRIMARY: u32 = 0x2000;
pub const POINTER_FLAG_CONFIDENCE: u32 = 0x4000;
pub const POINTER_FLAG_CANCELED: u32 = 0x8000;
pub const POINTER_FLAG_DOWN: u32 = 0x1_0000;
pub const POINTER_FLAG_UPDATE: u32 = 0x2_0000;
pub const POINTER_FLAG_UP: u32 = 0x4_0000;
pub const PEN_FLAG_BARREL: u32 = 1;
pub const PEN_FLAG_INVERTED: u32 = 2;
pub const PEN_FLAG_ERASER: u32 = 4;

/// One pointer frame for the synthetic pen: where (desktop pixels), how hard, and the
/// `POINTER_FLAG_*` and `PEN_FLAG_*` bits `InjectSyntheticPointerInput` gets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PenFrame {
    pub x: i32,
    pub y: i32,
    pub force: f32,
    pub in_contact: bool,
    pub in_range: bool,
    pub pointer_flags: u32,
    pub pen_flags: u32,
    pub tilt_x: i32,
    pub tilt_y: i32,
}

/// One pen sample, already mapped to desktop pixels and Windows tilt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PenInput {
    pub phase: PencilPhase,
    /// Eraser, or anything else for the pen tip.
    pub kind: PointerKind,
    /// The barrel (side) button is held.
    pub barrel: bool,
    pub x: i32,
    pub y: i32,
    pub force: f32,
    pub tilt_x: i32,
    pub tilt_y: i32,
}

/// The pen's proximity, contact and tool.
#[derive(Debug)]
pub struct PenSession {
    eraser: bool,
    barrel: bool,
    in_range: bool,
    /// A DOWN was sent and its UP not yet.
    down_latch: bool,
    /// The client's stroke is down.
    down: bool,
    hovered_into_contact: bool,
    last: (i32, i32),
}

impl Default for PenSession {
    fn default() -> Self {
        Self::new()
    }
}

impl PenSession {
    pub fn new() -> Self {
        Self {
            eraser: false,
            barrel: false,
            in_range: false,
            down_latch: false,
            down: false,
            hovered_into_contact: false,
            last: (0, 0),
        }
    }

    /// The tool of the current proximity session is the eraser.
    pub fn is_eraser(&self) -> bool {
        self.eraser
    }

    /// The client's stroke is down.
    pub fn stroke_down(&self) -> bool {
        self.down
    }

    /// Windows has the pen in contact.
    pub fn in_contact(&self) -> bool {
        self.down_latch
    }

    pub fn in_range(&self) -> bool {
        self.in_range
    }

    /// The frames for one pen sample.
    pub fn sample(&mut self, input: PenInput) -> Vec<PenFrame> {
        let PenInput { phase, x, y, .. } = input;
        let tilt = (input.tilt_x, input.tilt_y);
        let mut frames = Vec::new();
        if phase == PencilPhase::Cancel {
            return self.cancel().0;
        }
        if self.down && matches!(phase, PencilPhase::Hover | PencilPhase::HoverExit) {
            return frames;
        }
        if self.down && phase == PencilPhase::Down {
            let (lx, ly) = self.last;
            self.lift(&mut frames, lx, ly, (0, 0));
        }
        let eraser = input.kind == PointerKind::Eraser;
        if eraser != self.eraser && !self.down && !self.down_latch {
            if self.in_range {
                let (lx, ly) = self.last;
                frames.push(self.frame(lx, ly, 0.0, false, false, (0, 0)));
            }
            self.eraser = eraser;
        }
        self.barrel = input.barrel;
        self.last = (x, y);
        match phase {
            PencilPhase::Hover => frames.push(self.frame(x, y, 0.0, false, true, tilt)),
            PencilPhase::Down => {
                self.hovered_into_contact = self.in_range && !self.down_latch;
                // Into range first, as the tool it is, then the contact.
                if !self.in_range {
                    frames.push(self.frame(x, y, 0.0, false, true, tilt));
                }
                self.down = true;
                frames.push(self.frame(x, y, input.force, true, true, tilt));
            }
            PencilPhase::Moved => {
                let contact = self.down;
                frames.push(self.frame(x, y, input.force, contact, true, tilt));
            }
            PencilPhase::Up => self.lift(&mut frames, x, y, tilt),
            PencilPhase::HoverExit => {
                self.down = false;
                // Already out of range (after a contact that did not hover): nothing to leave.
                if self.in_range || self.down_latch {
                    frames.push(self.frame(x, y, 0.0, false, false, tilt));
                }
            }
            PencilPhase::Cancel => {}
        }
        frames
    }

    /// The client's stroke lifts at x, y: out of contact, and out of range unless it hovered in.
    fn lift(&mut self, frames: &mut Vec<PenFrame>, x: i32, y: i32, tilt: (i32, i32)) {
        self.down = false;
        frames.push(self.frame(x, y, 0.0, false, true, tilt));
        if !self.hovered_into_contact {
            frames.push(self.frame(x, y, 0.0, false, false, tilt));
        }
    }

    /// The contact was a palm or the system cancelled it: lift and leave range. The flag says
    /// whether it had touched down, so the host can have the app take the mark back.
    pub fn cancel(&mut self) -> (Vec<PenFrame>, bool) {
        let mut frames = Vec::new();
        let touched = self.down_latch;
        let (x, y) = self.last;
        if self.down_latch {
            frames.push(self.frame(x, y, 0.0, false, true, (0, 0)));
        }
        if self.in_range {
            frames.push(self.frame(x, y, 0.0, false, false, (0, 0)));
        }
        self.down = false;
        (frames, touched)
    }

    /// The client went away: lift the pen and leave range, so nothing stays in contact and the
    /// next session's first stroke starts with its own DOWN.
    pub fn release(&mut self) -> Vec<PenFrame> {
        self.cancel().0
    }

    fn frame(
        &mut self,
        x: i32,
        y: i32,
        force: f32,
        in_contact: bool,
        in_range: bool,
        (tilt_x, tilt_y): (i32, i32),
    ) -> PenFrame {
        let mut pointer_flags = POINTER_FLAG_PRIMARY | POINTER_FLAG_CONFIDENCE;
        if in_range {
            pointer_flags |= POINTER_FLAG_INRANGE;
        }
        if in_contact {
            pointer_flags |= POINTER_FLAG_INCONTACT | POINTER_FLAG_FIRSTBUTTON;
            if !self.in_range || !self.down_latch {
                pointer_flags |= POINTER_FLAG_DOWN;
                self.down_latch = true;
            } else {
                pointer_flags |= POINTER_FLAG_UPDATE;
            }
        } else if self.down_latch {
            pointer_flags |= POINTER_FLAG_UP;
            self.down_latch = false;
        } else {
            // Hovering, or leaving range.
            pointer_flags |= POINTER_FLAG_UPDATE;
        }
        self.in_range = in_range;
        let mut pen_flags = 0;
        if self.eraser {
            pen_flags |= PEN_FLAG_INVERTED;
            if in_contact {
                pen_flags |= PEN_FLAG_ERASER;
            }
        } else if self.barrel {
            pen_flags |= PEN_FLAG_BARREL;
        }
        PenFrame {
            x,
            y,
            force,
            in_contact,
            in_range,
            pointer_flags,
            pen_flags,
            tilt_x,
            tilt_y,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(phase: PencilPhase, kind: PointerKind, x: i32, y: i32) -> PenInput {
        PenInput {
            phase,
            kind,
            barrel: false,
            x,
            y,
            force: 0.5,
            tilt_x: 0,
            tilt_y: 0,
        }
    }

    fn has(frame: &PenFrame, flags: u32) -> bool {
        frame.pointer_flags & flags == flags
    }

    fn stroke(pen: &mut PenSession, kind: PointerKind) -> Vec<PenFrame> {
        let mut frames = pen.sample(input(PencilPhase::Down, kind, 10, 10));
        frames.extend(pen.sample(input(PencilPhase::Moved, kind, 20, 20)));
        frames.extend(pen.sample(input(PencilPhase::Up, kind, 30, 30)));
        frames
    }

    const RANGE_CONTACT: u32 = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;

    #[test]
    fn frames_carry_primary_and_confidence() {
        let frames = PenSession::new().sample(input(PencilPhase::Down, PointerKind::Pen, 100, 200));
        assert_eq!(frames[0].pointer_flags, 0x0002_6002);
        assert_eq!(frames[1].pointer_flags, 0x0001_6016);
    }

    #[test]
    fn a_contact_without_hover_enters_range_first_and_leaves_it_on_lift() {
        let mut pen = PenSession::new();
        let frames = stroke(&mut pen, PointerKind::Pen);
        assert_eq!(frames.len(), 5);
        assert!(has(&frames[0], POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE));
        assert!(!has(&frames[0], POINTER_FLAG_INCONTACT));
        assert_eq!((frames[0].x, frames[0].y), (10, 10));
        assert!(has(&frames[1], RANGE_CONTACT | POINTER_FLAG_DOWN));
        assert_eq!(frames[1].force, 0.5);
        assert!(has(&frames[2], RANGE_CONTACT | POINTER_FLAG_UPDATE));
        assert!(has(&frames[3], POINTER_FLAG_INRANGE | POINTER_FLAG_UP));
        assert!(has(&frames[4], POINTER_FLAG_UPDATE));
        assert!(!has(&frames[4], POINTER_FLAG_INRANGE));
        assert!(!pen.in_range() && !pen.in_contact() && !pen.stroke_down());
    }

    #[test]
    fn a_stroke_that_hovered_in_stays_in_range_until_its_hover_exit() {
        let mut pen = PenSession::new();
        let hover = pen.sample(input(PencilPhase::Hover, PointerKind::Pen, 5, 5));
        assert_eq!(hover.len(), 1);
        assert!(has(&hover[0], POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE));
        assert!(!has(&hover[0], POINTER_FLAG_INCONTACT));
        let frames = stroke(&mut pen, PointerKind::Pen);
        assert_eq!(frames.len(), 3, "no extra range frames");
        assert!(pen.in_range());
        let exit = pen.sample(input(PencilPhase::HoverExit, PointerKind::Pen, 30, 30));
        assert_eq!(exit.len(), 1);
        assert!(!has(&exit[0], POINTER_FLAG_INRANGE));
        // A second exit has nothing to leave.
        assert!(
            pen.sample(input(PencilPhase::HoverExit, PointerKind::Pen, 30, 30))
                .is_empty()
        );
    }

    #[test]
    fn an_eraser_stroke_without_hover_is_the_eraser_before_it_touches() {
        let mut pen = PenSession::new();
        stroke(&mut pen, PointerKind::Pen);
        let frames = pen.sample(input(PencilPhase::Down, PointerKind::Eraser, 50, 60));
        assert!(pen.is_eraser());
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].pen_flags, PEN_FLAG_INVERTED);
        assert!(!frames[0].in_contact && frames[0].in_range);
        assert_eq!(frames[1].pen_flags, PEN_FLAG_INVERTED | PEN_FLAG_ERASER);
        let up = pen.sample(input(PencilPhase::Up, PointerKind::Eraser, 50, 60));
        assert!(up.iter().all(|f| f.pen_flags == PEN_FLAG_INVERTED));
    }

    #[test]
    fn a_tool_change_while_hovering_leaves_range_as_the_old_tool() {
        let mut pen = PenSession::new();
        pen.sample(input(PencilPhase::Hover, PointerKind::Pen, 1, 2));
        let frames = pen.sample(input(PencilPhase::Hover, PointerKind::Eraser, 3, 4));
        assert_eq!(frames.len(), 2);
        assert_eq!(
            (frames[0].x, frames[0].y, frames[0].in_range),
            (1, 2, false)
        );
        assert_eq!(frames[0].pen_flags, 0);
        assert_eq!((frames[1].x, frames[1].y, frames[1].in_range), (3, 4, true));
        assert_eq!(frames[1].pen_flags, PEN_FLAG_INVERTED);
    }

    #[test]
    fn the_tool_does_not_change_in_contact() {
        let mut pen = PenSession::new();
        pen.sample(input(PencilPhase::Down, PointerKind::Eraser, 1, 1));
        let frames = pen.sample(input(PencilPhase::Moved, PointerKind::Pen, 2, 2));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].pen_flags, PEN_FLAG_INVERTED | PEN_FLAG_ERASER);
    }

    #[test]
    fn a_hover_from_another_pointer_during_a_stroke_is_dropped() {
        let mut pen = PenSession::new();
        pen.sample(input(PencilPhase::Down, PointerKind::Eraser, 1, 1));
        assert!(
            pen.sample(input(PencilPhase::Hover, PointerKind::Pen, 9, 9))
                .is_empty()
        );
        assert!(
            pen.sample(input(PencilPhase::HoverExit, PointerKind::Pen, 9, 9))
                .is_empty()
        );
        assert!(pen.stroke_down() && pen.is_eraser());
        let frames = pen.sample(input(PencilPhase::Moved, PointerKind::Eraser, 2, 2));
        assert!(has(&frames[0], RANGE_CONTACT | POINTER_FLAG_UPDATE));
    }

    #[test]
    fn a_down_while_down_lifts_the_lost_stroke_as_its_own_tool() {
        let mut pen = PenSession::new();
        pen.sample(input(PencilPhase::Down, PointerKind::Eraser, 1, 1));
        let frames = pen.sample(input(PencilPhase::Down, PointerKind::Pen, 7, 7));
        // Eraser up and out of range at its last point, then the pen in range and down.
        assert_eq!(frames.len(), 4);
        assert!(has(&frames[0], POINTER_FLAG_UP));
        assert_eq!((frames[0].x, frames[0].pen_flags), (1, PEN_FLAG_INVERTED));
        assert!(!frames[1].in_range);
        assert_eq!(
            (frames[2].x, frames[2].pen_flags, frames[2].in_range),
            (7, 0, true)
        );
        assert!(has(&frames[3], RANGE_CONTACT | POINTER_FLAG_DOWN));
        assert!(!pen.is_eraser());
    }

    #[test]
    fn a_move_without_a_down_only_hovers() {
        let mut pen = PenSession::new();
        let frames = pen.sample(input(PencilPhase::Moved, PointerKind::Pen, 1, 1));
        assert_eq!(frames.len(), 1);
        assert!(!frames[0].in_contact && frames[0].in_range);
    }

    #[test]
    fn the_barrel_button_is_a_pen_flag_and_never_the_erasers() {
        let mut pen = PenSession::new();
        let mut sample = input(PencilPhase::Down, PointerKind::Pen, 1, 1);
        sample.barrel = true;
        let frames = pen.sample(sample);
        assert!(frames.iter().all(|f| f.pen_flags == PEN_FLAG_BARREL));
        pen.release();
        sample.kind = PointerKind::Eraser;
        let frames = pen.sample(sample);
        assert!(frames.iter().all(|f| f.pen_flags & PEN_FLAG_BARREL == 0));
    }

    #[test]
    fn cancel_lifts_and_leaves_range_and_says_whether_it_touched() {
        let mut pen = PenSession::new();
        pen.sample(input(PencilPhase::Down, PointerKind::Pen, 4, 5));
        let (frames, touched) = pen.cancel();
        assert!(touched);
        assert_eq!(frames.len(), 2);
        assert!(has(&frames[0], POINTER_FLAG_INRANGE | POINTER_FLAG_UP));
        assert!(!frames[1].in_range);
        assert_eq!((frames[1].x, frames[1].y), (4, 5));
        assert!(!pen.in_range() && !pen.in_contact() && !pen.stroke_down());
        // Hovering only: it leaves range and had not touched.
        pen.sample(input(PencilPhase::Hover, PointerKind::Pen, 1, 1));
        let (frames, touched) = pen.cancel();
        assert!(!touched);
        assert_eq!(frames.len(), 1);
        // Nothing to do when idle.
        assert_eq!(pen.cancel(), (Vec::new(), false));
        assert!(
            pen.sample(input(PencilPhase::Cancel, PointerKind::Pen, 0, 0))
                .is_empty()
        );
    }

    #[test]
    fn the_stroke_after_a_release_starts_with_its_own_down() {
        let mut pen = PenSession::new();
        pen.sample(input(PencilPhase::Down, PointerKind::Pen, 1, 1));
        assert_eq!(pen.release().len(), 2);
        let frames = pen.sample(input(PencilPhase::Down, PointerKind::Pen, 2, 2));
        assert!(has(&frames[1], POINTER_FLAG_DOWN));
    }
}
