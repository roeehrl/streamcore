//! Coordinate mapping: normalized points to desktop pixels, desktop pixels to what `SendInput`
//! and the synthetic pointer devices want, pen tilt and pressure, and pinch zoom to wheel
//! notches.

/// The rectangle pointer input lands in, in desktop pixels: the coordinates Windows reports
/// window and monitor rectangles in to a per-monitor DPI-aware caller (physical pixels), whose
/// origin is the primary monitor's top-left (so a monitor to the left of or above it has negative
/// coordinates).
///
/// The rectangle is the edge of what the host lets a client point at. Every normalized point is
/// confined to it: a coordinate below 0 or above 1 lands on the nearest edge pixel inside the
/// rectangle, never beyond it, and a pan drag stops at its edge. An event with a coordinate that
/// is not a finite number, or with a rectangle whose width or height is not positive, is refused
/// ([`InjectError::InvalidEvent`](crate::InjectError::InvalidEvent)).
///
/// It scopes the mouse, the pen and touch only. Keys, shortcuts and text carry no rectangle:
/// Windows delivers them to whichever window has the keyboard focus. A host that confines
/// control to one window must give that window the focus before it injects keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl TargetRect {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Whether the rectangle has any pixels.
    pub fn is_valid(&self) -> bool {
        self.width > 0 && self.height > 0
    }

    /// A normalized point (0 to 1 across the rectangle) as a desktop pixel inside the rectangle.
    /// Coordinates outside 0 to 1 are clamped to its edge pixels.
    pub fn point(&self, nx: f32, ny: f32) -> (i32, i32) {
        let along = |n: f32, origin: i32, size: i32| {
            let size = size.max(1);
            // 1.0 is the far edge, whose last pixel is one short of the size.
            let offset = round(f64::from(n.clamp(0.0, 1.0)) * f64::from(size)).min(size - 1);
            origin.saturating_add(offset)
        };
        (
            along(nx, self.x, self.width),
            along(ny, self.y, self.height),
        )
    }

    /// A desktop pixel moved to the nearest pixel inside the rectangle.
    pub fn confine(&self, x: i32, y: i32) -> (i32, i32) {
        let within = |value: i32, origin: i32, size: i32| {
            value.clamp(origin, origin.saturating_add(size.max(1) - 1))
        };
        (
            within(x, self.x, self.width),
            within(y, self.y, self.height),
        )
    }

    /// A normalized distance in pixels.
    pub fn delta(&self, ndx: f64, ndy: f64) -> (f64, f64) {
        (
            ndx * f64::from(self.width.max(1)),
            ndy * f64::from(self.height.max(1)),
        )
    }
}

pub(crate) fn round(value: f64) -> i32 {
    value.round_ties_even() as i32
}

/// The bounding rectangle of all monitors, in desktop pixels (`SM_XVIRTUALSCREEN` and friends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualScreen {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl VirtualScreen {
    /// A desktop point as `SendInput` takes an absolute move with `MOUSEEVENTF_VIRTUALDESK`: 0 to
    /// 65535 across the virtual screen.
    pub fn absolute(&self, x: i32, y: i32) -> (i32, i32) {
        let scale = |value: i32, origin: i32, size: i32| {
            (f64::from(value.saturating_sub(origin)) * 65535.0 / f64::from((size - 1).max(1)))
                as i32
        };
        (scale(x, self.x, self.width), scale(y, self.y, self.height))
    }

    /// A desktop point as `InjectSyntheticPointerInput` takes it: relative to the top-left of the
    /// virtual screen, not of the primary monitor. With a monitor to the left of or above the
    /// primary the virtual origin is negative, and primary-relative pixels land one monitor over.
    pub fn pointer(&self, x: i32, y: i32) -> (i32, i32) {
        (x.saturating_sub(self.x), y.saturating_sub(self.y))
    }
}

