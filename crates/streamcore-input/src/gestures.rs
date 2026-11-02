//! Two-finger gestures (`GestureMessage.kind = "twoFinger"`).
//!
//! - [`TwoFingerTracker`] (client): two touch points per frame → one message per
//!   frame with the change in magnification, rotation and pan since the last one.
//! - [`TwoFingerRouter`] (host): a message → what to post for the front app's
//!   `gestures.delivery`: trackpad magnify/rotate (native), the profile's
//!   fallback shortcuts, or modifier+scroll (1.2); the pan as a scroll or, for
//!   `gestures.pan` middleDrag / spaceDrag, as a mouse drag.
//! - [`FallbackAccumulator`] (host): the step arithmetic behind the shortcuts.
//!
//! Units: `scale` is the relative change in finger distance since the last
//! message (`d / d_prev - 1`), so magnifications compose by multiplication;
//! `rotation` is degrees, counter-clockwise positive as the user sees it;
//! `dx`/`dy` are points of the tablet view (y down); positions are normalized
//! to the streamed content and clamped to 0...1.
//!
//! Fallback steps: zoom accumulates in log space, `acc += ln(max(0.05, 1 +
//! scale))`, one step per `ln(1 + zoomStep)`, so a spread and the same pinch
//! back cancel; rotation accumulates in degrees, one step per
//! `rotateStepDegrees`. Whole steps are taken toward zero and the remainder
//! carries to the next message; at most [`MAX_STEPS_PER_MESSAGE`] steps per axis
//! per message are sent and the excess whole steps are dropped. The remainders
//! reset when a gesture begins or ends.
//!
//! Pan as a drag: the drag starts once the pan since the last press adds up to
//! [`PAN_DRAG_START_POINTS`] screen points (a pinch's centroid wobble is not a
//! pan), and is released before any zoom or rotate shortcut or modifier+scroll
//! (Krita reads Ctrl or Shift with the middle button or Space as zoom or
//! rotate), then starts again with the next pan.

use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::modifiers::HostOs;
use crate::profile::{self, DEFAULT_ROTATE_STEP_DEGREES, DEFAULT_ZOOM_STEP, Fallbacks, Shortcut};
use crate::shortcuts::resolve_shortcut;
use crate::vocabulary::{GestureDelivery, PanDelivery};
use crate::wire::{GestureKind, GestureMessage, GesturePhase, KeyShortcutMessage};

/// Most fallback shortcut presses per axis per message.
pub const MAX_STEPS_PER_MESSAGE: i32 = 3;
/// Native delivery: rotation below this many accumulated degrees is pinch noise.
pub const ROTATION_DEAD_ZONE_DEGREES: f64 = 3.0;
/// A pan delivered as a drag starts once it has moved this many screen points.
pub const PAN_DRAG_START_POINTS: f64 = 4.0;
/// 1.2 delivery: scroll pixels per unit of magnification.
pub const ZOOM_SCROLL_FACTOR: f64 = 300.0;
const EPSILON: f64 = 1e-9;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// MARK: - Client tracker

#[derive(Debug, Clone, Copy)]
struct Frame {
    p1: (f64, f64),
    p2: (f64, f64),
    width: f64,
    height: f64,
}

impl Frame {
    fn distance(&self) -> f64 {
        (self.p2.0 - self.p1.0).hypot(self.p2.1 - self.p1.1)
    }

    /// Counter-clockwise angle of p1→p2 as seen on a y-down screen, in degrees.
    fn angle(&self) -> f64 {
        (-(self.p2.1 - self.p1.1))
            .atan2(self.p2.0 - self.p1.0)
            .to_degrees()
    }

    fn centroid(&self) -> (f64, f64) {
        ((self.p1.0 + self.p2.0) / 2.0, (self.p1.1 + self.p2.1) / 2.0)
    }

    fn message(
        &self,
        phase: GesturePhase,
        scale: f64,
        rotation: f64,
        dx: f64,
        dy: f64,
    ) -> GestureMessage {
        let n = |v: f64, size: f64| (v / size).clamp(0.0, 1.0);
        let (cx, cy) = self.centroid();
        let mut m = GestureMessage::new(GestureKind::TwoFinger);
        m.phase = Some(phase);
        m.scale = Some(scale);
        m.rotation = Some(rotation);
        m.dx = Some(dx);
        m.dy = Some(dy);
        m.x = Some(n(cx, self.width));
        m.y = Some(n(cy, self.height));
        m.contacts = Some(vec![
            n(self.p1.0, self.width),
            n(self.p1.1, self.height),
            n(self.p2.0, self.width),
            n(self.p2.1, self.height),
        ]);
        m
    }
}

