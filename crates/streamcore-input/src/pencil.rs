//! `PencilPacket` (0x20): a batch of pencil samples, little endian.
//!
//! Packet: u8 count, then `count` samples of 39 bytes each:
//! u8 phase, u8 kind, u8 button, f32 x, f32 y, f32 force, f32 altitude,
//! f32 azimuth, f32 roll, f32 hoverZ, f64 timestamp.

use serde::{Deserialize, Serialize};

use crate::{Result, StreamInputError};

/// Size of one encoded sample.
pub const PENCIL_SAMPLE_SIZE: usize = 3 + 4 * 7 + 8;
/// Most samples in one packet.
pub const PENCIL_PACKET_MAX_SAMPLES: usize = 255;

/// Sample phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum PencilPhase {
    /// Moving above the surface (or a plain cursor move). 0
    Hover,
    /// 1
    Down,
    /// 2
    Moved,
    /// 3
    Up,
    /// Left proximity. 4
    HoverExit,
    /// `penV2`: the contact was a palm or was cancelled by the system. 5
    Cancel,
}

impl PencilPhase {
    fn byte(self) -> u8 {
        match self {
            Self::Hover => 0,
            Self::Down => 1,
            Self::Moved => 2,
            Self::Up => 3,
            Self::HoverExit => 4,
            Self::Cancel => 5,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Hover,
            1 => Self::Down,
            2 => Self::Moved,
            3 => Self::Up,
            4 => Self::HoverExit,
            5 => Self::Cancel,
            _ => return None,
        })
    }
}

/// The pointer a sample came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum PointerKind {
    Pen,
    Eraser,
    Finger,
}

/// Which mouse button a stroke represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum StrokeButton {
    Left,
    Right,
    Middle,
}

/// One pencil sample. Coordinates are normalized 0...1 within the streamed content.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct PencilSample {
    pub phase: PencilPhase,
    pub kind: PointerKind,
    pub button: StrokeButton,
    pub x: f32,
    pub y: f32,
    /// 0...1, normalized to the maximum possible force, before the pressure curve.
    pub force: f32,
    /// Radians; pi/2 = perpendicular.
    pub altitude: f32,
    /// Radians in content space.
    pub azimuth: f32,
    /// Radians (barrel roll).
    pub roll: f32,
    /// 0...1 height above the screen while hovering.
    pub hover_z: f32,
    /// Seconds.
    pub timestamp: f64,
}

/// Encodes a packet. Fails above 255 samples, on non-finite values and on
/// `cancel` unless the host advertised `penV2`.
#[uniffi::export]
pub fn encode_pencil_packet(samples: Vec<PencilSample>, pen_v2: bool) -> Result<Vec<u8>> {
    if samples.len() > PENCIL_PACKET_MAX_SAMPLES {
        return Err(StreamInputError::binary(format!(
            "{} samples; a packet holds at most 255",
            samples.len()
        )));
    }
    let mut out = Vec::with_capacity(1 + samples.len() * PENCIL_SAMPLE_SIZE);
    out.push(samples.len() as u8);
    for s in &samples {
        check_sample(s, pen_v2)?;
        out.extend([s.phase.byte(), kind_byte(s.kind), button_byte(s.button)]);
        for f in [s.x, s.y, s.force, s.altitude, s.azimuth, s.roll, s.hover_z] {
            out.extend(f.to_le_bytes());
        }
        out.extend(s.timestamp.to_le_bytes());
    }
    Ok(out)
}

