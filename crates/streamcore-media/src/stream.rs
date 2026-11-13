//! Stream continuity policy shared by native renderers. Pixels never cross this
//! interface; encoded bytes cross only through the send and receive calls.
use crate::media::VideoMetadata;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Portable admission limits before any native surface allocation or narrowing
/// conversion. Platform availability may impose stricter codec limits.
#[uniffi::export]
pub fn valid_video_settings(max_dimension: i64, max_fps: i64, quality: f64) -> bool {
    (2..=8192).contains(&max_dimension)
        && (1..=120).contains(&max_fps)
        && quality.is_finite()
        && (0.0..=1.0).contains(&quality)
}

/// The most permits a stream may hold at once.
pub const MAX_IN_FLIGHT: u32 = 3;
const CLOSED: u32 = 1 << 31;

/// Called after every permit release, on the releasing thread. It must be a
/// leaf: non-blocking, no lock held across a native call.
pub type ReleaseHook = Arc<dyn Fn() + Send + Sync>;

struct WorkState {
    // Low bits: permits in flight. `CLOSED`: no further admission. One atomic, so
    // a close racing an acquire can never admit after the close returned.
    count: AtomicU32,
    hook: Option<ReleaseHook>,
}

impl Default for WorkState {
    fn default() -> Self {
        Self {
            count: AtomicU32::new(0),
            hook: None,
        }
    }
}

/// Bounds native pixel-buffer submission AND deferred encoded-output handling.
/// Only permits cross FFI; native buffers remain with the platform backend.
/// Lock-free: acquire, release and close are atomics only, so a release is a
/// leaf call on any thread.
#[derive(Default, uniffi::Object)]
pub struct VideoWorkQueue {
    state: Arc<WorkState>,
}

#[uniffi::export]
impl VideoWorkQueue {
    #[uniffi::constructor]
    pub fn new() -> Self {
        Self::default()
    }

    /// A saturated encoder drops a new capture before compression, so it does
    /// not create an encoded dependency that a later frame could reference.
    pub fn acquire(&self) -> Option<Arc<VideoWorkPermit>> {
        self.state
            .count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count & CLOSED == 0 && count < MAX_IN_FLIGHT).then_some(count + 1)
            })
            .ok()?;
        Some(Arc::new(VideoWorkPermit {
            released: AtomicBool::new(false),
            state: self.state.clone(),
        }))
    }

    pub fn close(&self) {
        self.state.count.fetch_or(CLOSED, Ordering::AcqRel);
    }
}

impl VideoWorkQueue {
    /// A queue whose every permit release, explicit or by Drop, also calls
    /// `hook` (after the count is decremented). Rust-only: a stream executor
    /// uses it to post `PermitReleased` to itself.
    pub fn with_release_hook(hook: ReleaseHook) -> Self {
        Self {
            state: Arc::new(WorkState {
                count: AtomicU32::new(0),
                hook: Some(hook),
            }),
        }
    }

    /// Permits currently in flight (not yet released).
    pub fn in_flight(&self) -> u32 {
        self.state.count.load(Ordering::Acquire) & !CLOSED
    }

    pub fn is_closed(&self) -> bool {
        self.state.count.load(Ordering::Acquire) & CLOSED != 0
    }
}