fn bare(phase: GesturePhase) -> GestureMessage {
    let mut m = GestureMessage::new(GestureKind::TwoFinger);
    m.phase = Some(phase);
    m
}

/// Below this finger spacing (points) the angle and the ratio are noise: scale
/// and rotation are reported as 0.
pub const MINIMUM_SPACING_POINTS: f64 = 1.0;

/// Turns two touch points per frame into `twoFinger` messages (PadPad's
/// `TwoFingerSampler`). Positions are normalized to the content rect; `width` and
/// `height` are that rect's size in unzoomed view points, so `dx`/`dy` come out in
/// tablet view points even while the client's view is zoomed.
#[derive(Debug, Default, uniffi::Object)]
pub struct TwoFingerTracker {
    last: Mutex<Option<Frame>>,
}

#[uniffi::export]
impl TwoFingerTracker {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Whether a gesture is in progress.
    pub fn is_active(&self) -> bool {
        lock(&self.last).is_some()
    }

    /// One frame: the two points normalized to the content rect and the content
    /// size in points. The first frame returns `began`; later frames return
    /// `changed`, or nothing when the points did not move. Points listed in the
    /// other order are matched back first. A frame with a non-finite value or an
    /// empty size is ignored; a size change re-baselines without a message.
    #[allow(clippy::too_many_arguments)]
    pub fn touches(
        &self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        width: f64,
        height: f64,
    ) -> Option<GestureMessage> {
        if ![x1, y1, x2, y2, width, height]
            .iter()
            .all(|v| v.is_finite())
            || width <= 0.0
            || height <= 0.0
        {
            return None;
        }
        let mut frame = Frame {
            p1: (x1 * width, y1 * height),
            p2: (x2 * width, y2 * height),
            width,
            height,
        };
        let mut last = lock(&self.last);
        let Some(prev) = *last else {
            *last = Some(frame);
            return Some(frame.message(GesturePhase::Began, 0.0, 0.0, 0.0, 0.0));
        };
        if (prev.width - width).abs() > EPSILON || (prev.height - height).abs() > EPSILON {
            *last = Some(frame);
            return None;
        }
        // Touches the platform listed the other way round are matched back to the
        // nearest previous point first, so a swap is not a half turn.
        let d = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).hypot(a.1 - b.1);
        if d(frame.p1, prev.p2) + d(frame.p2, prev.p1) < d(frame.p1, prev.p1) + d(frame.p2, prev.p2)
        {
            std::mem::swap(&mut frame.p1, &mut frame.p2);
        }
        if frame.p1 == prev.p1 && frame.p2 == prev.p2 {
            return None;
        }
        *last = Some(frame);
        let (before, now) = (prev.distance(), frame.distance());
        let (mut scale, mut rotation) = (0.0, 0.0);
        if before >= MINIMUM_SPACING_POINTS && now >= MINIMUM_SPACING_POINTS {
            scale = now / before - 1.0;
            rotation = frame.angle() - prev.angle();
            if rotation > 180.0 {
                rotation -= 360.0;
            } else if rotation <= -180.0 {
                rotation += 360.0;
            }
        }
        let (cx, cy) = frame.centroid();
        let (px, py) = prev.centroid();
        Some(frame.message(GesturePhase::Changed, scale, rotation, cx - px, cy - py))
    }

    /// The fingers lifted: a bare `ended` (kind and phase only), or nothing when idle.
    pub fn end(&self) -> Option<GestureMessage> {
        lock(&self.last).take().map(|_| bare(GesturePhase::Ended))
    }

    /// The system cancelled the touches (or the pen took over): a bare
    /// `cancelled`, or nothing when idle.
    pub fn cancel(&self) -> Option<GestureMessage> {
        lock(&self.last)
            .take()
            .map(|_| bare(GesturePhase::Cancelled))
    }
}

// MARK: - Fallback steps

/// Whole fallback steps: zoom > 0 = zoomIn presses, < 0 = zoomOut; rotate > 0 =
/// rotateLeft (counter-clockwise), < 0 = rotateRight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, uniffi::Record)]
pub struct FallbackCounts {
    pub zoom: i32,
    pub rotate: i32,
}

#[derive(Debug, Default, Clone, Copy)]
struct Steps {
    zoom: f64,
    rotate: f64,
}

