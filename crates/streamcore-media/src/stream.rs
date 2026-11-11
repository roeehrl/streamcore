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
        if metadata.flags == 1 {
            s.keyframe = true;
            return Decode;
        }
        if !consecutive {
            s.keyframe = false;
        }
        if s.keyframe { Decode } else { Recover }
    }
    /// Layer flush, attachment or decode failure invalidates reference frames.
    pub fn require_keyframe(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.keyframe = false;
        }
    }
    pub fn reject_config(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.configured = false;
            s.keyframe = false;
        }
    }
}
