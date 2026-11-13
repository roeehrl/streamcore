//! `frames.v3` media headers and the `frames.v4` extension block. Pixels never reach Rust; encoded bytes cross only
//! through the send and receive calls. UniFFI carries only stream metadata.
pub const FRAME_CAPABILITY: &str = "frames.v3";
pub const MAX_FRAME_LENGTH: usize = 32 * 1024 * 1024;
pub const VIDEO_HEADER_LENGTH: usize = 29;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid binary media packet")]
pub struct MediaError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct VideoMetadata {
    pub stream_id: u32,
    pub generation: u64,
    pub sequence: u64,
    pub flags: u8,
    pub pts_micros: u64,
}
impl VideoMetadata {
    pub fn validate(self) -> Result<Self, MediaError> {
        if self.generation == 0
            || !matches!(self.flags, 0..=2)
            || (self.flags == 2 && (self.sequence != 0 || self.pts_micros != 0))
            || (self.flags != 2 && self.sequence == 0)
        {
            return Err(MediaError);
        }
        Ok(self)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct VideoPacket<'a> {
    pub metadata: VideoMetadata,
    pub data: &'a [u8],
}
impl<'a> VideoPacket<'a> {
    pub fn decode(bytes: &'a [u8]) -> Result<Self, MediaError> {
        if bytes.len() <= VIDEO_HEADER_LENGTH || bytes.len() > MAX_FRAME_LENGTH {
            return Err(MediaError);
        }
        let metadata = VideoMetadata {
            stream_id: u32::from_le_bytes(bytes[0..4].try_into().map_err(|_| MediaError)?),
            flags: bytes[4],
            generation: u64::from_le_bytes(bytes[5..13].try_into().map_err(|_| MediaError)?),
            sequence: u64::from_le_bytes(bytes[13..21].try_into().map_err(|_| MediaError)?),
            pts_micros: u64::from_le_bytes(bytes[21..29].try_into().map_err(|_| MediaError)?),
        }
        .validate()?;
        Ok(Self {
            metadata,
            data: &bytes[VIDEO_HEADER_LENGTH..],
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>, MediaError> {
        let m = self.metadata.validate()?;
        if self.data.is_empty() || self.data.len() > MAX_FRAME_LENGTH - VIDEO_HEADER_LENGTH {
            return Err(MediaError);
        }
        let mut bytes = Vec::with_capacity(VIDEO_HEADER_LENGTH + self.data.len());
        bytes.extend(m.stream_id.to_le_bytes());
        bytes.push(m.flags);
        bytes.extend(m.generation.to_le_bytes());
        bytes.extend(m.sequence.to_le_bytes());
        bytes.extend(m.pts_micros.to_le_bytes());
        bytes.extend(self.data);
        Ok(bytes)
    }
}

/// Transport Hello capability for `frames.v4`: the v3 video header plus an optional
/// extension block (flag [`VIDEO_FLAG_EXTENSION`]) and the lane-0 video kinds
/// `0x12`–`0x15` ([`crate::video_control`]). A sender sets the extension flag and
/// emits those kinds only when both peers negotiated it.
pub const FRAMES_V4_CAPABILITY: &str = "frames.v4";
/// Video flag bit: an extension block follows the 29-byte header (`frames.v4` only).
pub const VIDEO_FLAG_EXTENSION: u8 = 0x80;
/// TLV types inside the extension block (protocol/BINARY.md, "frames.v4").
pub mod tlv {
    pub const TIMING: u8 = 0x01;
    pub const CAPTURE_US: u8 = 0x02;
    pub const DISCARDABLE: u8 = 0x03;
    pub const LTR: u8 = 0x04;
    pub const TEMPORAL_ID: u8 = 0x05;
    pub const CONTENT: u8 = 0x06;
}

/// Which video layout a peer speaks: `frames.v3` only, or `frames.v4` as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameVersion {
    V3,
    V4,
}
impl FrameVersion {
    /// `V4` when the negotiated transport capabilities include `frames.v4`.
    pub fn negotiated<S: AsRef<str>>(capabilities: &[S]) -> Self {
        if capabilities
            .iter()
            .any(|c| c.as_ref() == FRAMES_V4_CAPABILITY)
        {
            Self::V4
        } else {
            Self::V3
        }
    }
}

/// Signed stage offsets from H0 (`capture_us`) in microseconds: H2 − H0, H3 − H0,
/// H4 − H0 and H1 − H0. Every one can be negative: H0 is the source's display or
/// present time, which comes after capture (H1) and sometimes after encode on
/// platforms where capture precedes display (Windows WGC), and H1 − H0 is
/// negative whenever the capture callback precedes the present time.
/// [`FrameTiming::NOT_MEASURED`] (`i32::MIN`) marks an H2, H3 or H4 the host does
/// not measure (an NV12 path has no H2); a measured offset saturates at
/// `i32::MIN + 1` and `i32::MAX`, so it never takes that value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTiming {
    pub h2_us: i32,
    pub h3_us: i32,
    pub h4_us: i32,
    pub h1_minus_h0_us: i32,
}
impl FrameTiming {
    /// `0x80000000` on the wire.
    pub const NOT_MEASURED: i32 = i32::MIN;
    const LENGTH: usize = 16;
    /// A measured offset in µs, saturated so it never reads as
    /// [`Self::NOT_MEASURED`].
    pub fn offset_us(us: i64) -> i32 {
        i32::try_from(us.clamp(i64::from(i32::MIN) + 1, i64::from(i32::MAX))).unwrap_or(i32::MAX)
    }
}

/// Long-term reference state of one frame (`0x04 ltr`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LtrInfo {
    /// Sender-chosen token naming one long-term reference; never zero.
    pub token: u32,
    /// [`LtrInfo::MARK`] and/or [`LtrInfo::REFRESH`]; other bits are reserved,
    /// written as zero and ignored on receipt.
    pub flags: u8,
}
impl LtrInfo {
    /// This frame is stored as the long-term reference `token`.
    pub const MARK: u8 = 0x01;
    /// This frame is predicted only from the long-term reference `token`.
    pub const REFRESH: u8 = 0x02;
    const LENGTH: usize = 5;
}

/// The `frames.v4` extension fields of one video frame. Every field is optional on
/// the wire; an absent TLV has the meaning of the field's default: no timing, no
/// capture time, **not discardable**, no LTR state, **temporal layer 0** and no
/// layout epoch. A sender without temporal layers or discardable frames simply
/// omits those TLVs, and the canonical encoding omits every default (so a
/// `discardable` TLV is present only when true and `temporal_id` only when > 0).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoExtensions {
    pub timing: Option<FrameTiming>,
    /// H0, the source present time, on the host's StreamCore monotonic clock (µs).
    /// Kept across recovery re-encodes, unlike `pts_micros`.
    pub capture_us: Option<u64>,
    pub discardable: bool,
    pub ltr: Option<LtrInfo>,
    pub temporal_id: u8,
    /// The `layoutInfo` epoch this frame's content follows.
    pub layout_epoch: Option<u32>,
}
impl VideoExtensions {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
    /// Parse a TLV block (without its `ext_len` prefix). Unknown types are skipped;
    /// a known type with the wrong length, a repeated known type, a zero LTR token
    /// or a TLV running past the block is invalid.
    pub fn decode(mut block: &[u8]) -> Result<Self, MediaError> {
        let mut out = Self::default();
        let mut seen = 0u8;
        while !block.is_empty() {
            let [kind, length, rest @ ..] = block else {
                return Err(MediaError);
            };
            let (value, tail) = rest
                .split_at_checked(usize::from(*length))
                .ok_or(MediaError)?;
            block = tail;
            if !(tlv::TIMING..=tlv::CONTENT).contains(kind) {
                continue;
            }
            let bit = 1u8 << kind;
            if seen & bit != 0 {
                return Err(MediaError);
            }
            seen |= bit;
            let u32_at = |at: usize| -> Result<u32, MediaError> {
                Ok(u32::from_le_bytes(
                    value
                        .get(at..at + 4)
                        .ok_or(MediaError)?
                        .try_into()
                        .map_err(|_| MediaError)?,
                ))
            };
            let exact = |n: usize| {
                if value.len() == n {
                    Ok(())
                } else {
                    Err(MediaError)
                }
            };
            match *kind {
                tlv::TIMING => {
                    exact(FrameTiming::LENGTH)?;
                    out.timing = Some(FrameTiming {
                        h2_us: u32_at(0)? as i32,
                        h3_us: u32_at(4)? as i32,
                        h4_us: u32_at(8)? as i32,
                        h1_minus_h0_us: u32_at(12)? as i32,
                    });
                }
                tlv::CAPTURE_US => {
                    exact(8)?;
                    out.capture_us = Some(u64::from_le_bytes(
                        value.try_into().map_err(|_| MediaError)?,
                    ));
                }
                tlv::DISCARDABLE => {
                    exact(0)?;
                    out.discardable = true;
                }
                tlv::LTR => {
                    exact(LtrInfo::LENGTH)?;
                    let token = u32_at(0)?;
                    if token == 0 {
                        return Err(MediaError);
                    }
                    out.ltr = Some(LtrInfo {
                        token,
                        flags: value[4],
                    });
                }
                tlv::TEMPORAL_ID => {
                    exact(1)?;
                    out.temporal_id = value[0];
                }
                _ => {
                    exact(4)?;
                    out.layout_epoch = Some(u32_at(0)?);
                }
            }
        }
        Ok(out)
    }
    /// The canonical TLV block, in type order, omitting defaults.
    pub fn encode(&self) -> Result<Vec<u8>, MediaError> {
        let mut out = Vec::new();
        let mut put = |kind: u8, value: &[u8]| {
            out.push(kind);
            out.push(value.len() as u8);
            out.extend_from_slice(value);
        };
        if let Some(t) = self.timing {
            let mut value = [0; FrameTiming::LENGTH];
            value[..4].copy_from_slice(&t.h2_us.to_le_bytes());
            value[4..8].copy_from_slice(&t.h3_us.to_le_bytes());
            value[8..12].copy_from_slice(&t.h4_us.to_le_bytes());
            value[12..].copy_from_slice(&t.h1_minus_h0_us.to_le_bytes());
            put(tlv::TIMING, &value);
        }
        if let Some(at) = self.capture_us {
            put(tlv::CAPTURE_US, &at.to_le_bytes());
        }
        if self.discardable {
            put(tlv::DISCARDABLE, &[]);
        }
        if let Some(ltr) = self.ltr {
            if ltr.token == 0 {
                return Err(MediaError);
            }
            let mut value = [0; LtrInfo::LENGTH];
            value[..4].copy_from_slice(&ltr.token.to_le_bytes());
            value[4] = ltr.flags;
            put(tlv::LTR, &value);
        }
        if self.temporal_id != 0 {
            put(tlv::TEMPORAL_ID, &[self.temporal_id]);
        }
        if let Some(epoch) = self.layout_epoch {
            put(tlv::CONTENT, &epoch.to_le_bytes());
        }
        Ok(out)
    }
}