impl Steps {
    fn zoom(&mut self, scale: f64, zoom_step: f64) -> i32 {
        if !scale.is_finite() || scale == 0.0 {
            return 0;
        }
        let unit = (1.0 + profile::positive_or(Some(zoom_step), DEFAULT_ZOOM_STEP)).ln();
        self.zoom += (1.0 + scale).max(0.05).ln();
        take(&mut self.zoom, unit)
    }

    fn rotate(&mut self, rotation: f64, rotate_step: f64) -> i32 {
        if !rotation.is_finite() || rotation == 0.0 {
            return 0;
        }
        let unit = profile::positive_or(Some(rotate_step), DEFAULT_ROTATE_STEP_DEGREES);
        self.rotate += rotation;
        take(&mut self.rotate, unit)
    }
}

fn take(acc: &mut f64, unit: f64) -> i32 {
    // Tolerate float noise so 0.1 + 0.1 + 0.1 style sums land on the step.
    let ratio = *acc / unit;
    let rounded = ratio.round();
    let whole = if (ratio - rounded).abs() < 1e-9 {
        rounded
    } else {
        ratio.trunc()
    };
    *acc -= whole * unit;
    let clamped = whole.clamp(
        -f64::from(MAX_STEPS_PER_MESSAGE),
        f64::from(MAX_STEPS_PER_MESSAGE),
    );
    #[allow(clippy::cast_possible_truncation)]
    let steps = clamped as i32;
    steps
}

/// Which fallback shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum FallbackKind {
    ZoomIn,
    ZoomOut,
    RotateLeft,
    RotateRight,
}

/// The result of [`FallbackAccumulator::feed`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct FallbackOutput {
    pub counts: FallbackCounts,
    /// The presses to post, literal for the host: zoom first, then rotation.
    pub steps: Vec<KeyShortcutMessage>,
    /// "zoom" / "rotate" when the profile has no shortcut for a step taken;
    /// reported once per gesture (send `gesture.fallback`).
    pub unavailable: Vec<String>,
}

#[derive(Debug, Default)]
struct AccumulatorState {
    steps: Steps,
    reported: Vec<String>,
}

/// The host's fallback step arithmetic for `gestures.delivery = shortcuts`.
#[derive(Debug, Default, uniffi::Object)]
pub struct FallbackAccumulator {
    state: Mutex<AccumulatorState>,
}

fn presses(
    count: i32,
    positive: Option<&Shortcut>,
    negative: Option<&Shortcut>,
    host: HostOs,
    out: &mut Vec<KeyShortcutMessage>,
) -> bool {
    let shortcut = if count > 0 { positive } else { negative };
    match shortcut {
        Some(s) => {
            for _ in 0..count.unsigned_abs() {
                out.push(resolve_shortcut(s.clone(), host));
            }
            true
        }
        None => count == 0,
    }
}

#[uniffi::export]
impl FallbackAccumulator {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Drops the remainders (a gesture began or ended).
    pub fn reset(&self) {
        *lock(&self.state) = AccumulatorState::default();
    }

    /// Adds one message's deltas; returns the whole steps to press now.
    pub fn feed_deltas(
        &self,
        scale: f64,
        rotation: f64,
        zoom_step: f64,
        rotate_step_degrees: f64,
    ) -> FallbackCounts {
        let mut s = lock(&self.state);
        FallbackCounts {
            zoom: s.steps.zoom(scale, zoom_step),
            rotate: s.steps.rotate(rotation, rotate_step_degrees),
        }
    }

    /// One `twoFinger` message with the front app's fallbacks: `began` resets
    /// first, `ended` resets after, `cancelled` resets and presses nothing.
    pub fn feed(
        &self,
        message: GestureMessage,
        fallbacks: Option<Fallbacks>,
        host: HostOs,
    ) -> FallbackOutput {
        let fallbacks = fallbacks.unwrap_or_default();
        let mut s = lock(&self.state);
        let phase = message.phase.unwrap_or(GesturePhase::Changed);
        if matches!(phase, GesturePhase::Began | GesturePhase::Cancelled) {
            *s = AccumulatorState::default();
        }
        let mut out = FallbackOutput::default();
        if phase != GesturePhase::Cancelled {
            out.counts = FallbackCounts {
                zoom: s.steps.zoom(
                    message.scale.unwrap_or(0.0),
                    fallbacks.zoom_step_or_default(),
                ),
                rotate: s.steps.rotate(
                    message.rotation.unwrap_or(0.0),
                    fallbacks.rotate_step_or_default(),
                ),
            };
            let zoom_ok = presses(
                out.counts.zoom,
                fallbacks.zoom_in.as_ref(),
                fallbacks.zoom_out.as_ref(),
                host,
                &mut out.steps,
            );
            let rotate_ok = presses(
                out.counts.rotate,
                fallbacks.rotate_left.as_ref(),
                fallbacks.rotate_right.as_ref(),
                host,
                &mut out.steps,
            );
            for (ok, kind) in [(zoom_ok, "zoom"), (rotate_ok, "rotate")] {
                if !ok && !s.reported.iter().any(|r| r == kind) {
                    s.reported.push(kind.to_owned());
                    out.unavailable.push(kind.to_owned());
                }
            }
        }
        if phase != GesturePhase::Changed && phase != GesturePhase::Began {
            *s = AccumulatorState::default();
        }
        out
    }
}