/// Decodes a packet strictly: exact length, known enum values (`cancel` only
/// with `penV2`) and finite numbers.
#[uniffi::export]
pub fn decode_pencil_packet(bytes: Vec<u8>, pen_v2: bool) -> Result<Vec<PencilSample>> {
    let Some((&count, body)) = bytes.split_first() else {
        return Err(StreamInputError::binary("empty pencil packet"));
    };
    let expected = usize::from(count) * PENCIL_SAMPLE_SIZE;
    if body.len() != expected {
        return Err(StreamInputError::binary(format!(
            "pencil packet of {count} samples has {} bytes after the count, not {expected}",
            body.len()
        )));
    }
    let mut samples = Vec::with_capacity(usize::from(count));
    for chunk in body.as_chunks::<PENCIL_SAMPLE_SIZE>().0 {
        let f = |i: usize| f32::from_le_bytes([chunk[i], chunk[i + 1], chunk[i + 2], chunk[i + 3]]);
        let mut ts = [0u8; 8];
        ts.copy_from_slice(&chunk[31..39]);
        let sample = PencilSample {
            phase: PencilPhase::from_byte(chunk[0])
                .ok_or_else(|| StreamInputError::binary(format!("unknown phase {}", chunk[0])))?,
            kind: match chunk[1] {
                0 => PointerKind::Pen,
                1 => PointerKind::Eraser,
                2 => PointerKind::Finger,
                k => return Err(StreamInputError::binary(format!("unknown kind {k}"))),
            },
            button: match chunk[2] {
                0 => StrokeButton::Left,
                1 => StrokeButton::Right,
                2 => StrokeButton::Middle,
                b => return Err(StreamInputError::binary(format!("unknown button {b}"))),
            },
            x: f(3),
            y: f(7),
            force: f(11),
            altitude: f(15),
            azimuth: f(19),
            roll: f(23),
            hover_z: f(27),
            timestamp: f64::from_le_bytes(ts),
        };
        check_sample(&sample, pen_v2)?;
        samples.push(sample);
    }
    Ok(samples)
}

fn kind_byte(kind: PointerKind) -> u8 {
    match kind {
        PointerKind::Pen => 0,
        PointerKind::Eraser => 1,
        PointerKind::Finger => 2,
    }
}

fn button_byte(button: StrokeButton) -> u8 {
    match button {
        StrokeButton::Left => 0,
        StrokeButton::Right => 1,
        StrokeButton::Middle => 2,
    }
}

fn check_sample(s: &PencilSample, pen_v2: bool) -> Result<()> {
    if s.phase == PencilPhase::Cancel && !pen_v2 {
        return Err(StreamInputError::binary(
            "phase cancel needs penV2; send up to older hosts",
        ));
    }
    let floats = [s.x, s.y, s.force, s.altitude, s.azimuth, s.roll, s.hover_z];
    if floats.iter().any(|f| !f.is_finite()) || !s.timestamp.is_finite() {
        return Err(StreamInputError::binary(
            "pencil sample has a non-finite value",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(phase: PencilPhase) -> PencilSample {
        PencilSample {
            phase,
            kind: PointerKind::Eraser,
            button: StrokeButton::Right,
            x: 0.25,
            y: 0.5,
            force: 1.0,
            altitude: 1.5,
            azimuth: -0.5,
            roll: 0.0,
            hover_z: 0.0,
            timestamp: 12.5,
        }
    }

    #[test]
    fn layout_is_39_bytes() {
        assert_eq!(PENCIL_SAMPLE_SIZE, 39);
        let bytes = encode_pencil_packet(vec![sample(PencilPhase::Down)], false).unwrap();
        assert_eq!(bytes.len(), 40);
        assert_eq!(&bytes[..4], &[1, 1, 1, 1]);
        assert_eq!(&bytes[4..8], &0.25f32.to_le_bytes());
        assert_eq!(&bytes[32..40], &12.5f64.to_le_bytes());
        assert_eq!(
            decode_pencil_packet(bytes, false).unwrap(),
            vec![sample(PencilPhase::Down)]
        );
    }

    #[test]
    fn cancel_needs_pen_v2() {
        assert!(encode_pencil_packet(vec![sample(PencilPhase::Cancel)], false).is_err());
        let bytes = encode_pencil_packet(vec![sample(PencilPhase::Cancel)], true).unwrap();
        assert_eq!(bytes[1], 5);
        assert!(decode_pencil_packet(bytes.clone(), false).is_err());
        assert!(decode_pencil_packet(bytes, true).is_ok());
    }

    #[test]
    fn rejects_malformed() {
        let good = encode_pencil_packet(vec![sample(PencilPhase::Moved)], false).unwrap();
        assert!(decode_pencil_packet(vec![], false).is_err());
        assert!(decode_pencil_packet(good[..39].to_vec(), false).is_err());
        let mut long = good.clone();
        long.push(0);
        assert!(decode_pencil_packet(long, false).is_err());
        for (index, value) in [(1, 6), (2, 3), (3, 3)] {
            let mut bad = good.clone();
            bad[index] = value;
            assert!(decode_pencil_packet(bad, true).is_err(), "{index}");
        }
        let mut nan = good;
        nan[4..8].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(decode_pencil_packet(nan, false).is_err());
        assert!(encode_pencil_packet(vec![sample(PencilPhase::Up); 256], false).is_err());
        assert_eq!(decode_pencil_packet(vec![0], false).unwrap(), vec![]);
    }
}
