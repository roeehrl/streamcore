//! `frames.v3` media headers. Payload slices stay in native buffers; UniFFI
//! carries only the small metadata used by the stream state machine.
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