// MARK: - Host router

/// Which trackpad gesture a native bracket is named after.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum GestureSeries {
    Zoom,
    Rotation,
}

/// What the host posts for a `twoFinger` message, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum TwoFingerOutput {
    /// Opens the trackpad gesture bracket (macOS gesture begin event).
    GestureBegin { series: GestureSeries },
    /// Closes the bracket.
    GestureEnd { series: GestureSeries },
    /// Trackpad magnify by `amount` at the centroid.
    Magnify { amount: f64, phase: GesturePhase },
    /// Trackpad rotate by `degrees` (counter-clockwise +) at the centroid.
    Rotate { degrees: f64, phase: GesturePhase },
    /// Scroll for the pan, in screen points.
    Scroll {
        dx: f64,
        dy: f64,
        phase: GesturePhase,
    },
    /// 1.2 zoom: modifier+scroll by `dy` pixels (scale × 300).
    ZoomScroll { dy: f64, phase: GesturePhase },
    /// The pan as a mouse drag (`gestures.pan`), in screen points. `began`:
    /// put the pointer at the centroid and press (`middleDrag`: the middle
    /// button; `spaceDrag`: Space, then the left button), then move by dx/dy;
    /// `changed`: move by dx/dy; `ended`/`cancelled`: release what was pressed.
    PanDrag {
        via: PanDelivery,
        dx: f64,
        dy: f64,
        phase: GesturePhase,
    },
    /// Press a fallback shortcut (literal for the host).
    Shortcut {
        kind: FallbackKind,
        message: KeyShortcutMessage,
    },
    /// The profile has no shortcut for this ("zoom" | "rotate"); once per gesture.
    FallbackUnavailable { kind: String },
}

#[derive(Debug, Default)]
struct RouterState {
    bracket: Option<GestureSeries>,
    magnifying: bool,
    rotating: bool,
    scrolling: bool,
    zoom_scrolling: bool,
    dragging: Option<PanDelivery>,
    pending_pan: (f64, f64),
    pending_rotation: f64,
    steps: Steps,
    reported: Vec<String>,
}

impl RouterState {
    fn finish(&mut self, phase: GesturePhase) -> Vec<TwoFingerOutput> {
        use TwoFingerOutput::*;
        let mut out = Vec::new();
        if self.magnifying {
            out.push(Magnify { amount: 0.0, phase });
        }
        if self.rotating {
            out.push(Rotate {
                degrees: 0.0,
                phase,
            });
        }
        if let Some(series) = self.bracket {
            out.push(GestureEnd { series });
        }
        if self.scrolling {
            out.push(Scroll {
                dx: 0.0,
                dy: 0.0,
                phase,
            });
        }
        if self.zoom_scrolling {
            out.push(ZoomScroll { dy: 0.0, phase });
        }
        if let Some(via) = self.dragging {
            out.push(PanDrag {
                via,
                dx: 0.0,
                dy: 0.0,
                phase,
            });
        }
        *self = RouterState::default();
        out
    }

    /// Releases a pan drag in flight, so a shortcut or modifier+scroll is not
    /// read together with the held button.
    fn release_drag(&mut self, out: &mut Vec<TwoFingerOutput>) {
        if let Some(via) = self.dragging.take() {
            out.push(TwoFingerOutput::PanDrag {
                via,
                dx: 0.0,
                dy: 0.0,
                phase: GesturePhase::Ended,
            });
        }
        self.pending_pan = (0.0, 0.0);
    }