/// Release after native output handling, including queue hops, or on encode
/// failure. Release and Drop are idempotent. A stream may reuse its queue across
/// encoder replacements so deferred old callbacks still count against the bound.
#[derive(uniffi::Object)]
pub struct VideoWorkPermit {
    released: AtomicBool,
    state: Arc<WorkState>,
}
#[uniffi::export]
impl VideoWorkPermit {
    pub fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        self.state.count.fetch_sub(1, Ordering::AcqRel);
        if let Some(hook) = &self.state.hook {
            hook();
        }
    }
}
impl Drop for VideoWorkPermit {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum StreamError {
    #[error("stream sequence exhausted")]
    Exhausted,
    #[error("stream state unavailable")]
    Unavailable,
}
#[derive(Default)]
struct Sender {
    generation: u64,
    sequence: u64,
}
#[derive(uniffi::Object)]
pub struct VideoPacketizer {
    stream_id: u32,
    state: Mutex<Sender>,
}
#[uniffi::export]
impl VideoPacketizer {
    #[uniffi::constructor]
    pub fn new(stream_id: u32) -> Self {
        Self {
            stream_id,
            state: Mutex::new(Sender::default()),
        }
    }
    /// Called once per encoded sample, in encoder output order, before any
    /// congestion drop. A new format or encoder gets a process-unique generation.
    /// Repeat config before every keyframe so late subscribers can recover.
    pub fn next(
        &self,
        keyframe: bool,
        config_changed: bool,
        sequence: u64,
        pts_micros: u64,
    ) -> Result<Vec<VideoMetadata>, StreamError> {
        let mut state = self.state.lock().map_err(|_| StreamError::Unavailable)?;
        if sequence == 0 || sequence <= state.sequence {
            return Ok(Vec::new());
        }
        let changed = config_changed || state.generation == 0;
        if changed {
            let previous = GENERATION
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map_err(|_| StreamError::Exhausted)?;
            state.generation = previous + 1;
            state.sequence = 0;
        }
        state.sequence = sequence;
        let mut packets = Vec::with_capacity(2);
        if changed || keyframe {
            packets.push(VideoMetadata {
                stream_id: self.stream_id,
                generation: state.generation,
                sequence: 0,
                flags: 2,
                pts_micros: 0,
            });
        }
        packets.push(VideoMetadata {
            stream_id: self.stream_id,
            generation: state.generation,
            sequence: state.sequence,
            flags: u8::from(keyframe),
            pts_micros,
        });
        Ok(packets)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum VideoDisposition {
    Configure,
    Decode,
    Ignore,
    Recover,
}
#[derive(Default)]
struct Receiver {
    generation: u64,
    sequence: u64,
    configured: bool,
    keyframe: bool,
    /// H.264 parameters of this generation's configuration, when it is H.264.
    h264: Option<crate::h264::AvcParams>,
    /// `frame_num` of the last reference picture accepted on an intact chain.
    prev_ref_frame_num: Option<u32>,
    /// LTR tokens of this generation the client decoded on an intact chain,
    /// newest last ([`VideoReceiver::note_ltr_decoded`]).
    ltr_decoded: std::collections::VecDeque<u32>,
}

/// How many decoded LTR tokens a receiver remembers.
const LTR_DECODED_KEPT: usize = 64;

/// What a receiver needs of a frame beyond its header (STREAMCORE-VIDEO.md
/// 6.5): the `ltr` TLV and the first bytes of the payload.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ReceivedFrame {
    /// The `ltr` TLV's token (0: none) and flags.
    pub ltr_token: u32,
    pub ltr_flags: u8,
    /// The configuration packet's whole payload (`avcC`/`hvcC`), or the first
    /// bytes of a sample (256 are plenty for an H.264 slice header).
    pub head: Vec<u8>,
}

#[derive(uniffi::Object)]
pub struct VideoReceiver {
    stream_id: u32,
    state: Mutex<Receiver>,
}
#[uniffi::export]
impl VideoReceiver {
    #[uniffi::constructor]
    pub fn new(stream_id: u32) -> Self {
        Self {
            stream_id,
            state: Mutex::new(Receiver::default()),
        }
    }
    pub fn accept(&self, metadata: VideoMetadata) -> VideoDisposition {
        self.accept_inner(metadata, None)
    }
    /// [`Self::accept`] for a stream with temporal layers or LTR (6.5). A gap
    /// in sequences breaks the chain only if it removed a reference picture
    /// (H.264: `frame_num` does not follow the last reference picture's); a
    /// missing discardable frame does not. An LTR refresh (`ltr_flags` `0x02`)
    /// predicted from a token this receiver was told it decoded
    /// ([`Self::note_ltr_decoded`]) restores a broken chain like a keyframe.
    /// For other codecs, or without the configuration's SPS, every gap is a
    /// break, as with `accept`.
    pub fn accept_frame(&self, metadata: VideoMetadata, frame: ReceivedFrame) -> VideoDisposition {
        self.accept_inner(metadata, Some(frame))
    }
    /// The decoder output frame `token` (an `ltr` mark) of `generation`, which
    /// was accepted on an intact chain: an LTR refresh from it can be decoded,
    /// and the client acknowledges it in `videoFeedback` (after decode, never
    /// on submit, 5).
    pub fn note_ltr_decoded(&self, generation: u64, token: u32) {
        let Ok(mut s) = self.state.lock() else {
            return;
        };
        if token == 0 || generation != s.generation || s.ltr_decoded.contains(&token) {
            return;
        }
        if s.ltr_decoded.len() == LTR_DECODED_KEPT {
            s.ltr_decoded.pop_front();
        }
        s.ltr_decoded.push_back(token);
    }
    /// Layer flush, attachment or decode failure invalidates reference frames.
    pub fn require_keyframe(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.keyframe = false;
            // A flushed or new decoder holds no LTR frames.
            s.ltr_decoded.clear();
        }
    }
    pub fn reject_config(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.configured = false;
            s.keyframe = false;
            s.ltr_decoded.clear();
        }
    }
}