/// A video payload in either layout. Under [`FrameVersion::V3`] it is exactly
/// [`VideoPacket`]: the extension flag is invalid and `extensions` must be empty
/// to decode, and is dropped on encode. Under [`FrameVersion::V4`] the flag
/// `0x80` adds `ext_len u16 LE` and that many bytes of TLVs after the 29-byte
/// header; the encoder sets it only when `extensions` is not empty. The data is
/// borrowed, as in [`VideoPacket`].
#[derive(Debug, PartialEq, Eq)]
pub struct ExtendedVideoPacket<'a> {
    pub metadata: VideoMetadata,
    pub extensions: VideoExtensions,
    pub data: &'a [u8],
}
impl<'a> ExtendedVideoPacket<'a> {
    pub fn decode(bytes: &'a [u8], version: FrameVersion) -> Result<Self, MediaError> {
        let flags = *bytes.get(4).ok_or(MediaError)?;
        if flags & VIDEO_FLAG_EXTENSION == 0 {
            let packet = VideoPacket::decode(bytes)?;
            return Ok(Self {
                metadata: packet.metadata,
                extensions: VideoExtensions::default(),
                data: packet.data,
            });
        }
        if version != FrameVersion::V4 || bytes.len() > MAX_FRAME_LENGTH {
            return Err(MediaError);
        }
        let length = bytes
            .get(VIDEO_HEADER_LENGTH..VIDEO_HEADER_LENGTH + 2)
            .ok_or(MediaError)?;
        let length = usize::from(u16::from_le_bytes(
            length.try_into().map_err(|_| MediaError)?,
        ));
        let start = VIDEO_HEADER_LENGTH + 2;
        let block = bytes.get(start..start + length).ok_or(MediaError)?;
        let data = &bytes[start + length..];
        if data.is_empty() {
            return Err(MediaError);
        }
        let mut header = bytes[..VIDEO_HEADER_LENGTH].to_vec();
        header[4] &= !VIDEO_FLAG_EXTENSION;
        header.push(0); // nonempty data for the shared v3 header check
        let metadata = VideoPacket::decode(&header)?.metadata;
        Ok(Self {
            metadata,
            extensions: VideoExtensions::decode(block)?,
            data,
        })
    }
    pub fn encode(&self, version: FrameVersion) -> Result<Vec<u8>, MediaError> {
        if version == FrameVersion::V3 || self.extensions.is_empty() {
            return VideoPacket {
                metadata: self.metadata,
                data: self.data,
            }
            .encode();
        }
        let block = self.extensions.encode()?;
        let length = u16::try_from(block.len()).map_err(|_| MediaError)?;
        let total = VIDEO_HEADER_LENGTH + 2 + block.len() + self.data.len();
        if self.data.is_empty() || total > MAX_FRAME_LENGTH {
            return Err(MediaError);
        }
        let header = VideoPacket {
            metadata: self.metadata,
            data: &[0],
        }
        .encode()?;
        let mut bytes = Vec::with_capacity(total);
        bytes.extend_from_slice(&header[..VIDEO_HEADER_LENGTH]);
        bytes[4] |= VIDEO_FLAG_EXTENSION;
        bytes.extend(length.to_le_bytes());
        bytes.extend(block);
        bytes.extend_from_slice(self.data);
        Ok(bytes)
    }
}
