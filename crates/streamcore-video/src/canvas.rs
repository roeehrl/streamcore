//! The combined canvas policy (3.9): layout validation, the encoded canvas size,
//! tile maths, re-observation and input mapping. Pure functions; the stream's
//! canvas lifecycle (all-or-nothing group start and stop, the first-image
//! deadline, re-observation, cadence) is in the stream executor.
//!
//! A layout names exact displays in one global coordinate space (macOS: points;
//! Windows: desktop pixels). Members never overlap; gaps between them are black
//! in the canvas and receive no input. The canvas keeps one uniform scale over
//! the union of the members, so mixed densities keep their relative sizes.

use crate::error::MediaError;
use crate::types::{CanvasTile, DisplayLayout, Rect, Rotation};

/// The limits every consumer applies before a canvas exists (2.5.7). A
/// consumer's own validator mirrors these values.
pub mod limits {
    /// Members of one canvas.
    pub const MIN_MEMBERS: usize = 2;
    pub const MAX_MEMBERS: usize = 8;
    /// Largest absolute origin coordinate, in global units.
    pub const MAX_COORDINATE: f64 = 100_000.0;
    /// Longest edge of one member, in global units.
    pub const MAX_MEMBER_EDGE: f64 = 16_384.0;
    /// Longest edge of the union of all members, in global units.
    pub const MAX_UNION_EDGE: f64 = 65_536.0;
    pub const MAX_SCALE: f64 = 8.0;
    /// The encoded canvas: longest edge and total pixels.
    pub const MAX_PIXEL_EDGE: u32 = 8192;
    pub const MAX_PIXELS: u64 = 7680 * 4320;
    /// The composite pool's minimum: the largest permit target (3) + 2 (3.2).
    pub const COMPOSITE_POOL: u8 = 5;
    /// Rust's leases per member at most: the latest image and one undelivered (3.2).
    pub const MEMBER_LEASES: usize = 2;
}

/// One display of a combined canvas: its identity and its global layout, as
/// observed when the arrangement was reviewed.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct CanvasDisplay {
    pub display_id: u32,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Pixels per global unit.
    pub scale: f64,
    pub rotation: Rotation,
}

impl CanvasDisplay {
    fn max_x(&self) -> f64 {
        self.x + self.width
    }
    fn max_y(&self) -> f64 {
        self.y + self.height
    }
}

/// The displays of one combined canvas. It names exact members; it never means
/// "whatever displays exist when the stream starts".
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct CanvasArrangement {
    pub members: Vec<CanvasDisplay>,
}

/// The encoded canvas size, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct CanvasSize {
    pub width: u32,
    pub height: u32,
}

/// Where a canvas point lands: the member (its index in the layout) and the
/// global point to deliver input at.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct CanvasPoint {
    pub member: u8,
    pub display_id: u32,
    pub x: f64,
    pub y: f64,
}

/// A point normalized to the canvas (0–1 per axis).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct NormalizedPoint {
    pub x: f64,
    pub y: f64,
}

/// The smallest rectangle in global units containing every member: (x, y,
/// width, height).
pub(crate) fn union(layout: &CanvasArrangement) -> Option<(f64, f64, f64, f64)> {
    let first = layout.members.first()?;
    let (mut min_x, mut min_y, mut max_x, mut max_y) =
        (first.x, first.y, first.max_x(), first.max_y());
    for m in &layout.members[1..] {
        min_x = min_x.min(m.x);
        min_y = min_y.min(m.y);
        max_x = max_x.max(m.max_x());
        max_y = max_y.max(m.max_y());
    }
    Some((min_x, min_y, max_x - min_x, max_y - min_y))
}

fn overlaps(a: &CanvasDisplay, b: &CanvasDisplay) -> bool {
    a.x < b.max_x() && b.x < a.max_x() && a.y < b.max_y() && b.y < a.max_y()
}

/// Why `layout` cannot form a canvas, or `None` when it can.
pub fn problem(layout: &CanvasArrangement) -> Option<&'static str> {
    use limits::*;
    let count = layout.members.len();
    if !(MIN_MEMBERS..=MAX_MEMBERS).contains(&count) {
        return Some("a canvas needs 2 to 8 displays");
    }
    for (index, m) in layout.members.iter().enumerate() {
        if layout.members[..index]
            .iter()
            .any(|other| other.display_id == m.display_id)
        {
            return Some("a display appears twice");
        }
        let finite = [m.x, m.y, m.width, m.height, m.scale]
            .iter()
            .all(|v| v.is_finite());
        if !finite
            || m.x.abs() > MAX_COORDINATE
            || m.y.abs() > MAX_COORDINATE
            || m.width <= 0.0
            || m.height <= 0.0
            || m.width > MAX_MEMBER_EDGE
            || m.height > MAX_MEMBER_EDGE
            || m.scale <= 0.0
            || m.scale > MAX_SCALE
        {
            return Some("a display has invalid bounds or scale");
        }
    }
    for (index, a) in layout.members.iter().enumerate() {
        if layout.members[index + 1..].iter().any(|b| overlaps(a, b)) {
            return Some("displays overlap");
        }
    }
    let (_, _, width, height) = union(layout)?;
    if !width.is_finite()
        || !height.is_finite()
        || width > MAX_UNION_EDGE
        || height > MAX_UNION_EDGE
    {
        return Some("the arrangement is too large");
    }
    None
}