impl VideoReceiver {
    fn accept_inner(
        &self,
        metadata: VideoMetadata,
        frame: Option<ReceivedFrame>,
    ) -> VideoDisposition {
        use VideoDisposition::{Configure, Decode, Ignore, Recover};
        let Ok(mut s) = self.state.lock() else {
            return Recover;
        };
        if metadata.stream_id != self.stream_id || metadata.generation < s.generation {
            return Ignore;
        }
        if metadata.validate().is_err() {
            s.keyframe = false;
            return Recover;
        }
        if metadata.generation > s.generation {
            *s = Receiver {
                generation: metadata.generation,
                ..Receiver::default()
            };
        }
        if metadata.flags == 2 {
            if s.configured {
                return Ignore;
            }
            s.configured = true;
            s.keyframe = false;
            s.h264 = frame
                .as_ref()
                .and_then(|f| crate::h264::AvcParams::from_avcc(&f.head));
            return Configure;
        }
        if metadata.sequence <= s.sequence {
            return Ignore;
        }
        let consecutive = s.sequence.checked_add(1) == Some(metadata.sequence);
        s.sequence = metadata.sequence;
        if !s.configured {
            s.keyframe = false;
            return Recover;
        }
        let refs = match (&frame, s.h264) {
            (Some(f), Some(params)) => crate::h264::slice_refs(&f.head, &params),
            _ => None,
        };
        if metadata.flags == 1 {
            s.keyframe = true;
            s.prev_ref_frame_num = refs.map(|r| r.frame_num);
            return Decode;
        }
        let refresh = frame.as_ref().is_some_and(|f| {
            f.ltr_token != 0
                && f.ltr_flags & crate::media::LtrInfo::REFRESH != 0
                && s.ltr_decoded.contains(&f.ltr_token)
        });
        if refresh {
            // Predicted only from an LTR frame this decoder holds.
            s.keyframe = true;
            s.prev_ref_frame_num = refs.map(|r| r.frame_num);
            return Decode;
        }
        if !consecutive {
            // Only a gap of non-reference pictures keeps the chain: the next
            // frame_num follows the last reference picture's.
            let intact = match (refs, s.h264, s.prev_ref_frame_num) {
                (Some(refs), Some(params), Some(prev)) => {
                    refs.frame_num == (prev + 1) % params.max_frame_num()
                }
                _ => false,
            };
            if !intact {
                s.keyframe = false;
            }
        }
        if !s.keyframe {
            return Recover;
        }
        if let Some(refs) = refs.filter(|r| r.nal_ref_idc != 0) {
            s.prev_ref_frame_num = Some(refs.frame_num);
        }
        Decode
    }
}