/// A stylus's altitude (0 flat to π/2 upright) and azimuth, in radians, as Windows pen tilt in
/// degrees (-90 to 90 on each axis), the form painting apps expect.
pub fn tilt_degrees(altitude: f32, azimuth: f32) -> (i32, i32) {
    if altitude <= 0.0 || !altitude.is_finite() || !azimuth.is_finite() {
        return (0, 0);
    }
    let from_vertical = std::f64::consts::FRAC_PI_2 - f64::from(altitude);
    let azimuth = f64::from(azimuth);
    let degrees = |component: f64| {
        round((from_vertical.tan() * component).atan().to_degrees()).clamp(-90, 90)
    };
    (degrees(azimuth.cos()), degrees(azimuth.sin()))
}

/// Force (0 to about 1) as pen pressure, 0 to 1024, with a floor while in contact so a light
/// touch still draws.
pub fn pen_pressure(force: f32, in_contact: bool) -> u32 {
    let floor = i32::from(in_contact);
    round(f64::from(force * 1024.0)).clamp(floor, 1024) as u32
}

/// Scale change per wheel notch, in log space (about 1.28x): one zoom step per notch in most
/// apps.
const ZOOM_PER_NOTCH: f64 = 0.25;
/// A jumpy frame cannot flood the app.
const MAX_NOTCHES_PER_EVENT: i32 = 3;

/// A pinch as wheel notches. It arrives as small per-event scale changes, which add up in log
/// space (so a spread and the same pinch back cancel out) and go out as whole notches.
#[derive(Debug, Default)]
pub struct ZoomNotches {
    accumulated: f64,
}

impl ZoomNotches {
    /// A new pinch starts, or one ended: forget the remainder.
    pub fn reset(&mut self) {
        self.accumulated = 0.0;
    }

    /// The notches for a scale change since the last event (positive spreads, zooming in).
    pub fn add(&mut self, delta: f64) -> i32 {
        if !delta.is_finite() {
            return 0;
        }
        self.accumulated += (1.0 + delta).max(0.05).ln();
        let notches = (self.accumulated / ZOOM_PER_NOTCH) as i32;
        self.accumulated -= f64::from(notches) * ZOOM_PER_NOTCH;
        notches.clamp(-MAX_NOTCHES_PER_EVENT, MAX_NOTCHES_PER_EVENT)
    }
}