    fn pan(&mut self, dx: f64, dy: f64, via: PanDelivery, out: &mut Vec<TwoFingerOutput>) {
        use TwoFingerOutput::*;
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        if via == PanDelivery::Scroll {
            let phase = if self.scrolling {
                GesturePhase::Changed
            } else {
                GesturePhase::Began
            };
            self.scrolling = true;
            out.push(Scroll { dx, dy, phase });
            return;
        }
        if let Some(held) = self.dragging {
            out.push(PanDrag {
                via: held,
                dx,
                dy,
                phase: GesturePhase::Changed,
            });
            return;
        }
        self.pending_pan.0 += dx;
        self.pending_pan.1 += dy;
        let (px, py) = self.pending_pan;
        if px.hypot(py) >= PAN_DRAG_START_POINTS - EPSILON {
            self.dragging = Some(via);
            self.pending_pan = (0.0, 0.0);
            out.push(PanDrag {
                via,
                dx: px,
                dy: py,
                phase: GesturePhase::Began,
            });
        }
    }

    fn open_bracket(&mut self, series: GestureSeries, out: &mut Vec<TwoFingerOutput>) {
        if self.bracket.is_none() {
            self.bracket = Some(series);
            out.push(TwoFingerOutput::GestureBegin { series });
        }
    }

    fn report_once(&mut self, kind: &str, out: &mut Vec<TwoFingerOutput>) {
        if !self.reported.iter().any(|r| r == kind) {
            self.reported.push(kind.to_owned());
            out.push(TwoFingerOutput::FallbackUnavailable {
                kind: kind.to_owned(),
            });
        }
    }

    fn zoom_scroll(&mut self, scale: f64, out: &mut Vec<TwoFingerOutput>) {
        if scale == 0.0 {
            return;
        }
        let phase = if self.zoom_scrolling {
            GesturePhase::Changed
        } else {
            GesturePhase::Began
        };
        self.zoom_scrolling = true;
        out.push(TwoFingerOutput::ZoomScroll {
            dy: scale * ZOOM_SCROLL_FACTOR,
            phase,
        });
    }

    fn native(&mut self, scale: f64, rotation: f64, out: &mut Vec<TwoFingerOutput>) {
        use TwoFingerOutput::*;
        if scale != 0.0 {
            if !self.magnifying {
                self.open_bracket(GestureSeries::Zoom, out);
                out.push(Magnify {
                    amount: 0.0,
                    phase: GesturePhase::Began,
                });
                self.magnifying = true;
            }
            out.push(Magnify {
                amount: scale,
                phase: GesturePhase::Changed,
            });
        }
        if rotation != 0.0 {
            if self.rotating {
                out.push(Rotate {
                    degrees: rotation,
                    phase: GesturePhase::Changed,
                });
            } else {
                self.pending_rotation += rotation;
                if self.pending_rotation.abs() >= ROTATION_DEAD_ZONE_DEGREES {
                    self.open_bracket(GestureSeries::Rotation, out);
                    out.push(Rotate {
                        degrees: 0.0,
                        phase: GesturePhase::Began,
                    });
                    out.push(Rotate {
                        degrees: self.pending_rotation,
                        phase: GesturePhase::Changed,
                    });
                    self.rotating = true;
                    self.pending_rotation = 0.0;
                }
            }
        }
    }

