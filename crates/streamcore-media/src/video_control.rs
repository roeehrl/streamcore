//! Typed bodies of the lane-0 video kinds `0x12`–`0x15` (`frames.v4`,
//! protocol/BINARY.md "Lane-0 video kinds"). Integers are little endian; every body
//! has an exact length for its contents, and unknown flag bits are invalid.
//! Times are microseconds of the sending process's StreamCore monotonic clock.
use crate::media::MediaError;

pub const KIND_VIDEO_FEEDBACK: u8 = 0x12;
pub const KIND_VIDEO_NACK: u8 = 0x13;
pub const KIND_FRAME_TIMING: u8 = 0x14;
pub const KIND_LAYOUT_INFO: u8 = 0x15;

/// True for the four lane-0 video kinds, which a sender emits only under
/// `frames.v4`.
pub fn is_video_control_kind(kind: u8) -> bool {
    (KIND_VIDEO_FEEDBACK..=KIND_LAYOUT_INFO).contains(&kind)
}

/// Checks a lane-0 video body against its kind; other kinds are not checked.
pub fn validate_video_control(kind: u8, payload: &[u8]) -> Result<(), MediaError> {
    match kind {
        KIND_VIDEO_FEEDBACK => VideoFeedback::decode(payload).map(|_| ()),
        KIND_VIDEO_NACK => VideoNack::decode(payload).map(|_| ()),
        KIND_FRAME_TIMING => FrameTimingReport::decode(payload).map(|_| ()),
        KIND_LAYOUT_INFO => LayoutInfo::decode(payload).map(|_| ()),
        _ => Ok(()),
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], MediaError> {
        let (head, tail) = self.bytes.split_at_checked(n).ok_or(MediaError)?;
        self.bytes = tail;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], MediaError> {
        self.take(N)?.try_into().map_err(|_| MediaError)
    }
    fn u8(&mut self) -> Result<u8, MediaError> {
        Ok(self.array::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, MediaError> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, MediaError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn i32(&mut self) -> Result<i32, MediaError> {
        Ok(i32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, MediaError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn finish(self) -> Result<(), MediaError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(MediaError)
        }
    }
}

fn check(valid: bool) -> Result<(), MediaError> {
    if valid { Ok(()) } else { Err(MediaError) }
}

/// One frame's client-side times in a [`VideoFeedback`]: C1 (message complete),
/// C3 (decoded) and C5 (presented). Zero means the frame did not reach that stage
/// (yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePresentation {
    pub sequence: u64,
    pub receive_us: u64,
    pub decode_us: u64,
    pub present_us: u64,
}

/// Missing samples: bit `i` (least significant bit first in each byte) set means
/// sequence `base_sequence + i` is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LossBitmap {
    pub base_sequence: u64,
    pub bits: Vec<u8>,
}
impl LossBitmap {
    pub const MAX_BYTES: usize = 128;
}