/// Rust is the single validator: a layout that cannot form a canvas is
/// `InvalidSettings`.
#[uniffi::export]
pub fn canvas_validate(layout: CanvasArrangement) -> Result<(), MediaError> {
    match problem(&layout) {
        None => Ok(()),
        Some(_) => Err(MediaError::InvalidSettings),
    }
}

/// The encoded canvas size for a longest edge of at most `max_dimension`: one
/// uniform scale over the union, the densest member's scale as the ceiling,
/// then the edge limit and the pixel budget; even sizes. `None` when the layout
/// is invalid or the canvas would be smaller than 2 × 2.
#[uniffi::export]
pub fn canvas_size(layout: CanvasArrangement, max_dimension: u32) -> Option<CanvasSize> {
    size(&layout, max_dimension)
}

pub(crate) fn size(layout: &CanvasArrangement, max_dimension: u32) -> Option<CanvasSize> {
    if problem(layout).is_some() || max_dimension < 2 {
        return None;
    }
    let (_, _, uw, uh) = union(layout)?;
    let mut factor = layout
        .members
        .iter()
        .map(|m| m.scale)
        .fold(f64::MIN, f64::max)
        .max(f64::MIN_POSITIVE);
    factor = factor.min(f64::from(max_dimension.min(limits::MAX_PIXEL_EDGE)) / uw.max(uh));
    factor = factor.min((limits::MAX_PIXELS as f64 / (uw * uh)).sqrt());
    let even = |v: f64| {
        let v = v.floor();
        if !(0.0..=f64::from(u32::MAX)).contains(&v) {
            return 0;
        }
        (v as u32) & !1
    };
    let (width, height) = (even(uw * factor), even(uh * factor));
    (width >= 2 && height >= 2).then_some(CanvasSize { width, height })
}

/// Each member's whole-pixel tile in a `width` × `height` canvas, in layout
/// order (`CanvasTile::member` is the member's index). Edges are rounded
/// independently, so adjacent displays share an edge exactly.
#[uniffi::export]
pub fn canvas_tiles(layout: CanvasArrangement, width: u32, height: u32) -> Vec<CanvasTile> {
    tiles(&layout, width, height)
}

pub(crate) fn tiles(layout: &CanvasArrangement, width: u32, height: u32) -> Vec<CanvasTile> {
    let Some((ux, uy, uw, uh)) = union(layout) else {
        return Vec::new();
    };
    let sx = f64::from(width) / uw;
    let sy = f64::from(height) / uh;
    let edge = |value: f64, scale: f64, limit: u32| -> u32 {
        let v = (value * scale).round();
        if v.is_nan() || v <= 0.0 {
            0
        } else if v >= f64::from(limit) {
            limit
        } else {
            v as u32
        }
    };
    layout
        .members
        .iter()
        .enumerate()
        .map(|(index, m)| {
            let x0 = edge(m.x - ux, sx, width);
            let x1 = edge(m.max_x() - ux, sx, width);
            let y0 = edge(m.y - uy, sy, height);
            let y1 = edge(m.max_y() - uy, sy, height);
            CanvasTile {
                member: index as u8,
                rect: Rect {
                    x: x0,
                    y: y0,
                    width: x1.saturating_sub(x0),
                    height: y1.saturating_sub(y0),
                },
            }
        })
        .collect()
}

/// Where a point normalized to the canvas lands: the member under it and the
/// global point to deliver there, kept one native pixel inside the member's far
/// edges. `None` in a gap or outside the canvas: input never lands on a display
/// it was not aimed at. A shared edge belongs to the display that starts there.
#[uniffi::export]
pub fn canvas_input_target(layout: CanvasArrangement, x: f64, y: f64) -> Option<CanvasPoint> {
    input_target(&layout, x, y)
}