/// The delay before a held key repeats and the time between repeats, in milliseconds, from the
/// system's keyboard delay (0 to 3) and speed (0 to 31) settings, as a real key repeats.
pub fn key_repeat_timing(delay_setting: u32, speed_setting: u32) -> (u64, u64) {
    let delay = (u64::from(delay_setting.min(3)) + 1) * 250;
    let per_second = 2.5 + f64::from(speed_setting.min(31)) * (27.5 / 31.0);
    (delay, ((1000.0 / per_second) as u64).max(15))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_points_map_into_the_target() {
        let target = TargetRect::new(-1920, 100, 1920, 1080);
        assert_eq!(target.point(0.0, 0.0), (-1920, 100));
        assert_eq!(target.point(0.5, 0.25), (-960, 370));
        assert_eq!(target.delta(0.5, -0.25), (960.0, -270.0));
        // The far edge is the last pixel inside, not the first one past it.
        assert_eq!(target.point(1.0, 1.0), (-1, 1179));
    }

    #[test]
    fn points_never_leave_the_target() {
        let target = TargetRect::new(-1920, 100, 1920, 1080);
        assert_eq!(target.point(3.0, -1.0), (-1, 100));
        assert_eq!(target.point(-0.001, 1.001), (-1920, 1179));
        assert_eq!(target.point(f32::INFINITY, f32::NEG_INFINITY), (-1, 100));
        assert_eq!(target.point(f32::MAX, f32::MIN), (-1, 100));
        // Not a number is refused before it gets here; if it did, it stays inside too.
        assert_eq!(target.point(f32::NAN, f32::NAN), (-1920, 100));
        // A rectangle without pixels is refused as well; here it is treated as one pixel.
        assert_eq!(TargetRect::new(7, 8, 0, -4).point(1.0, 1.0), (7, 8));
        assert!(!TargetRect::new(7, 8, 0, 5).is_valid());
        assert!(!TargetRect::new(7, 8, 5, -1).is_valid());
        assert!(TargetRect::new(7, 8, 1, 1).is_valid());
        let edge = TargetRect::new(i32::MAX - 10, 0, 100, 100);
        assert_eq!(edge.point(1.0, 0.0).0, i32::MAX);
    }

    #[test]
    fn desktop_points_are_confined_to_the_target() {
        let target = TargetRect::new(100, 200, 50, 20);
        assert_eq!(target.confine(120, 210), (120, 210));
        assert_eq!(target.confine(0, 0), (100, 200));
        assert_eq!(target.confine(500, 500), (149, 219));
        assert_eq!(target.confine(i32::MIN, i32::MAX), (100, 219));
    }

    #[test]
    fn absolute_moves_span_the_virtual_screen() {
        let screen = VirtualScreen {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };
        assert_eq!(screen.absolute(-1920, 0), (0, 0));
        assert_eq!(screen.absolute(1919, 1079), (65535, 65535));
        assert_eq!(screen.absolute(0, 540), (32776, 32797));
        let empty = VirtualScreen {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        assert_eq!(empty.absolute(0, 0), (0, 0));
    }

    #[test]
    fn pointer_devices_count_from_the_virtual_origin() {
        let screen = VirtualScreen {
            x: -1920,
            y: -200,
            width: 3840,
            height: 1280,
        };
        assert_eq!(screen.pointer(0, 0), (1920, 200));
        assert_eq!(screen.pointer(-1920, -200), (0, 0));
    }

    #[test]
    fn tilt_from_altitude_and_azimuth() {
        use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};
        assert_eq!(tilt_degrees(FRAC_PI_2, 0.0), (0, 0));
        assert_eq!(tilt_degrees(FRAC_PI_4, 0.0), (45, 0));
        assert_eq!(tilt_degrees(FRAC_PI_4, FRAC_PI_2), (0, 45));
        assert_eq!(tilt_degrees(FRAC_PI_4, PI), (-45, 0));
        assert_eq!(tilt_degrees(FRAC_PI_4, -FRAC_PI_2), (0, -45));
        // No altitude reported: upright.
        assert_eq!(tilt_degrees(0.0, 1.0), (0, 0));
        assert_eq!(tilt_degrees(f32::NAN, 1.0), (0, 0));
        let (x, y) = tilt_degrees(0.01, FRAC_PI_4);
        assert!((80..=90).contains(&x) && (80..=90).contains(&y));
    }

    #[test]
    fn pressure_has_a_floor_in_contact() {
        assert_eq!(pen_pressure(0.0, true), 1);
        assert_eq!(pen_pressure(0.0, false), 0);
        assert_eq!(pen_pressure(0.5, true), 512);
        assert_eq!(pen_pressure(4.0, true), 1024);
        assert_eq!(pen_pressure(-1.0, true), 1);
        assert_eq!(pen_pressure(f32::NAN, true), 1);
    }

    #[test]
    fn a_pinch_becomes_whole_notches_and_cancels_out() {
        let mut zoom = ZoomNotches::default();
        assert_eq!(zoom.add(0.1), 0);
        assert_eq!(zoom.add(0.1), 0);
        assert_eq!(zoom.add(0.1), 1);
        let mut zoom = ZoomNotches::default();
        assert_eq!(zoom.add(0.2), 0);
        assert_eq!(zoom.add(-1.0 / 6.0), 0);
        assert!(
            zoom.accumulated.abs() < 1e-9,
            "a spread and the pinch back cancel"
        );
        assert_eq!(zoom.add(-0.3), -1);
        // A jumpy frame is capped, and its remainder below a notch is kept.
        assert_eq!(zoom.add(50.0), 3);
        assert_eq!(zoom.add(-5.0), -3);
        assert_eq!(zoom.add(f64::NAN), 0);
        zoom.add(0.2);
        zoom.reset();
        assert_eq!(zoom.accumulated, 0.0);
    }

    #[test]
    fn key_repeat_follows_the_system_settings() {
        assert_eq!(key_repeat_timing(0, 31), (250, 33));
        assert_eq!(key_repeat_timing(3, 0), (1000, 400));
        assert_eq!(key_repeat_timing(1, 31).0, 500);
        assert_eq!(key_repeat_timing(99, 99), (1000, 33));
    }
}