    fn shortcuts(
        &mut self,
        scale: f64,
        rotation: f64,
        fallbacks: &Fallbacks,
        host: HostOs,
        out: &mut Vec<TwoFingerOutput>,
    ) {
        let press = |kind, shortcut: &Shortcut, count: i32, out: &mut Vec<TwoFingerOutput>| {
            for _ in 0..count.unsigned_abs() {
                out.push(TwoFingerOutput::Shortcut {
                    kind,
                    message: resolve_shortcut(shortcut.clone(), host),
                });
            }
        };
        if scale != 0.0 {
            match (&fallbacks.zoom_in, &fallbacks.zoom_out) {
                (Some(zin), Some(zout)) => {
                    let n = self.steps.zoom(scale, fallbacks.zoom_step_or_default());
                    if n > 0 {
                        press(FallbackKind::ZoomIn, zin, n, out);
                    } else {
                        press(FallbackKind::ZoomOut, zout, n, out);
                    }
                }
                _ => {
                    self.report_once("zoom", out);
                    self.zoom_scroll(scale, out);
                }
            }
        }
        if rotation != 0.0 {
            match (&fallbacks.rotate_left, &fallbacks.rotate_right) {
                (Some(left), Some(right)) => {
                    let n = self
                        .steps
                        .rotate(rotation, fallbacks.rotate_step_or_default());
                    if n > 0 {
                        press(FallbackKind::RotateLeft, left, n, out);
                    } else {
                        press(FallbackKind::RotateRight, right, n, out);
                    }
                }
                _ => self.report_once("rotate", out),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn deliver(
        &mut self,
        message: &GestureMessage,
        delivery: &str,
        pan: &str,
        fallbacks: &Fallbacks,
        pan_scale: f64,
        host: HostOs,
        out: &mut Vec<TwoFingerOutput>,
    ) {
        let finite = |v: Option<f64>| v.filter(|x| x.is_finite()).unwrap_or(0.0);
        let scale = finite(message.scale);
        let rotation = finite(message.rotation);
        let mut zoom_and_rotate = Vec::new();
        match GestureDelivery::from_name(delivery) {
            Some(GestureDelivery::Shortcuts) => {
                self.shortcuts(scale, rotation, fallbacks, host, &mut zoom_and_rotate);
            }
            Some(GestureDelivery::Scroll) => self.zoom_scroll(scale, &mut zoom_and_rotate),
            Some(GestureDelivery::Native) | None => {
                self.native(scale, rotation, &mut zoom_and_rotate);
            }
        }
        if zoom_and_rotate.iter().any(|o| {
            matches!(
                o,
                TwoFingerOutput::Shortcut { .. } | TwoFingerOutput::ZoomScroll { .. }
            )
        }) {
            self.release_drag(out);
        }
        out.extend(zoom_and_rotate);
        let factor = if pan_scale.is_finite() {
            pan_scale
        } else {
            1.0
        };
        let via = PanDelivery::from_name(pan).unwrap_or(PanDelivery::Scroll);
        self.pan(
            finite(message.dx) * factor,
            finite(message.dy) * factor,
            via,
            out,
        );
    }
}

/// The host's delivery of `twoFinger` messages for the front app's profile.
/// One per connection; call [`finish`](Self::finish) on disconnect or when the
/// front app changes mid-gesture.
#[derive(Debug, Default, uniffi::Object)]
pub struct TwoFingerRouter {
    state: Mutex<RouterState>,
}

#[uniffi::export]
impl TwoFingerRouter {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// One message. `delivery` is the profile's `gestures.delivery` (unknown or
    /// absent = native), `pan` its `gestures.pan` (unknown or absent = scroll),
    /// `pan_scale` screen points per tablet point.
    pub fn handle(
        &self,
        message: GestureMessage,
        delivery: Option<String>,
        pan: Option<String>,
        fallbacks: Option<Fallbacks>,
        pan_scale: f64,
        host: HostOs,
    ) -> Vec<TwoFingerOutput> {
        let delivery = delivery.unwrap_or_default();
        let pan = pan.unwrap_or_default();
        let fallbacks = fallbacks.unwrap_or_default();
        let mut s = lock(&self.state);
        let mut out = Vec::new();
        match message.phase.unwrap_or(GesturePhase::Changed) {
            GesturePhase::Began => {
                out = s.finish(GesturePhase::Cancelled);
                s.deliver(
                    &message, &delivery, &pan, &fallbacks, pan_scale, host, &mut out,
                );
            }
            GesturePhase::Changed => {
                s.deliver(
                    &message, &delivery, &pan, &fallbacks, pan_scale, host, &mut out,
                );
            }
            GesturePhase::Cancelled => out = s.finish(GesturePhase::Cancelled),
            GesturePhase::Ended | GesturePhase::Tap | GesturePhase::Down | GesturePhase::Up => {
                s.deliver(
                    &message, &delivery, &pan, &fallbacks, pan_scale, host, &mut out,
                );
                out.extend(s.finish(GesturePhase::Ended));
            }
        }
        out
    }

    /// Ends whatever is in flight with `phase` (ended or cancelled).
    pub fn finish(&self, phase: GesturePhase) -> Vec<TwoFingerOutput> {
        lock(&self.state).finish(phase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::KeySpec;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn tracker_measures_pinch_rotation_and_pan() {
        let t = TwoFingerTracker::new();
        assert!(t.end().is_none());
        let began = t.touches(0.25, 0.5, 0.75, 0.5, 400.0, 200.0).unwrap();
        assert_eq!(began.phase, Some(GesturePhase::Began));
        assert_eq!(began.x, Some(0.5));
        assert_eq!(began.contacts, Some(vec![0.25, 0.5, 0.75, 0.5]));
        assert!(t.touches(0.25, 0.5, 0.75, 0.5, 400.0, 200.0).is_none());
        // Spread to twice the distance.
        let m = t.touches(0.0, 0.5, 1.0, 0.5, 400.0, 200.0).unwrap();
        assert!(approx(m.scale.unwrap(), 1.0));
        assert!(approx(m.rotation.unwrap(), 0.0));
        // Rotate 90 degrees counter-clockwise on screen: p2 goes above p1.
        let m = t.touches(0.5, 1.0, 0.5, 0.0, 400.0, 400.0);
        assert!(m.is_none(), "size change re-baselines");
        let m = t.touches(0.0, 0.5, 1.0, 0.5, 400.0, 400.0).unwrap();
        assert!(approx(m.rotation.unwrap(), -90.0), "{:?}", m.rotation);
        // Pan 40 points right and 20 down.
        let m = t.touches(0.1, 0.55, 1.1, 0.55, 400.0, 400.0).unwrap();
        assert!(approx(m.dx.unwrap(), 40.0) && approx(m.dy.unwrap(), 20.0));
        assert_eq!(m.contacts.as_ref().unwrap()[2], 1.0, "clamped");
        let end = t.end().unwrap();
        assert_eq!(end.phase, Some(GesturePhase::Ended));
        assert!(!t.is_active());
        t.touches(0.1, 0.1, 0.2, 0.2, 10.0, 10.0);
        assert_eq!(t.cancel().unwrap().phase, Some(GesturePhase::Cancelled));
        assert!(t.touches(f64::NAN, 0.0, 0.0, 0.0, 1.0, 1.0).is_none());
        assert!(t.touches(0.0, 0.0, 0.0, 0.0, 0.0, 1.0).is_none());
    }

    #[test]
    fn rotation_wraps() {
        let t = TwoFingerTracker::new();
        t.touches(0.5, 0.5, 0.0, 0.51, 100.0, 100.0);
        let m = t.touches(0.5, 0.5, 0.0, 0.49, 100.0, 100.0).unwrap();
        assert!(m.rotation.unwrap().abs() < 5.0, "{:?}", m.rotation);
    }

    #[test]
    fn steps_follow_the_padwin_rules() {
        let a = FallbackAccumulator::new();
        let z = |s| a.feed_deltas(s, 0.0, 0.25, 15.0).zoom;
        assert_eq!([z(0.1), z(0.1), z(0.1), z(0.1)], [0, 0, 1, 0]);
        a.reset();
        assert_eq!(
            a.feed_deltas(9.0, -200.0, 0.25, 15.0),
            FallbackCounts {
                zoom: 3,
                rotate: -3
            }
        );
        a.reset();
        assert_eq!(
            a.feed_deltas(0.26, 16.0, 0.0, -3.0),
            FallbackCounts { zoom: 1, rotate: 1 }
        );
    }

    #[test]
    fn feed_presses_and_reports_missing_shortcuts() {
        let a = FallbackAccumulator::new();
        let f = Fallbacks {
            zoom_in: Some(Shortcut::new(KeySpec::char("+"), 8)),
            zoom_out: Some(Shortcut::new(KeySpec::char("-"), 8)),
            ..Fallbacks::default()
        };
        let mut m = GestureMessage::new(GestureKind::TwoFinger);
        m.phase = Some(GesturePhase::Began);
        m.scale = Some(0.6);
        m.rotation = Some(40.0);
        let out = a.feed(m.clone(), Some(f.clone()), HostOs::Windows);
        assert_eq!(out.counts, FallbackCounts { zoom: 2, rotate: 2 });
        assert_eq!(out.steps.len(), 2);
        assert_eq!(out.steps[0].modifiers, 2, "primary is Ctrl on Windows");
        assert_eq!(out.unavailable, vec!["rotate".to_owned()]);
        m.phase = Some(GesturePhase::Changed);
        let out = a.feed(m.clone(), Some(f.clone()), HostOs::Windows);
        assert!(out.unavailable.is_empty(), "reported once per gesture");
        m.phase = Some(GesturePhase::Cancelled);
        assert_eq!(
            a.feed(m, Some(f), HostOs::Windows),
            FallbackOutput::default()
        );
    }

    #[test]
    fn router_brackets_native_gestures() {
        let r = TwoFingerRouter::new();
        let mut m = GestureMessage::new(GestureKind::TwoFinger);
        m.phase = Some(GesturePhase::Began);
        m.rotation = Some(-5.0);
        let out = r.handle(m.clone(), None, None, None, 1.0, HostOs::Mac);
        assert_eq!(
            out[0],
            TwoFingerOutput::GestureBegin {
                series: GestureSeries::Rotation
            }
        );
        assert_eq!(out.len(), 3);
        m.phase = Some(GesturePhase::Cancelled);
        let out = r.handle(m, None, None, None, 1.0, HostOs::Mac);
        assert_eq!(out.len(), 2);
        assert!(r.finish(GesturePhase::Ended).is_empty());
    }

    fn pan_message(phase: GesturePhase, dx: f64, dy: f64, scale: f64) -> GestureMessage {
        let mut m = GestureMessage::new(GestureKind::TwoFinger);
        m.phase = Some(phase);
        m.dx = Some(dx);
        m.dy = Some(dy);
        m.scale = Some(scale);
        m
    }

    #[test]
    fn pan_drags_start_past_the_dead_zone_and_yield_to_shortcuts() {
        use GesturePhase::*;
        let r = TwoFingerRouter::new();
        let f = Fallbacks {
            zoom_in: Some(Shortcut::new(KeySpec::char("="), 8)),
            zoom_out: Some(Shortcut::new(KeySpec::char("-"), 8)),
            ..Fallbacks::default()
        };
        let pan = Some("middleDrag".to_owned());
        let shortcuts = Some("shortcuts".to_owned());
        let go = |m| {
            r.handle(
                m,
                shortcuts.clone(),
                pan.clone(),
                Some(f.clone()),
                2.0,
                HostOs::Windows,
            )
        };
        assert!(
            go(pan_message(Began, 1.0, 0.0, 0.0)).is_empty(),
            "2 points: wobble"
        );
        let out = go(pan_message(Changed, 0.0, 2.0, 0.0));
        assert_eq!(
            out,
            vec![TwoFingerOutput::PanDrag {
                via: PanDelivery::MiddleDrag,
                dx: 2.0,
                dy: 4.0,
                phase: Began
            }]
        );
        let out = go(pan_message(Changed, 3.0, 0.0, 0.0));
        assert_eq!(
            out,
            vec![TwoFingerOutput::PanDrag {
                via: PanDelivery::MiddleDrag,
                dx: 6.0,
                dy: 0.0,
                phase: Changed
            }]
        );
        // A zoom step releases the button before the shortcut.
        let out = go(pan_message(Changed, 0.0, 0.0, 0.3));
        assert!(
            matches!(out[0], TwoFingerOutput::PanDrag { phase: Ended, dx, .. } if dx == 0.0),
            "{out:?}"
        );
        assert!(matches!(
            out[1],
            TwoFingerOutput::Shortcut {
                kind: FallbackKind::ZoomIn,
                ..
            }
        ));
        assert_eq!(out.len(), 2);
        let out = go(pan_message(Changed, 3.0, 0.0, 0.0));
        assert!(matches!(
            out[0],
            TwoFingerOutput::PanDrag { phase: Began, .. }
        ));
        let out = go(pan_message(Ended, 0.0, 0.0, 0.0));
        assert_eq!(
            out,
            vec![TwoFingerOutput::PanDrag {
                via: PanDelivery::MiddleDrag,
                dx: 0.0,
                dy: 0.0,
                phase: Ended
            }]
        );
        assert!(r.finish(Cancelled).is_empty());
        // Unknown or absent pan is the scroll.
        let out = r.handle(
            pan_message(Began, 1.0, 0.0, 0.0),
            shortcuts.clone(),
            Some("orbit".into()),
            None,
            1.0,
            HostOs::Mac,
        );
        assert!(matches!(
            out[0],
            TwoFingerOutput::Scroll { phase: Began, .. }
        ));
        let out = r.handle(
            pan_message(Cancelled, 0.0, 0.0, 0.0),
            shortcuts,
            None,
            None,
            1.0,
            HostOs::Mac,
        );
        assert!(matches!(
            out[0],
            TwoFingerOutput::Scroll {
                phase: Cancelled,
                ..
            }
        ));
    }

    #[test]
    fn fallbacks_use_the_hosts_own_key() {
        let mut zoom_in = Shortcut::new(KeySpec::char("+"), 8);
        zoom_in.hosts = Some(crate::profile::HostShortcuts {
            windows: Some(crate::profile::HostShortcut {
                key: KeySpec::char("="),
                modifiers: 8,
                extra: None,
            }),
            ..Default::default()
        });
        let f = Fallbacks {
            zoom_in: Some(zoom_in),
            zoom_out: Some(Shortcut::new(KeySpec::char("-"), 8)),
            ..Fallbacks::default()
        };
        for (host, want) in [(HostOs::Windows, "="), (HostOs::Mac, "+")] {
            let r = TwoFingerRouter::new();
            let out = r.handle(
                pan_message(GesturePhase::Began, 0.0, 0.0, 0.3),
                Some("shortcuts".into()),
                None,
                Some(f.clone()),
                1.0,
                host,
            );
            match &out[0] {
                TwoFingerOutput::Shortcut { message, .. } => {
                    assert_eq!(message.key.char_value(), Some(want))
                }
                other => panic!("{other:?}"),
            }
        }
    }
}