/// `videoFeedback` (`0x12`, client → host, about every 100 ms; every 20 ms in
/// datagram mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFeedback {
    pub stream_id: u32,
    pub generation: u64,
    /// Highest sample sequence received complete in `generation`; zero for none.
    pub highest_complete_sequence: u64,
    /// Video payload bytes of this stream the client consumed since the stream
    /// started on this channel.
    pub consumed_bytes: u64,
    /// Decoder flushes since the stream started on this channel.
    pub flushes: u32,
    /// Decoded frames never presented since the stream started on this channel.
    pub present_drops: u32,
    pub loss: Option<LossBitmap>,
    /// A long-term reference token decoded on the current transport.
    pub ltr_ack: Option<u32>,
    /// Recent frames' times, at most [`VideoFeedback::MAX_FRAMES`].
    pub frames: Vec<FramePresentation>,
}
impl VideoFeedback {
    pub const MAX_FRAMES: usize = 32;
    const LOSS: u8 = 0x01;
    const LTR: u8 = 0x02;
    fn validate(&self) -> Result<(), MediaError> {
        check(
            self.generation != 0
                && self.ltr_ack != Some(0)
                && self.frames.len() <= Self::MAX_FRAMES
                && self.frames.iter().all(|f| f.sequence != 0)
                && self.loss.as_ref().is_none_or(|l| {
                    l.base_sequence != 0 && (1..=LossBitmap::MAX_BYTES).contains(&l.bits.len())
                }),
        )
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, MediaError> {
        let mut r = Reader { bytes };
        let stream_id = r.u32()?;
        let generation = r.u64()?;
        let highest_complete_sequence = r.u64()?;
        let consumed_bytes = r.u64()?;
        let flushes = r.u32()?;
        let present_drops = r.u32()?;
        let flags = r.u8()?;
        check(flags & !(Self::LOSS | Self::LTR) == 0)?;
        let loss = if flags & Self::LOSS != 0 {
            let base_sequence = r.u64()?;
            let length = usize::from(r.u8()?);
            Some(LossBitmap {
                base_sequence,
                bits: r.take(length)?.to_vec(),
            })
        } else {
            None
        };
        let ltr_ack = if flags & Self::LTR != 0 {
            Some(r.u32()?)
        } else {
            None
        };
        let count = usize::from(r.u8()?);
        check(count <= Self::MAX_FRAMES)?;
        let mut frames = Vec::with_capacity(count);
        for _ in 0..count {
            frames.push(FramePresentation {
                sequence: r.u64()?,
                receive_us: r.u64()?,
                decode_us: r.u64()?,
                present_us: r.u64()?,
            });
        }
        r.finish()?;
        let value = Self {
            stream_id,
            generation,
            highest_complete_sequence,
            consumed_bytes,
            flushes,
            present_drops,
            loss,
            ltr_ack,
            frames,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn encode(&self) -> Result<Vec<u8>, MediaError> {
        self.validate()?;
        let mut b = Vec::with_capacity(38 + 32 * self.frames.len());
        b.extend(self.stream_id.to_le_bytes());
        b.extend(self.generation.to_le_bytes());
        b.extend(self.highest_complete_sequence.to_le_bytes());
        b.extend(self.consumed_bytes.to_le_bytes());
        b.extend(self.flushes.to_le_bytes());
        b.extend(self.present_drops.to_le_bytes());
        let mut flags = 0;
        if self.loss.is_some() {
            flags |= Self::LOSS;
        }
        if self.ltr_ack.is_some() {
            flags |= Self::LTR;
        }
        b.push(flags);
        if let Some(loss) = &self.loss {
            b.extend(loss.base_sequence.to_le_bytes());
            b.push(loss.bits.len() as u8);
            b.extend(&loss.bits);
        }
        if let Some(token) = self.ltr_ack {
            b.extend(token.to_le_bytes());
        }
        b.push(self.frames.len() as u8);
        for f in &self.frames {
            for value in [f.sequence, f.receive_us, f.decode_us, f.present_us] {
                b.extend(value.to_le_bytes());
            }
        }
        Ok(b)
    }
}

/// `videoNack` (`0x13`, datagram mode only): fragments of one frame the client is
/// missing, by index in strictly increasing order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoNack {
    pub stream_id: u32,
    pub generation: u64,
    pub sequence: u64,
    pub fragments: Vec<u16>,
}
impl VideoNack {
    pub const MAX_FRAGMENTS: usize = 1024;
    fn validate(&self) -> Result<(), MediaError> {
        check(
            self.generation != 0
                && self.sequence != 0
                && (1..=Self::MAX_FRAGMENTS).contains(&self.fragments.len())
                && self.fragments.windows(2).all(|w| w[0] < w[1]),
        )
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, MediaError> {
        let mut r = Reader { bytes };
        let stream_id = r.u32()?;
        let generation = r.u64()?;
        let sequence = r.u64()?;
        let count = usize::from(r.u16()?);
        check(count <= Self::MAX_FRAGMENTS)?;
        let fragments = (0..count).map(|_| r.u16()).collect::<Result<_, _>>()?;
        r.finish()?;
        let value = Self {
            stream_id,
            generation,
            sequence,
            fragments,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn encode(&self) -> Result<Vec<u8>, MediaError> {
        self.validate()?;
        let mut b = Vec::with_capacity(22 + 2 * self.fragments.len());
        b.extend(self.stream_id.to_le_bytes());
        b.extend(self.generation.to_le_bytes());
        b.extend(self.sequence.to_le_bytes());
        b.extend((self.fragments.len() as u16).to_le_bytes());
        for index in &self.fragments {
            b.extend(index.to_le_bytes());
        }
        Ok(b)
    }
}

/// `frameTiming` (`0x14`, host → client, sampled; every frame on USB): H5
/// (`VideoSink` admit) and H6 (last fragment written) of one sample, which cannot
/// be in the frame's own header, and whether the sink was backpressured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTimingReport {
    pub stream_id: u32,
    pub generation: u64,
    pub sequence: u64,
    pub h5_us: u64,
    pub h6_us: u64,
    pub backpressured: bool,
}
impl FrameTimingReport {
    pub const LENGTH: usize = 37;
    const BACKPRESSURED: u8 = 0x01;
    fn validate(&self) -> Result<(), MediaError> {
        check(self.generation != 0 && self.sequence != 0 && self.h6_us >= self.h5_us)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, MediaError> {
        let mut r = Reader { bytes };
        let stream_id = r.u32()?;
        let generation = r.u64()?;
        let sequence = r.u64()?;
        let h5_us = r.u64()?;
        let h6_us = r.u64()?;
        let flags = r.u8()?;
        check(flags & !Self::BACKPRESSURED == 0)?;
        r.finish()?;
        let value = Self {
            stream_id,
            generation,
            sequence,
            h5_us,
            h6_us,
            backpressured: flags & Self::BACKPRESSURED != 0,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn encode(&self) -> Result<[u8; Self::LENGTH], MediaError> {
        self.validate()?;
        let mut b = [0; Self::LENGTH];
        b[..4].copy_from_slice(&self.stream_id.to_le_bytes());
        b[4..12].copy_from_slice(&self.generation.to_le_bytes());
        b[12..20].copy_from_slice(&self.sequence.to_le_bytes());
        b[20..28].copy_from_slice(&self.h5_us.to_le_bytes());
        b[28..36].copy_from_slice(&self.h6_us.to_le_bytes());
        b[36] = if self.backpressured {
            Self::BACKPRESSURED
        } else {
            0
        };
        Ok(b)
    }
}

/// A rectangle in the capture source's pixel space; the origin may be negative
/// (a display left of or above the main one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
/// A rectangle in the encoded picture's pixel space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// `layoutInfo` (`0x15`, host → client): what the frames of one layout epoch show.
/// Frames name their epoch in the `0x06 content` TLV.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutInfo {
    pub stream_id: u32,
    pub epoch: u32,
    /// The captured region of the source.
    pub source: SourceRect,
    pub frame_width: u32,
    pub frame_height: u32,
    /// Where that region sits in the encoded picture; within the frame.
    pub content: ContentRect,
    /// Clockwise quarter turns (0–3) that display the content upright.
    pub rotation: u8,
}
impl LayoutInfo {
    pub const LENGTH: usize = 49;
    fn validate(&self) -> Result<(), MediaError> {
        let c = self.content;
        let s = self.source;
        let inside = |offset: u32, size: u32, limit: u32| {
            offset.checked_add(size).is_some_and(|end| end <= limit)
        };
        check(
            s.width != 0
                && s.height != 0
                && i64::from(s.x) + i64::from(s.width) <= i64::from(i32::MAX) + 1
                && i64::from(s.y) + i64::from(s.height) <= i64::from(i32::MAX) + 1
                && c.width != 0
                && c.height != 0
                && inside(c.x, c.width, self.frame_width)
                && inside(c.y, c.height, self.frame_height)
                && self.rotation <= 3,
        )
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, MediaError> {
        let mut r = Reader { bytes };
        let value = Self {
            stream_id: r.u32()?,
            epoch: r.u32()?,
            source: SourceRect {
                x: r.i32()?,
                y: r.i32()?,
                width: r.u32()?,
                height: r.u32()?,
            },
            frame_width: r.u32()?,
            frame_height: r.u32()?,
            content: ContentRect {
                x: r.u32()?,
                y: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
            },
            rotation: r.u8()?,
        };
        r.finish()?;
        value.validate()?;
        Ok(value)
    }
    pub fn encode(&self) -> Result<[u8; Self::LENGTH], MediaError> {
        self.validate()?;
        let mut b = Vec::with_capacity(Self::LENGTH);
        b.extend(self.stream_id.to_le_bytes());
        b.extend(self.epoch.to_le_bytes());
        b.extend(self.source.x.to_le_bytes());
        b.extend(self.source.y.to_le_bytes());
        for value in [
            self.source.width,
            self.source.height,
            self.frame_width,
            self.frame_height,
            self.content.x,
            self.content.y,
            self.content.width,
            self.content.height,
        ] {
            b.extend(value.to_le_bytes());
        }
        b.push(self.rotation);
        b.try_into().map_err(|_| MediaError)
    }
}