pub(crate) fn input_target(layout: &CanvasArrangement, x: f64, y: f64) -> Option<CanvasPoint> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let (ux, uy, uw, uh) = union(layout)?;
    let (gx, gy) = (ux + x * uw, uy + y * uh);
    let half_open = |m: &CanvasDisplay| m.x <= gx && gx < m.max_x() && m.y <= gy && gy < m.max_y();
    let closed = |m: &CanvasDisplay| m.x <= gx && gx <= m.max_x() && m.y <= gy && gy <= m.max_y();
    let (index, member) = layout
        .members
        .iter()
        .enumerate()
        .find(|(_, m)| half_open(m))
        .or_else(|| layout.members.iter().enumerate().find(|(_, m)| closed(m)))?;
    let inset = 1.0 / member.scale;
    Some(CanvasPoint {
        member: index as u8,
        display_id: member.display_id,
        x: gx.min(member.max_x() - inset),
        y: gy.min(member.max_y() - inset),
    })
}

/// The nearest canvas point that lies on a member, for a cursor the client
/// moves itself, so it never rests in a gap.
#[uniffi::export]
pub fn canvas_nearest_member_point(layout: CanvasArrangement, x: f64, y: f64) -> NormalizedPoint {
    let Some((ux, uy, uw, uh)) = union(&layout) else {
        return NormalizedPoint { x, y };
    };
    let mut best = NormalizedPoint { x, y };
    let mut distance = f64::INFINITY;
    for m in &layout.members {
        let (fx, fy) = ((m.x - ux) / uw, (m.y - uy) / uh);
        let (fw, fh) = (m.width / uw, m.height / uh);
        let cx = x.max(fx).min(fx + fw);
        let cy = y.max(fy).min(fy + fh);
        let d = (cx - x).hypot(cy - y);
        if d < distance {
            distance = d;
            best = NormalizedPoint { x: cx, y: cy };
        }
    }
    best
}

/// True while the observed display is exactly where and how it was reviewed:
/// the same origin, size (pixels over scale) and scale. A rotation that changes
/// the size is caught here; a backend reports a mirrored display, or one whose
/// rotation changed since its capture opened, as gone (`observe_layout` →
/// `None`).
pub(crate) fn observed_matches(member: &CanvasDisplay, observed: &DisplayLayout) -> bool {
    if observed.display_id != member.display_id
        || !observed.scale.is_finite()
        || observed.scale <= 0.0
    {
        return false;
    }
    let width = f64::from(observed.pixel_width) / observed.scale;
    let height = f64::from(observed.pixel_height) / observed.scale;
    const EPSILON: f64 = 0.01;
    (f64::from(observed.origin_x) - member.x).abs() < EPSILON
        && (f64::from(observed.origin_y) - member.y).abs() < EPSILON
        && (width - member.width).abs() < EPSILON
        && (height - member.height).abs() < EPSILON
        && (observed.scale - member.scale).abs() < 0.001
}

/// The canvas size a stream may encode at: within the limits, even, at least
/// 2 × 2, with the union's aspect ratio (within 2 %, the rounding of a small
/// canvas), and every tile at least 2 × 2.
pub(crate) fn valid_canvas_size(layout: &CanvasArrangement, width: u32, height: u32) -> bool {
    let Some((_, _, uw, uh)) = union(layout) else {
        return false;
    };
    let edges = 2..=limits::MAX_PIXEL_EDGE;
    if !edges.contains(&width)
        || !edges.contains(&height)
        || u64::from(width) * u64::from(height) > limits::MAX_PIXELS
    {
        return false;
    }
    let aspect = f64::from(width) / f64::from(height);
    let expected = uw / uh;
    if ((aspect - expected) / expected).abs() > 0.02 {
        return false;
    }
    tiles(layout, width, height)
        .iter()
        .all(|t| t.rect.width >= 2 && t.rect.height >= 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: u32, x: f64, y: f64, w: f64, h: f64, scale: f64) -> CanvasDisplay {
        CanvasDisplay {
            display_id: id,
            x,
            y,
            width: w,
            height: h,
            scale,
            rotation: Rotation::None,
        }
    }

    #[test]
    fn size_and_tiles_share_edges() {
        let layout = CanvasArrangement {
            members: vec![
                member(1, 0.0, 0.0, 1512.0, 982.0, 2.0),
                member(2, 1512.0, -200.0, 2560.0, 1440.0, 1.0),
            ],
        };
        let size = size(&layout, 4096).unwrap();
        assert!(
            size.width <= 4096 && size.width.is_multiple_of(2) && size.height.is_multiple_of(2)
        );
        let t = tiles(&layout, size.width, size.height);
        assert_eq!(t[0].rect.x + t[0].rect.width, t[1].rect.x);
    }
}
