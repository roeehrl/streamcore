//! Portable input, audio, terminal, file and outer-frame codecs for frames.v3
//! and frames.v4.
//! UUIDs use RFC/network byte order; integers in payloads use little endian.
use crate::media::{MAX_FRAME_LENGTH, MediaError};

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn take<const N: usize>(&mut self) -> Result<[u8; N], MediaError> {
        let end = self.offset.checked_add(N).ok_or(MediaError)?;
        let value = self.bytes.get(self.offset..end).ok_or(MediaError)?;
        self.offset = end;
        value.try_into().map_err(|_| MediaError)
    }
    fn u8(&mut self) -> Result<u8, MediaError> {
        Ok(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, MediaError> {
        Ok(u16::from_le_bytes(self.take()?))
    }
    fn u32(&mut self) -> Result<u32, MediaError> {
        Ok(u32::from_le_bytes(self.take()?))
    }
    fn f32(&mut self) -> Result<f32, MediaError> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn boolean(&mut self) -> Result<bool, MediaError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(MediaError),
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InputEvent {
    Pointer {
        stream: u32,
        phase: u8,
        button: u8,
        x: f32,
        y: f32,
        clicks: u8,
        pressure: f32,
        modifiers: u32,
    },
    Scroll {
        stream: u32,
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
        precise: bool,
        momentum: u8,
        modifiers: u32,
    },
    Magnify {
        stream: u32,
        x: f32,
        y: f32,
        scale: f32,
        phase: u8,
    },
    Key {
        stream: u32,
        code: u16,
        down: bool,
        modifiers: u32,
    },
}
impl InputEvent {
    pub fn decode(bytes: &[u8]) -> Result<Self, MediaError> {
        let mut r = Reader::new(bytes);
        let event = match r.u8()? {
            1 => Self::Pointer {
                stream: r.u32()?,
                phase: r.u8()?,
                button: r.u8()?,
                x: r.f32()?,
                y: r.f32()?,
                clicks: r.u8()?,
                pressure: r.f32()?,
                modifiers: r.u32()?,
            },
            2 => Self::Scroll {
                stream: r.u32()?,
                x: r.f32()?,
                y: r.f32()?,
                dx: r.f32()?,
                dy: r.f32()?,
                precise: r.boolean()?,
                momentum: r.u8()?,
                modifiers: r.u32()?,
            },
            3 => Self::Magnify {
                stream: r.u32()?,
                x: r.f32()?,
                y: r.f32()?,
                scale: r.f32()?,
                phase: r.u8()?,
            },
            4 => Self::Key {
                stream: r.u32()?,
                code: r.u16()?,
                down: r.boolean()?,
                modifiers: r.u32()?,
            },
            _ => return Err(MediaError),
        };
        if r.offset != bytes.len() {
            return Err(MediaError);
        }
        event.validate()?;
        Ok(event)
    }
    fn validate(&self) -> Result<(), MediaError> {
        let unit = |v: f32| v.is_finite() && (0.0..=1.0).contains(&v);
        let flags = |v: u32| v & !63 == 0;
        let valid = match *self {
            Self::Pointer {
                phase,
                button,
                x,
                y,
                pressure,
                modifiers,
                ..
            } => {
                phase <= 3
                    && button <= 2
                    && unit(x)
                    && unit(y)
                    && unit(pressure)
                    && flags(modifiers)
            }
            Self::Scroll {
                x,
                y,
                dx,
                dy,
                momentum,
                modifiers,
                ..
            } => {
                unit(x)
                    && unit(y)
                    && dx.is_finite()
                    && dy.is_finite()
                    && momentum <= 3
                    && flags(modifiers)
            }
            Self::Magnify {
                x, y, scale, phase, ..
            } => unit(x) && unit(y) && scale.is_finite() && phase <= 2,
            Self::Key { modifiers, .. } => flags(modifiers),
        };
        if valid { Ok(()) } else { Err(MediaError) }
    }
    pub fn encode(&self) -> Result<Vec<u8>, MediaError> {
        self.validate()?;
        let mut b = Vec::with_capacity(27);
        match *self {
            Self::Pointer {
                stream,
                phase,
                button,
                x,
                y,
                clicks,
                pressure,
                modifiers,
            } => {
                b.push(1);
                b.extend(stream.to_le_bytes());
                b.extend([phase, button]);
                b.extend(x.to_le_bytes());
                b.extend(y.to_le_bytes());
                b.push(clicks);
                b.extend(pressure.to_le_bytes());
                b.extend(modifiers.to_le_bytes());
            }
            Self::Scroll {
                stream,
                x,
                y,
                dx,
                dy,
                precise,
                momentum,
                modifiers,
            } => {
                b.push(2);
                b.extend(stream.to_le_bytes());
                for value in [x, y, dx, dy] {
                    b.extend(value.to_le_bytes());
                }
                b.extend([u8::from(precise), momentum]);
                b.extend(modifiers.to_le_bytes());
            }
            Self::Magnify {
                stream,
                x,
                y,
                scale,
                phase,
            } => {
                b.push(3);
                b.extend(stream.to_le_bytes());
                for value in [x, y, scale] {
                    b.extend(value.to_le_bytes());
                }
                b.push(phase);
            }
            Self::Key {
                stream,
                code,
                down,
                modifiers,
            } => {
                b.push(4);
                b.extend(stream.to_le_bytes());
                b.extend(code.to_le_bytes());
                b.push(u8::from(down));
                b.extend(modifiers.to_le_bytes());
            }
        }
        Ok(b)
    }
}

fn bounded_payload(bytes: &[u8], header: usize) -> Result<(), MediaError> {
    if bytes.len() <= header || bytes.len() > MAX_FRAME_LENGTH {
        Err(MediaError)
    } else {
        Ok(())
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct AudioPacket<'a> {
    pub pts_micros: u64,
    pub data: &'a [u8],
}
impl<'a> AudioPacket<'a> {
    pub fn decode(bytes: &'a [u8]) -> Result<Self, MediaError> {
        bounded_payload(bytes, 8)?;
        Ok(Self {
            pts_micros: u64::from_le_bytes(bytes[..8].try_into().map_err(|_| MediaError)?),
            data: &bytes[8..],
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>, MediaError> {
        if self.data.is_empty() || self.data.len() > MAX_FRAME_LENGTH - 8 {
            return Err(MediaError);
        }
        let mut out = Vec::with_capacity(8 + self.data.len());
        out.extend(self.pts_micros.to_le_bytes());
        out.extend(self.data);
        Ok(out)
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct TerminalPacket<'a> {
    pub agent_id: [u8; 16],
    pub subscription_id: [u8; 16],
    pub kind: u8,
    pub sequence: u64,
    pub cols: u16,
    pub rows: u16,
    pub data: &'a [u8],
}
impl<'a> TerminalPacket<'a> {
    pub fn decode(bytes: &'a [u8]) -> Result<Self, MediaError> {
        if bytes.len() < 45 || bytes.len() > 45 + 4 * 1024 * 1024 {
            return Err(MediaError);
        }
        let value = Self {
            agent_id: bytes[..16].try_into().map_err(|_| MediaError)?,
            subscription_id: bytes[16..32].try_into().map_err(|_| MediaError)?,
            kind: bytes[32],
            sequence: u64::from_le_bytes(bytes[33..41].try_into().map_err(|_| MediaError)?),
            cols: u16::from_le_bytes(bytes[41..43].try_into().map_err(|_| MediaError)?),
            rows: u16::from_le_bytes(bytes[43..45].try_into().map_err(|_| MediaError)?),
            data: &bytes[45..],
        };
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<(), MediaError> {
        let geometry = match self.kind {
            0 | 1 => {
                (2..=4096).contains(&self.cols)
                    && (1..=4096).contains(&self.rows)
                    && u32::from(self.cols) * u32::from(self.rows) <= 262144
            }
            2 => self.cols == 0 && self.rows == 0,
            _ => false,
        };
        if !geometry
            || (self.data.is_empty() && self.kind != 2)
            || self.data.len() > 4 * 1024 * 1024
        {
            return Err(MediaError);
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>, MediaError> {
        self.validate()?;
        let mut out = Vec::with_capacity(45 + self.data.len());
        out.extend(self.agent_id);
        out.extend(self.subscription_id);
        out.push(self.kind);
        out.extend(self.sequence.to_le_bytes());
        out.extend(self.cols.to_le_bytes());
        out.extend(self.rows.to_le_bytes());
        out.extend(self.data);
        Ok(out)
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct FilePacket<'a> {
    pub transfer_id: [u8; 16],
    pub offset: u64,
    pub data: &'a [u8],
}
impl<'a> FilePacket<'a> {
    pub fn decode(bytes: &'a [u8]) -> Result<Self, MediaError> {
        bounded_payload(bytes, 24)?;
        let offset = u64::from_le_bytes(bytes[16..24].try_into().map_err(|_| MediaError)?);
        let data = &bytes[24..];
        offset.checked_add(data.len() as u64).ok_or(MediaError)?;
        Ok(Self {
            transfer_id: bytes[..16].try_into().map_err(|_| MediaError)?,
            offset,
            data,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>, MediaError> {
        if self.data.is_empty() || self.data.len() > MAX_FRAME_LENGTH - 24 {
            return Err(MediaError);
        }
        self.offset
            .checked_add(self.data.len() as u64)
            .ok_or(MediaError)?;
        let mut out = Vec::with_capacity(24 + self.data.len());
        out.extend(self.transfer_id);
        out.extend(self.offset.to_le_bytes());
        out.extend(self.data);
        Ok(out)
    }
}

/// Typed ping body (outer kind `0x02`) under `clock.v1`: the sender's nonce and its
/// monotonic send time in microseconds. Exactly 16 bytes, little endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockPing {
    pub nonce: u64,
    pub send_us: u64,
}
impl ClockPing {
    pub const LENGTH: usize = 16;
    pub fn decode(bytes: &[u8]) -> Result<Self, MediaError> {
        if bytes.len() != Self::LENGTH {
            return Err(MediaError);
        }
        let mut r = Reader::new(bytes);
        Ok(Self {
            nonce: u64::from_le_bytes(r.take()?),
            send_us: u64::from_le_bytes(r.take()?),
        })
    }
    pub fn encode(&self) -> [u8; Self::LENGTH] {
        let mut out = [0; Self::LENGTH];
        out[..8].copy_from_slice(&self.nonce.to_le_bytes());
        out[8..].copy_from_slice(&self.send_us.to_le_bytes());
        out
    }
}

/// Typed pong body (outer kind `0x03`) under `clock.v1`: the ping's nonce, then the
/// answering peer's monotonic receive and send times in microseconds. Exactly 24
/// bytes, little endian; the send time is never earlier than the receive time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockPong {
    pub nonce: u64,
    pub receive_us: u64,
    pub send_us: u64,
}
impl ClockPong {
    pub const LENGTH: usize = 24;
    pub fn decode(bytes: &[u8]) -> Result<Self, MediaError> {
        if bytes.len() != Self::LENGTH {
            return Err(MediaError);
        }
        let mut r = Reader::new(bytes);
        let pong = Self {
            nonce: u64::from_le_bytes(r.take()?),
            receive_us: u64::from_le_bytes(r.take()?),
            send_us: u64::from_le_bytes(r.take()?),
        };
        if pong.send_us < pong.receive_us {
            return Err(MediaError);
        }
        Ok(pong)
    }
    pub fn encode(&self) -> Result<[u8; Self::LENGTH], MediaError> {
        if self.send_us < self.receive_us {
            return Err(MediaError);
        }
        let mut out = [0; Self::LENGTH];
        out[..8].copy_from_slice(&self.nonce.to_le_bytes());
        out[8..16].copy_from_slice(&self.receive_us.to_le_bytes());
        out[16..].copy_from_slice(&self.send_us.to_le_bytes());
        Ok(out)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub kind: u8,
    pub payload: &'a [u8],
}
impl<'a> Frame<'a> {
    /// Parse one complete frame, preserving the unconsumed tail for the caller.
    /// None means the caller needs more bytes. Invalid headers fail immediately.
    pub fn read(bytes: &'a [u8]) -> Result<Option<(Self, usize)>, MediaError> {
        if bytes.len() < 5 {
            return Ok(None);
        }
        if !Self::valid_kind(bytes[0]) {
            return Err(MediaError);
        }
        let length = u32::from_be_bytes(bytes[1..5].try_into().map_err(|_| MediaError)?) as usize;
        if length > MAX_FRAME_LENGTH {
            return Err(MediaError);
        }
        let end = 5 + length;
        if bytes.len() < end {
            return Ok(None);
        }
        Ok(Some((
            Self {
                kind: bytes[0],
                payload: &bytes[5..end],
            },
            end,
        )))
    }
    pub fn encode(&self) -> Result<Vec<u8>, MediaError> {
        if !Self::valid_kind(self.kind) || self.payload.len() > MAX_FRAME_LENGTH {
            return Err(MediaError);
        }
        let mut out = Vec::with_capacity(5 + self.payload.len());
        out.push(self.kind);
        out.extend((self.payload.len() as u32).to_be_bytes());
        out.extend(self.payload);
        Ok(out)
    }
    /// The envelope's kinds. The lane-0 video kinds `0x12`–`0x15` are accepted
    /// unconditionally (this check cannot see negotiated capabilities); senders emit
    /// them only under `frames.v4`.
    pub fn valid_kind(kind: u8) -> bool {
        matches!(kind, 1 | 2 | 3 | 0x10..=0x15 | 0x20 | 0x30 | 0x40)
    }
}
