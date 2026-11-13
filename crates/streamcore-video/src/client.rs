//! The client decode control plane (STREAMCORE-VIDEO.md 5, 6.2, 6.5, 9.2–9.4).
//!
//! A [`ClientStream`] receives one stream's encoded frames (lane 1), runs a
//! [`VideoReceiver`] over them and owns every decode policy: when to configure,
//! decode, flush or re-create the native decoder, when to ask the host for a
//! keyframe, and what to report in `videoFeedback`. The native decoder behind
//! [`StreamDecoder`] only decodes and presents.
//!
//! Like a host stream, a client stream has exactly one serial executor on its
//! own [`SerialQueue`]: every [`ClientStream`] method and every
//! [`DecoderEvents`] call only posts; decoder calls and [`ClientEvents::post`]
//! run on the executor, during a drain, with no Rust lock held. The rules and
//! the policy are in [`crate::threading`] ("Client").

use crate::error::MediaError;
use crate::executor::{
    Clock, CloseSlot, DrainMarker, StreamPort, blocking_allowed, is_current, lock, next_executor_id,
};
use crate::traits::SerialQueue;
use crate::types::{CloseOutcome, Codec, ColorTags, InvalidateOutcome, TimerToken};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;
use streamcore_media::binary::Frame;
use streamcore_media::media::{
    ExtendedVideoPacket, FrameVersion, LtrInfo, VideoExtensions, VideoMetadata,
};
use streamcore_media::stream::{ReceivedFrame, VideoDisposition, VideoReceiver};
use streamcore_media::video_control::{
    FramePresentation, KIND_VIDEO_FEEDBACK, LossBitmap, VideoFeedback,
};
use streamcore_transport::channel::{ChannelError, SecureChannel, TimedMessage};

/// The envelope kind of a video frame (lane 1).
const KIND_VIDEO: u8 = 0x10;
/// Bytes of a sample's start the receiver reads (an H.264 slice header).
const HEAD_BYTES: usize = 256;
/// Frames kept for `videoFeedback` before the oldest is dropped unreported.
const RECORDS_KEPT: usize = 64;
/// Decoded frames kept while waiting for `presented`.
const DECODED_KEPT: usize = 8;
/// Missing sequences remembered for the loss bitmap.
const LOSS_KEPT: usize = LossBitmap::MAX_BYTES * 8;
/// More than this many decoder re-creations within [`RECREATE_WINDOW_NS`]
/// fail the stream.
const RECREATE_BUDGET: usize = 3;
const RECREATE_WINDOW_NS: u64 = 10_000_000_000;

// ---------------------------------------------------------------------------
// Records.

/// How the native decoder presents (6.1). The decoder applies it; Rust passes
/// it on after every `configure` and on [`ClientStream::set_mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PresentMode {
    /// Present each frame when it is decoded; if several wait for one vsync,
    /// only the newest is shown (R2, R4). The default.
    Responsive,
    /// The newest frame per vsync through an adaptive jitter buffer of 0–1
    /// frames.
    Smooth,
}

/// What [`StreamDecoder::configure`] created.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DecoderInfo {
    /// The platform confirmed its low-latency mode (Android
    /// `KEY_LOW_LATENCY` or a vendor key read back; VideoToolbox
    /// `RealTime`).
    pub low_latency_confirmed: bool,
    pub hardware: bool,
    /// Frames the decoder holds before its first output (its pipeline depth;
    /// 0 or 1 for a low-latency decoder). The decoder queue bound is
    /// `max(2, max_queued)` (6.2).
    pub max_queued: u8,
    /// Free-form notes for diagnostics: the vendor keys applied, retries with
    /// fewer keys, the decoder's name.
    pub notes: Vec<String>,
}

/// One sample's metadata for [`StreamDecoder::decode`], from the frame's
/// header and its frames.v4 extensions (9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct SampleMeta {
    /// The wire sequence: the decoder tags its `decoded` and `presented`
    /// events with it.
    pub seq: u64,
    /// The wire generation.
    pub generation: u64,
    /// The host's submission time (`pts_micros` × 1000): strictly increasing
    /// within a generation. Usable as the platform's presentation time.
    pub pts_ns: u64,
    pub keyframe: bool,
    /// The `discardable` TLV (absent: false): no later frame references it.
    pub discardable: bool,
    /// The `temporal_id` TLV (absent: 0).
    pub temporal_id: u8,
    /// The `ltr` TLV's token (0: none) and flags (`0x01` mark, `0x02`
    /// refresh).
    pub ltr_token: u32,
    pub ltr_flags: u8,
    /// The `content` TLV: the `layoutInfo` epoch this frame follows.
    pub layout_epoch: Option<u32>,
}

/// What [`StreamDecoder::decode`] did with a sample.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum DecodeResult {
    /// Queued for decode. A `decoded` event for `seq` follows, unless the
    /// decoder produces no output for it (a later frame's `decoded` then
    /// retires it).
    Accepted,
    /// No input buffer now. Call [`DecoderEvents::input_ready`] once the
    /// decoder can take input again: Rust then submits the newest waiting
    /// sample.
    Busy,
    /// Not accepted. `DeviceLost` re-creates the decoder; any other error
    /// drops the frame and requests a keyframe.
    Failed { error: MediaError },
}

/// Why a [`ClientStream`] asks the host for a keyframe
/// ([`ClientJob::RequestKeyframe`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum KeyframeReason {
    /// The receiver found the chain broken: a sequence gap that removed a
    /// reference picture, a frame before its generation's configuration, or
    /// an invalid header.
    Broken,
    /// More frames waited for the decoder than its queue bound (6.2): they
    /// were flushed.
    QueueBound,
    /// The decoder was busy and a reference frame waiting for it was replaced.
    Busy,
    /// The decoder refused a frame ([`DecodeResult::Failed`]).
    DecodeFailed,
    /// A new decoder session (after a stall or a failure, or a failed
    /// `configure`) holds no reference frames.
    NewSession,
}

/// Why a [`ClientStream`] replaced its decoder session.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum RecreateReason {
    /// No output for 500 ms while frames waited
    /// ([`ClientTimings::stall_ns`]).
    Stalled,
    /// A recoverable [`DecoderEvents::failed`], or a frame refused with
    /// `DeviceLost`.
    Failed { error: MediaError },
    /// `flush` overran its bound or returned `TimedOut`.
    FlushTimedOut,
}

/// A client stream's counters, published after every drain
/// ([`ClientStream::stats`]) and posted with [`ClientJob::Stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, uniffi::Record)]
pub struct ClientStats {
    /// Video messages of this stream taken by [`ClientStream::receive`].
    pub frames_received: u64,
    /// Messages that did not parse as a video frame of this stream's layout.
    pub invalid_frames: u64,
    /// Frames the receiver ignored (stale, repeated configuration).
    pub ignored: u64,
    /// Frames dropped because the chain was broken (waiting for a keyframe or
    /// an LTR refresh).
    pub waiting_dropped: u64,
    /// Frames the decoder accepted.
    pub submitted: u64,
    pub decoded: u64,
    pub presented: u64,
    /// Of `presented`, those whose C5 the platform only estimated.
    pub presented_estimated: u64,
    /// Frames dropped unshown (`videoFeedback`'s `flushes`, skip cause e):
    /// flushed from the decoder, replaced while it was busy, refused by it, or
    /// given no output.
    pub flushed: u64,
    /// Decoded frames never presented because a newer one was.
    pub present_drops: u64,
    /// Two-frame bound flushes (6.2).
    pub queue_flushes: u64,
    /// `decode` returned `Busy`.
    pub busy: u64,
    /// `decode` returned `Failed`.
    pub decode_failures: u64,
    pub keyframe_requests: u64,
    /// Keyframe requests held back by the rate limit (one is sent when it
    /// allows, if still needed).
    pub keyframe_requests_deferred: u64,
    /// LTR waits armed after a break (150 ms for the host's refresh).
    pub ltr_waits: u64,
    /// LTR tokens acknowledged after decode.
    pub ltr_acks: u64,
    /// Decoder sessions configured.
    pub sessions: u64,
    /// Sessions replaced after a stall, failure or hung flush.
    pub recreates: u64,
    pub stalls: u64,
    /// `DecoderEvents::failed` calls of the current session.
    pub decoder_failures: u64,
    /// `flush` returned `TimedOut` or overran its bound.
    pub flush_timeouts: u64,
    /// Decoder events of a replaced session, or naming no waiting frame.
    pub stale_events: u64,
    /// `videoFeedback` messages enqueued on lane 0.
    pub feedback_sent: u64,
    /// `videoFeedback` messages handed to the consumer
    /// ([`ClientJob::Feedback`]) because there is no frames.v4 channel.
    pub feedback_posted: u64,
    /// `videoFeedback` messages the channel refused.
    pub feedback_refused: u64,
}

/// A client stream's lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ClientPhase {
    /// No decoder session yet: waiting for a configuration packet.
    Waiting,
    Running,
    /// The stream failed ([`ClientJob::Failed`]): frames are dropped until
    /// it is closed.
    Failed,
    Closing,
    Closed,
}

/// Jobs for the consumer, posted to its [`ClientEvents`] on the executor.
/// Like `ObserverJob`, the consumer only enqueues them and handles them
/// elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ClientJob {
    /// A decoder session was configured for wire `generation` (the first one
    /// starts the stream).
    Configured {
        generation: u64,
        info: DecoderInfo,
    },
    /// Ask the host for a keyframe. StreamCore has no lane-0 kind for it: the
    /// consumer sends it over its own control protocol (the host passes it to
    /// `StreamController::request_keyframe`). Rate limited by Rust
    /// ([`ClientTimings::keyframe_interval_ns`]).
    RequestKeyframe {
        reason: KeyframeReason,
    },
    /// A complete enveloped `videoFeedback` message (kind `0x12`, 9.4) for a
    /// stream without a frames.v4 channel. With one, Rust enqueues it on lane
    /// 0 itself and posts nothing.
    Feedback {
        message: Vec<u8>,
    },
    /// The decoder session was replaced; the stream continues.
    DecoderRecreated {
        reason: RecreateReason,
    },
    /// The stream failed: a non-recoverable decoder failure, an unsupported
    /// configuration, or more than 3 re-creations in 10 s. Close it.
    Failed {
        error: MediaError,
    },
    Stats {
        stats: ClientStats,
    },
    /// The close completed: nothing more is called on the decoder.
    Closed {
        outcome: CloseOutcome,
    },
}

/// Time bounds of a client stream. Defaults are the design's values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ClientTimings {
    /// Frames waiting for the decoder (accepted and not decoded, or held while
    /// it is busy) before a flush and a keyframe request (6.2: 2). The bound
    /// is `max(queue_bound, DecoderInfo::max_queued)`.
    pub queue_bound: u8,
    /// No decoder output for this long while frames wait re-creates the
    /// decoder (500 ms).
    pub stall_ns: u64,
    /// While LTR is in use, how long a break waits for the host's LTR refresh
    /// before requesting a keyframe (150 ms).
    pub ltr_wait_ns: u64,
    /// Minimum spacing of keyframe requests (6.5: at most 2 per second).
    pub keyframe_interval_ns: u64,
    /// `videoFeedback` cadence (9.4: about every 100 ms), sent only when
    /// something changed.
    pub feedback_interval_ns: u64,
    /// `StreamDecoder::flush`'s bound (500 ms): a flush that overruns it is
    /// treated as hung.
    pub flush_bound_ns: u64,
}

impl Default for ClientTimings {
    fn default() -> Self {
        Self {
            queue_bound: 2,
            stall_ns: 500_000_000,
            ltr_wait_ns: 150_000_000,
            keyframe_interval_ns: 500_000_000,
            feedback_interval_ns: 100_000_000,
            flush_bound_ns: 500_000_000,
        }
    }
}

// ---------------------------------------------------------------------------
// Traits.

/// The native decoder (5): Android `MediaCodec`, Apple
/// `VTDecompressionSession`. It decodes and presents; every policy (when to
/// configure, flush or re-create, keyframe requests, feedback) is Rust's.
///
/// Named `StreamDecoder` for the same reason as `StreamEncoder`: Swift's
/// standard library has a `Decoder` (Codable). [`crate::Decoder`] is the Rust
/// alias.
///
/// Every method runs on the client stream's executor, during a drain, with no
/// Rust lock held. None of them may call back into the stream synchronously
/// except through [`DecoderEvents`], whose calls only post.
///
/// **Sessions.** Each successful `configure` starts a decoder session with its
/// own `events` port; calls on a replaced session's port are ignored. A new
/// wire generation (a resolution, codec or colour change, or any new encoder
/// on the host) is a new session: Rust calls `flush()` and then `configure()`
/// again on this same object with the new configuration. A decoder that
/// cannot change its configuration in place tears its platform session down
/// and creates a new one inside `configure`. Re-creation after a stall or a
/// failure is the same `flush()` then `configure()`, with the generation's
/// configuration.
#[uniffi::export(rust, foreign)]
pub trait StreamDecoder: Send + Sync {
    /// Start a session for `config` (`avcC` for H.264, `hvcC` for HEVC).
    /// `events` is this session's port; outputs are posted through it, from
    /// any thread. `queue` is the stream's queue (Android: the decoder's
    /// callbacks may run on its Looper). `Unsupported` or `InvalidSettings`
    /// fail the stream; any other error drops the configuration until the host
    /// repeats it (before its next keyframe).
    fn configure(
        &self,
        codec: Codec,
        config: Vec<u8>,
        color: ColorTags,
        queue: Arc<dyn SerialQueue>,
        events: Arc<dyn DecoderEvents>,
    ) -> Result<DecoderInfo, MediaError>;
    /// Queue one length-prefixed sample (`Avcc4`). Never blocks. `events` may
    /// be called from inside this call (VideoToolbox decodes synchronously on
    /// the caller's thread); those calls only post.
    fn decode(&self, sample: Vec<u8>, meta: SampleMeta) -> DecodeResult;
    /// Discard every frame waiting in the session (decoded or not) and its
    /// references. Bounded like `StreamEncoder::invalidate`: returns within
    /// [`ClientTimings::flush_bound_ns`] (500 ms). An adapter whose platform
    /// call has no timeout runs it with [`crate::run_bounded`] (450 ms) and
    /// returns [`InvalidateOutcome::TimedOut`] when that passes; Rust then
    /// treats the session as hung and re-creates it. Events the session posts
    /// after a flush name frames Rust no longer waits for and are ignored.
    fn flush(&self) -> InvalidateOutcome;
    /// Responsive or Smooth presentation (6.1). Applies to the current session
    /// and later ones until changed.
    fn set_mode(&self, mode: PresentMode);
    /// The native monotonic clock, in nanoseconds: the clock of the `c3_ns`
    /// and `c5_ns` that [`DecoderEvents`] report (Android `System.nanoTime`,
    /// Apple `mach_absolute_time` in ns). Read once after every `configure` to
    /// map those times onto the StreamCore clock (9.5).
    fn now_ns(&self) -> u64;
}

/// One decoder session's port. Implemented by Rust; post-only; callable from
/// any thread, including synchronously from inside
/// [`StreamDecoder::decode`] and the platform's output and present callbacks.
/// Each call takes at most the stream's mailbox lock, never a lock a decoder
/// caller holds, and returns in O(1). Calls of a replaced session are
/// ignored.
#[uniffi::export]
pub trait DecoderEvents: Send + Sync {
    /// Frame `seq` was decoded at `c3_ns` (C3, the native clock). Frames
    /// accepted before it that got no output are retired as flushed. An LTR
    /// mark is acknowledged only here, never on submit (5).
    fn decoded(&self, seq: u64, c3_ns: u64);
    /// Frame `seq` reached the glass at `c5_ns` (C5). `estimated`: the
    /// platform gives no present time and this is an estimate (5). Decoded
    /// frames before it that were never presented count as present drops.
    fn presented(&self, seq: u64, c5_ns: u64, estimated: bool);
    /// The decoder can take input again after a `Busy` decode.
    fn input_ready(&self);
    /// The session failed outside any `decode` (a lost device, a codec
    /// error). `recoverable` (or `DeviceLost`): Rust re-creates the session
    /// and requests a keyframe. Otherwise the stream fails.
    fn failed(&self, error: MediaError, recoverable: bool);
}

/// The consumer's side of a client stream. Post-only, like `ObserverQueue`:
/// `post` runs on the executor and must only enqueue the job (never block or
/// call back into the stream synchronously).
#[uniffi::export(rust, foreign)]
pub trait ClientEvents: Send + Sync {
    fn post(&self, job: ClientJob);
}

/// Where a client stream's `videoFeedback` goes when Rust sends it itself.
/// Implemented for [`SecureChannel`]; tests implement it too.
#[doc(hidden)]
pub trait FeedbackTransport: Send + Sync {
    /// Both peers negotiated frames.v4 (lane-0 video kinds may be sent).
    fn frames_v4(&self) -> bool;
    /// Enqueue one complete enveloped lane-0 message; false when refused.
    fn send(&self, message: Vec<u8>) -> bool;
}

struct ChannelFeedback(Arc<SecureChannel>);

impl FeedbackTransport for ChannelFeedback {
    fn frames_v4(&self) -> bool {
        self.0.frames_v4()
    }
    fn send(&self, message: Vec<u8>) -> bool {
        // The pending send is dropped: the message is written regardless.
        self.0.enqueue_application_frame(message).is_ok()
    }
}

/// The StreamCore transport clock (`monotonic_now_us`) in nanoseconds: the
/// clock of `TimedMessage` and of every time in `videoFeedback`, so the
/// executor's timers and the wire times are one clock.
struct TransportClock;

impl Clock for TransportClock {
    fn now_ns(&self) -> u64 {
        streamcore_transport::clock::monotonic_now_us().saturating_mul(1_000)
    }
}

/// Everything a client stream needs. `port` must be the port `queue` calls;
/// it is bound to this stream and cannot be reused.
#[derive(uniffi::Record)]
pub struct ClientParts {
    pub stream_id: u32,
    /// The stream's codec and colour (from the consumer's stream-started
    /// message), passed to every `configure`.
    pub codec: Codec,
    pub color: ColorTags,
    pub mode: PresentMode,
    pub port: Arc<StreamPort>,
    pub queue: Arc<dyn SerialQueue>,
    pub decoder: Arc<dyn StreamDecoder>,
    pub events: Arc<dyn ClientEvents>,
    /// The channel the stream arrives on. When it negotiated frames.v4, Rust
    /// enqueues `videoFeedback` on its lane 0 itself (as the host's sink
    /// writes `frameTiming`); otherwise feedback goes to the consumer as
    /// [`ClientJob::Feedback`].
    pub channel: Option<Arc<SecureChannel>>,
    pub timings: Option<ClientTimings>,
}

// ---------------------------------------------------------------------------
// The consumer's handle.

/// One received stream: a serial executor over a [`StreamDecoder`]. Dropping
/// it without closing posts a close; the executor keeps itself alive until it
/// is `Closed`.
#[derive(uniffi::Object)]
pub struct ClientStream {
    shared: Arc<ClientShared>,
}

#[uniffi::export]
impl ClientStream {
    #[uniffi::constructor]
    pub fn new(parts: ClientParts) -> Result<Arc<Self>, MediaError> {
        Self::with_clock(parts, Arc::new(TransportClock))
    }

    /// One application message from `SecureChannel::receive_timed`. Takes it
    /// (true) when it is a video frame (kind `0x10`) of this stream: C0 and C1
    /// are its `first_byte_us` and `complete_us`. Returns false, leaving the
    /// message to the consumer, for any other kind or stream. Parses on the
    /// calling thread, then posts.
    pub fn receive(&self, message: TimedMessage) -> bool {
        self.shared.receive(&message)
    }

    /// The receive loop for a consumer whose channel carries this stream:
    /// receives from `channel`, hands this stream's video frames to the
    /// executor and returns the first other message (control, input, another
    /// stream's video), so video bytes never leave Rust.
    pub async fn next_message(
        &self,
        channel: Arc<SecureChannel>,
    ) -> Result<TimedMessage, ChannelError> {
        loop {
            let message = channel.receive_timed().await?;
            if !self.shared.receive(&message) {
                return Ok(message);
            }
        }
    }

    /// Responsive or Smooth presentation, passed to the decoder (6.1).
    pub fn set_mode(&self, mode: PresentMode) {
        self.shared.post(ClientEvent::SetMode(mode));
    }

    /// The stream now arrives over another transport (a path change or
    /// handover): LTR frames acknowledged on the old one no longer count, so
    /// a later break waits for a keyframe, not an LTR refresh, until a new
    /// acknowledgement (6.5).
    pub fn note_transport_changed(&self) {
        self.shared.post(ClientEvent::TransportChanged);
    }

    /// Post a [`ClientJob::Stats`].
    pub fn request_stats(&self) {
        self.shared.post(ClientEvent::Stats);
    }

    /// Post a close and return at once: the decoder is flushed, timers stop,
    /// later decoder events are ignored, and [`ClientJob::Closed`] completes
    /// it.
    pub fn begin_close(&self) {
        if self.shared.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        self.shared.post(ClientEvent::Close { keepalive: None });
    }

    /// `begin_close`, then wait without blocking for the close to complete.
    pub async fn close(&self) -> CloseOutcome {
        self.begin_close();
        ClientCloseFuture {
            shared: self.shared.clone(),
        }
        .await
    }

    /// For synchronous native teardown only: as
    /// `StreamController::close_and_wait` (on an executor or a network worker
    /// it asserts in debug builds and returns `Internal` in release builds).
    pub fn close_and_wait(&self, timeout_ms: u64) -> Result<CloseOutcome, MediaError> {
        if !blocking_allowed() {
            debug_assert!(
                false,
                "close_and_wait on a stream executor or a network worker (rule 6)"
            );
            return Err(MediaError::Internal);
        }
        self.begin_close();
        Ok(self
            .shared
            .close_slot
            .wait(Duration::from_millis(timeout_ms))
            .unwrap_or(CloseOutcome::StopUnknown))
    }

    /// The phase the executor last published.
    pub fn phase(&self) -> ClientPhase {
        lock(&self.shared.published).0
    }

    /// The counters the executor last published.
    pub fn stats(&self) -> ClientStats {
        lock(&self.shared.published).1
    }
}

impl ClientStream {
    /// [`ClientStream::new`] with an injected clock (tests). The clock drives
    /// the timers and stamps the wire times (C1 when a message has none, C2,
    /// and the decoder times mapped onto it).
    pub fn with_clock(parts: ClientParts, clock: Arc<dyn Clock>) -> Result<Arc<Self>, MediaError> {
        let transport = parts
            .channel
            .clone()
            .map(|channel| Arc::new(ChannelFeedback(channel)) as Arc<dyn FeedbackTransport>);
        Self::with_transport(parts, clock, transport)
    }

    /// [`ClientStream::with_clock`] with feedback sent on `transport` instead
    /// of `parts.channel` (tests).
    #[doc(hidden)]
    pub fn with_transport(
        parts: ClientParts,
        clock: Arc<dyn Clock>,
        transport: Option<Arc<dyn FeedbackTransport>>,
    ) -> Result<Arc<Self>, MediaError> {
        let timings = parts.timings.unwrap_or_default();
        if timings.queue_bound == 0 || timings.feedback_interval_ns == 0 {
            return Err(MediaError::InvalidSettings);
        }
        let ClientParts {
            stream_id,
            codec,
            color,
            mode,
            port,
            queue,
            decoder,
            events,
            ..
        } = parts;
        let shared = Arc::new_cyclic(|weak: &Weak<ClientShared>| ClientShared {
            id: next_executor_id(),
            stream_id,
            mailbox: Mutex::default(),
            wake_pending: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            queue: Mutex::new(Some(queue)),
            state: Mutex::new(Some(Box::new(ClientState::new(
                weak.clone(),
                stream_id,
                codec,
                color,
                mode,
                timings,
                decoder,
                events,
                transport,
            )))),
            clock,
            published: Mutex::new((ClientPhase::Waiting, ClientStats::default())),
            close_slot: CloseSlot::default(),
            closing: AtomicBool::new(false),
        });
        port.bind_client(&shared)?;
        Ok(Arc::new(Self { shared }))
    }

    /// The close outcome, once the close completed.
    pub fn close_outcome(&self) -> Option<CloseOutcome> {
        self.shared.close_slot.outcome()
    }
}

impl Drop for ClientStream {
    fn drop(&mut self) {
        if self.shared.close_slot.outcome().is_some() {
            return;
        }
        self.shared.closing.store(true, Ordering::Release);
        // The event keeps the executor alive until it reaches Closed.
        self.shared.post(ClientEvent::Close {
            keepalive: Some(self.shared.clone()),
        });
    }
}

struct ClientCloseFuture {
    shared: Arc<ClientShared>,
}

impl Future for ClientCloseFuture {
    type Output = CloseOutcome;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<CloseOutcome> {
        match self.shared.close_slot.poll(cx.waker()) {
            Some(outcome) => Poll::Ready(outcome),
            None => Poll::Pending,
        }
    }
}

// ---------------------------------------------------------------------------
// The executor.

/// A frame parsed off the wire.
struct Received {
    metadata: VideoMetadata,
    extensions: VideoExtensions,
    data: Vec<u8>,
    /// C1 on the transport clock (µs); 0: stamp it on arrival.
    c1_us: u64,
}

enum ClientEvent {
    Frame(Received),
    /// A video message of this stream that does not parse.
    Invalid,
    Decoded {
        session: u64,
        seq: u64,
        c3_ns: u64,
    },
    Presented {
        session: u64,
        seq: u64,
        c5_ns: u64,
        estimated: bool,
    },
    InputReady {
        session: u64,
    },
    Failed {
        session: u64,
        error: MediaError,
        recoverable: bool,
    },
    SetMode(PresentMode),
    TransportChanged,
    Stats,
    Timer(TimerToken),
    Close {
        keepalive: Option<Arc<ClientShared>>,
    },
}

#[derive(Default)]
struct Mailbox {
    events: VecDeque<ClientEvent>,
    closed: bool,
}

pub(crate) struct ClientShared {
    id: u64,
    stream_id: u32,
    mailbox: Mutex<Mailbox>,
    wake_pending: AtomicBool,
    draining: AtomicBool,
    queue: Mutex<Option<Arc<dyn SerialQueue>>>,
    /// Taken out for the length of a drain, so no guard is held across a
    /// native call.
    state: Mutex<Option<Box<ClientState>>>,
    clock: Arc<dyn Clock>,
    published: Mutex<(ClientPhase, ClientStats)>,
    close_slot: CloseSlot,
    closing: AtomicBool,
}

impl ClientShared {
    /// Parse a lane-1 message on the calling thread and post it. False when
    /// it is not a video frame of this stream.
    fn receive(&self, message: &TimedMessage) -> bool {
        let Ok(Some((frame, _))) = Frame::read(&message.payload) else {
            return false;
        };
        if frame.kind != KIND_VIDEO || frame.payload.len() < 4 {
            return false;
        }
        let stream_id = u32::from_le_bytes([
            frame.payload[0],
            frame.payload[1],
            frame.payload[2],
            frame.payload[3],
        ]);
        if stream_id != self.stream_id {
            return false;
        }
        // The channel refuses the extension flag without frames.v4, so V4
        // parses whatever arrived.
        match ExtendedVideoPacket::decode(frame.payload, FrameVersion::V4) {
            Ok(packet) => self.post(ClientEvent::Frame(Received {
                metadata: packet.metadata,
                extensions: packet.extensions,
                data: packet.data.to_vec(),
                c1_us: message.complete_us,
            })),
            Err(_) => self.post(ClientEvent::Invalid),
        }
        true
    }

    /// Enqueue and return in O(1); wakes the queue after the mailbox lock is
    /// dropped. After `Closed` the event is dropped on this thread.
    fn post(&self, event: ClientEvent) {
        let rejected = {
            let mut mailbox = lock(&self.mailbox);
            if mailbox.closed {
                Some(event)
            } else {
                mailbox.events.push_back(event);
                None
            }
        };
        if rejected.is_some() {
            return;
        }
        self.request_wake();
    }

    fn request_wake(&self) {
        if self.wake_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let queue = lock(&self.queue).clone();
        if let Some(queue) = queue {
            queue.wake();
        }
    }

    fn end_drain(&self) {
        self.draining.store(false, Ordering::Release);
        let pending = {
            let mailbox = lock(&self.mailbox);
            !mailbox.closed && !mailbox.events.is_empty()
        };
        if pending {
            self.wake_pending.store(false, Ordering::Release);
            self.request_wake();
        }
    }

    /// A wake or a timer: drain what the mailbox holds now.
    pub(crate) fn run(self: &Arc<Self>, timer: Option<TimerToken>) {
        if let Some(timer) = timer {
            let mut mailbox = lock(&self.mailbox);
            if mailbox.closed {
                return;
            }
            mailbox.events.push_back(ClientEvent::Timer(timer));
        }
        if self
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.wake_pending.store(false, Ordering::Release);
        let batch = std::mem::take(&mut lock(&self.mailbox).events);
        let state = lock(&self.state).take();
        match state {
            None => drop(batch),
            Some(mut state) => {
                let marker = DrainMarker::enter(self.id);
                let drain = &mut state;
                let drained = catch_unwind(AssertUnwindSafe(move || {
                    for event in batch {
                        drain.handle(self, event);
                    }
                    drain.after_drain(self);
                }));
                if drained.is_err() {
                    self.abandon(state);
                } else if state.closed.is_some() {
                    self.finalize(state);
                } else {
                    *lock(&self.state) = Some(state);
                }
                drop(marker);
            }
        }
        self.end_drain();
    }

    /// A drain panicked (a decoder or consumer callback, or Rust): report the
    /// failure if possible, refuse every post and complete the close with
    /// `StopUnknown`. The decoder is not called again.
    fn abandon(&self, state: Box<ClientState>) {
        let (leftovers, queue) = self.close_mailbox();
        {
            let mut published = lock(&self.published);
            published.0 = ClientPhase::Closed;
        }
        let _ = catch_unwind(AssertUnwindSafe(move || {
            state.notify(
                self,
                ClientJob::Failed {
                    error: MediaError::Internal,
                },
            );
            state.notify(
                self,
                ClientJob::Closed {
                    outcome: CloseOutcome::StopUnknown,
                },
            );
            drop((state, leftovers, queue));
        }));
        self.close_slot.complete(CloseOutcome::StopUnknown);
    }

    fn finalize(&self, mut state: Box<ClientState>) {
        let (leftovers, queue) = self.close_mailbox();
        drop(leftovers);
        let keepalive = state.keepalive.take();
        let outcome = state.closed.unwrap_or(CloseOutcome::Closed);
        state.publish(self);
        drop(state);
        drop(queue);
        self.close_slot.complete(outcome);
        drop(keepalive);
    }

    fn close_mailbox(&self) -> (VecDeque<ClientEvent>, Option<Arc<dyn SerialQueue>>) {
        let leftovers = {
            let mut mailbox = lock(&self.mailbox);
            mailbox.closed = true;
            std::mem::take(&mut mailbox.events)
        };
        (leftovers, lock(&self.queue).take())
    }

    fn queue(&self) -> Option<Arc<dyn SerialQueue>> {
        lock(&self.queue).clone()
    }
}

/// One decoder session's [`DecoderEvents`]: holds the executor weakly and
/// carries the session, so a replaced session's calls are ignored.
struct DecoderPort {
    executor: Weak<ClientShared>,
    session: u64,
    /// The newest sequence this session output, raised before the post: a
    /// frame decoded inside `decode` (or while its event waits in the
    /// mailbox) no longer counts against the queue bound.
    output: Arc<AtomicU64>,
}

impl DecoderPort {
    fn post(&self, event: ClientEvent) {
        if let Some(executor) = self.executor.upgrade() {
            executor.post(event);
        }
    }
}

impl DecoderEvents for DecoderPort {
    fn decoded(&self, seq: u64, c3_ns: u64) {
        self.output.fetch_max(seq, Ordering::AcqRel);
        self.post(ClientEvent::Decoded {
            session: self.session,
            seq,
            c3_ns,
        });
    }
    fn presented(&self, seq: u64, c5_ns: u64, estimated: bool) {
        self.post(ClientEvent::Presented {
            session: self.session,
            seq,
            c5_ns,
            estimated,
        });
    }
    fn input_ready(&self) {
        self.post(ClientEvent::InputReady {
            session: self.session,
        });
    }
    fn failed(&self, error: MediaError, recoverable: bool) {
        self.post(ClientEvent::Failed {
            session: self.session,
            error,
            recoverable,
        });
    }
}

// ---------------------------------------------------------------------------
// The stream state, owned by the executor.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerKind {
    /// A rate-limited keyframe request is due.
    Keyframe,
    /// The LTR wait after a break passed.
    LtrWait,
    /// Check for a decoder stall.
    Stall,
    /// Send `videoFeedback`.
    Feedback,
}

/// A sample on its way to the decoder.
struct Sample {
    data: Vec<u8>,
    meta: SampleMeta,
}

/// A frame the decoder accepted and has not output yet.
struct InFlight {
    seq: u64,
    generation: u64,
    ltr_token: u32,
    ltr_flags: u8,
}

/// One frame's client times for `videoFeedback`, on the transport clock (µs;
/// 0: not reached).
struct FrameRecord {
    generation: u64,
    seq: u64,
    c1_us: u64,
    c3_us: u64,
    c5_us: u64,
    /// Nothing more will happen to it (presented, dropped, flushed).
    finished: bool,
}

struct ClientState {
    me: Weak<ClientShared>,
    stream_id: u32,
    codec: Codec,
    color: ColorTags,
    mode: PresentMode,
    timings: ClientTimings,
    decoder: Option<Arc<dyn StreamDecoder>>,
    events: Option<Arc<dyn ClientEvents>>,
    transport: Option<Arc<dyn FeedbackTransport>>,
    receiver: VideoReceiver,
    phase: ClientPhase,

    // The wire stream.
    /// The current wire generation (0: none yet).
    generation: u64,
    /// The current generation's configuration, for re-creation.
    config: Option<Vec<u8>>,
    /// Highest sample sequence received in `generation`.
    highest_seq: u64,

    // The decoder session.
    /// The current session (0: none).
    session: u64,
    /// The current session was configured and is not hung.
    live: bool,
    info: Option<DecoderInfo>,
    /// Transport clock minus the decoder's clock, in ns.
    clock_offset_ns: i128,
    /// The session port's newest output sequence.
    output: Arc<AtomicU64>,
    in_flight: VecDeque<InFlight>,
    /// The newest sample waiting while the decoder is busy.
    held: Option<Sample>,
    /// `decode` returned `Busy` and no `input_ready` followed yet.
    busy: bool,
    /// Decoded frames waiting for `presented` (wire generation, sequence).
    decoded: VecDeque<(u64, u64)>,
    /// The last decoder output, or when frames started waiting after none
    /// did (the stall clock).
    last_progress_ns: u64,
    recreates: VecDeque<u64>,

    // Recovery.
    /// Frames are dropped until a keyframe or an LTR refresh restores the
    /// chain.
    broken: bool,
    /// An LTR frame was decoded and acknowledged on this session and
    /// transport: a break first waits for the host's LTR refresh.
    ltr_in_use: bool,
    last_keyframe_request_ns: Option<u64>,
    deferred_reason: KeyframeReason,

    // Feedback.
    records: VecDeque<FrameRecord>,
    /// Missing sequences of `generation` not yet reported, ascending.
    loss: VecDeque<u64>,
    consumed_bytes: u64,
    flushes: u32,
    present_drops: u32,
    /// A newly decoded LTR token to acknowledge in the next report.
    pending_ack: Option<u32>,
    /// Something changed since the last report.
    dirty: bool,

    // Timers.
    timers: BTreeMap<TimerToken, (u64, TimerKind)>,
    next_timer: TimerToken,
    armed: Option<(u64, TimerToken)>,

    stats: ClientStats,
    closed: Option<CloseOutcome>,
    keepalive: Option<Arc<ClientShared>>,
}

impl ClientState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        me: Weak<ClientShared>,
        stream_id: u32,
        codec: Codec,
        color: ColorTags,
        mode: PresentMode,
        timings: ClientTimings,
        decoder: Arc<dyn StreamDecoder>,
        events: Arc<dyn ClientEvents>,
        transport: Option<Arc<dyn FeedbackTransport>>,
    ) -> Self {
        Self {
            me,
            stream_id,
            codec,
            color,
            mode,
            timings,
            decoder: Some(decoder),
            events: Some(events),
            transport,
            receiver: VideoReceiver::new(stream_id),
            phase: ClientPhase::Waiting,
            generation: 0,
            config: None,
            highest_seq: 0,
            session: 0,
            live: false,
            info: None,
            clock_offset_ns: 0,
            output: Arc::default(),
            in_flight: VecDeque::new(),
            held: None,
            busy: false,
            decoded: VecDeque::new(),
            last_progress_ns: 0,
            recreates: VecDeque::new(),
            broken: false,
            ltr_in_use: false,
            last_keyframe_request_ns: None,
            deferred_reason: KeyframeReason::Broken,
            records: VecDeque::new(),
            loss: VecDeque::new(),
            consumed_bytes: 0,
            flushes: 0,
            present_drops: 0,
            pending_ack: None,
            dirty: false,
            timers: BTreeMap::new(),
            next_timer: 0,
            armed: None,
            stats: ClientStats::default(),
            closed: None,
            keepalive: None,
        }
    }

    fn handle(&mut self, cx: &Arc<ClientShared>, event: ClientEvent) {
        if self.closed.is_some() {
            return;
        }
        match event {
            ClientEvent::Frame(frame) => self.on_frame(cx, frame),
            ClientEvent::Invalid => {
                self.stats.invalid_frames += 1;
                if self.phase != ClientPhase::Failed {
                    self.on_break(cx, KeyframeReason::Broken);
                }
            }
            ClientEvent::Decoded {
                session,
                seq,
                c3_ns,
            } => self.on_decoded(cx, session, seq, c3_ns),
            ClientEvent::Presented {
                session,
                seq,
                c5_ns,
                estimated,
            } => self.on_presented(cx, session, seq, c5_ns, estimated),
            ClientEvent::InputReady { session } => self.on_input_ready(cx, session),
            ClientEvent::Failed {
                session,
                error,
                recoverable,
            } => self.on_failed(cx, session, error, recoverable),
            ClientEvent::SetMode(mode) => {
                self.mode = mode;
                if self.live
                    && let Some(decoder) = self.decoder.clone()
                {
                    decoder.set_mode(mode);
                }
            }
            ClientEvent::TransportChanged => {
                self.receiver.forget_ltr();
                self.pending_ack = None;
                self.ltr_in_use = false;
            }
            ClientEvent::Stats => {
                let stats = self.stats;
                self.notify(cx, ClientJob::Stats { stats });
            }
            ClientEvent::Timer(token) => {
                if self.armed.is_some_and(|(_, armed)| armed == token) {
                    self.armed = None;
                }
            }
            ClientEvent::Close { keepalive } => {
                if keepalive.is_some() && self.keepalive.is_none() {
                    self.keepalive = keepalive;
                }
                self.on_close(cx);
            }
        }
    }

    fn after_drain(&mut self, cx: &Arc<ClientShared>) {
        if self.closed.is_none() {
            let now = cx.clock.now_ns();
            let due: Vec<_> = self
                .timers
                .iter()
                .filter(|(_, (deadline, _))| *deadline <= now)
                .map(|(token, (_, kind))| (*token, *kind))
                .collect();
            for (token, kind) in due {
                if self.timers.remove(&token).is_some() {
                    self.on_timer(cx, kind);
                }
                if self.closed.is_some() {
                    break;
                }
            }
            if self.closed.is_none() {
                self.arm(cx);
            }
        }
        self.publish(cx);
    }

    fn publish(&self, cx: &ClientShared) {
        let phase = if self.closed.is_some() {
            ClientPhase::Closed
        } else if cx.closing.load(Ordering::Acquire) {
            ClientPhase::Closing
        } else {
            self.phase
        };
        *lock(&cx.published) = (phase, self.stats);
    }

    // -----------------------------------------------------------------------
    // Outbound helpers: on the executor, during a drain, no Rust lock held.

    fn notify(&self, cx: &ClientShared, job: ClientJob) {
        debug_assert!(is_current(cx.id));
        if let Some(events) = &self.events {
            events.post(job);
        }
    }

    fn set_timer(&mut self, cx: &ClientShared, delay_ns: u64, kind: TimerKind) {
        self.cancel_timer(kind);
        self.next_timer += 1;
        let deadline = cx.clock.now_ns().saturating_add(delay_ns);
        self.timers.insert(self.next_timer, (deadline, kind));
    }

    fn cancel_timer(&mut self, kind: TimerKind) {
        self.timers.retain(|_, (_, k)| *k != kind);
    }

    fn has_timer(&self, kind: TimerKind) -> bool {
        self.timers.values().any(|(_, k)| *k == kind)
    }

    fn arm(&mut self, cx: &ClientShared) {
        let Some((&token, &(deadline, _))) = self.timers.iter().min_by_key(|(_, (d, _))| *d) else {
            return;
        };
        if self.armed.is_some_and(|(armed, _)| armed <= deadline) {
            return;
        }
        self.armed = Some((deadline, token));
        let delay = deadline.saturating_sub(cx.clock.now_ns());
        if let Some(queue) = cx.queue() {
            queue.wake_after(delay, token);
        }
    }

    fn on_timer(&mut self, cx: &Arc<ClientShared>, kind: TimerKind) {
        match kind {
            TimerKind::Keyframe => {
                if self.broken && self.phase != ClientPhase::Failed {
                    let reason = self.deferred_reason;
                    self.request_keyframe(cx, reason);
                }
            }
            TimerKind::LtrWait => {
                if self.broken && self.phase != ClientPhase::Failed {
                    self.request_keyframe(cx, KeyframeReason::Broken);
                }
            }
            TimerKind::Stall => self.check_stall(cx),
            TimerKind::Feedback => self.send_feedback(cx),
        }
    }

    /// The transport clock in µs.
    fn now_us(cx: &ClientShared) -> u64 {
        cx.clock.now_ns() / 1_000
    }

    /// A decoder time (its clock, ns) on the transport clock (µs); never 0.
    fn wire_us(&self, native_ns: u64) -> u64 {
        let ns = i128::from(native_ns) + self.clock_offset_ns;
        u64::try_from(ns / 1_000).unwrap_or(0).max(1)
    }

    // -----------------------------------------------------------------------
    // Frames.

    fn on_frame(&mut self, cx: &Arc<ClientShared>, frame: Received) {
        if self.phase == ClientPhase::Failed {
            return;
        }
        let Received {
            metadata,
            extensions,
            data,
            c1_us,
        } = frame;
        self.stats.frames_received += 1;
        let c1_us = if c1_us == 0 {
            Self::now_us(cx).max(1)
        } else {
            c1_us
        };
        if metadata.generation > self.generation {
            self.new_generation(cx, metadata.generation);
        }
        let current = metadata.generation == self.generation;
        if current {
            self.consumed_bytes = self.consumed_bytes.saturating_add(data.len() as u64);
            self.mark_dirty(cx);
        }
        let is_config = metadata.flags == 2;
        if !is_config && current && metadata.sequence > self.highest_seq {
            if self.highest_seq != 0 {
                for missing in self.highest_seq + 1..metadata.sequence {
                    if self.loss.len() == LOSS_KEPT {
                        self.loss.pop_front();
                    }
                    self.loss.push_back(missing);
                }
            }
            self.highest_seq = metadata.sequence;
        }
        if !is_config {
            if self.records.len() == RECORDS_KEPT {
                self.records.pop_front();
            }
            self.records.push_back(FrameRecord {
                generation: metadata.generation,
                seq: metadata.sequence,
                c1_us,
                c3_us: 0,
                c5_us: 0,
                finished: false,
            });
        }
        let ltr = extensions.ltr.unwrap_or(LtrInfo { token: 0, flags: 0 });
        let head = if is_config {
            data.clone()
        } else {
            data[..data.len().min(HEAD_BYTES)].to_vec()
        };
        let disposition = self.receiver.accept_frame(
            metadata,
            ReceivedFrame {
                ltr_token: ltr.token,
                ltr_flags: ltr.flags,
                head,
            },
        );
        match disposition {
            VideoDisposition::Configure => self.on_config(cx, data),
            VideoDisposition::Decode => {
                if self.broken {
                    // A keyframe or an LTR refresh restored the chain.
                    self.broken = false;
                    self.cancel_timer(TimerKind::Keyframe);
                    self.cancel_timer(TimerKind::LtrWait);
                }
                let meta = SampleMeta {
                    seq: metadata.sequence,
                    generation: metadata.generation,
                    pts_ns: metadata.pts_micros.saturating_mul(1_000),
                    keyframe: metadata.flags == 1,
                    discardable: extensions.discardable,
                    temporal_id: extensions.temporal_id,
                    ltr_token: ltr.token,
                    ltr_flags: ltr.flags,
                    layout_epoch: extensions.layout_epoch,
                };
                self.submit(cx, Sample { data, meta });
            }
            VideoDisposition::Ignore => {
                self.stats.ignored += 1;
                self.finish(metadata.generation, metadata.sequence);
            }
            VideoDisposition::Recover => {
                self.stats.waiting_dropped += 1;
                self.finish(metadata.generation, metadata.sequence);
                self.on_break(cx, KeyframeReason::Broken);
            }
        }
    }

    /// A new wire generation: report the old one, then start counting afresh.
    /// The decoder session is replaced when its configuration arrives.
    fn new_generation(&mut self, cx: &Arc<ClientShared>, generation: u64) {
        if self.generation != 0 && self.dirty {
            self.send_feedback(cx);
        }
        self.records.clear();
        self.loss.clear();
        self.generation = generation;
        self.highest_seq = 0;
        self.config = None;
        self.pending_ack = None;
        self.ltr_in_use = false;
    }

    /// A configuration packet the receiver accepted: a new session.
    fn on_config(&mut self, cx: &Arc<ClientShared>, config: Vec<u8>) {
        if self.live {
            // The old generation's frames are not shown.
            if self.flush_decoder(cx) {
                self.live = false;
            }
        }
        self.config = Some(config.clone());
        self.open_session(cx, config);
    }

    /// `configure` a new session. On success the stream runs; `Unsupported`
    /// and `InvalidSettings` fail it; any other error waits for the host's
    /// next configuration (the receiver forgets this one) and counts against
    /// the re-creation budget.
    fn open_session(&mut self, cx: &Arc<ClientShared>, config: Vec<u8>) -> bool {
        let (Some(decoder), Some(queue)) = (self.decoder.clone(), cx.queue()) else {
            return false;
        };
        self.session += 1;
        self.live = false;
        self.busy = false;
        self.output = Arc::default();
        let port: Arc<dyn DecoderEvents> = Arc::new(DecoderPort {
            executor: self.me.clone(),
            session: self.session,
            output: self.output.clone(),
        });
        match decoder.configure(self.codec, config, self.color, queue, port) {
            Ok(info) => {
                self.live = true;
                self.clock_offset_ns = i128::from(cx.clock.now_ns()) - i128::from(decoder.now_ns());
                decoder.set_mode(self.mode);
                self.last_progress_ns = cx.clock.now_ns();
                self.stats.sessions += 1;
                self.phase = ClientPhase::Running;
                self.info = Some(info.clone());
                self.notify(
                    cx,
                    ClientJob::Configured {
                        generation: self.generation,
                        info,
                    },
                );
                true
            }
            Err(error @ (MediaError::Unsupported | MediaError::InvalidSettings)) => {
                self.fail(cx, error);
                false
            }
            Err(error) => {
                self.receiver.reject_config();
                if self.note_recreate(cx) {
                    self.on_break(cx, KeyframeReason::NewSession);
                } else {
                    self.fail(cx, error);
                }
                false
            }
        }
    }

    /// Frames waiting for the decoder: accepted and not output, or held. A
    /// frame whose `decoded` is still in the mailbox has been output (a
    /// session's sequences increase, and it outputs in decode order).
    fn waiting(&self) -> usize {
        let output = self.output.load(Ordering::Acquire);
        self.in_flight.iter().filter(|f| f.seq > output).count() + usize::from(self.held.is_some())
    }

    fn queue_bound(&self) -> usize {
        let declared = self.info.as_ref().map_or(0, |info| info.max_queued);
        usize::from(self.timings.queue_bound.max(declared))
    }

    /// A frame the receiver passed: the two-frame bound, then the busy hold,
    /// then `decode`.
    fn submit(&mut self, cx: &Arc<ClientShared>, sample: Sample) {
        if !self.live {
            // No session (a configure failed or a flush hung): the chain
            // restarts with the next session.
            self.drop_sample(&sample);
            self.receiver.require_keyframe();
            self.on_break(cx, KeyframeReason::NewSession);
            return;
        }
        if self.waiting() >= self.queue_bound() {
            // 6.2: flush what waits and start again at a keyframe.
            self.stats.queue_flushes += 1;
            if !self.flush_decoder(cx) {
                self.recreate(cx, RecreateReason::FlushTimedOut);
                self.drop_sample(&sample);
                return;
            }
            self.ltr_in_use = false;
            if !sample.meta.keyframe {
                self.receiver.require_keyframe();
                self.drop_sample(&sample);
                self.on_break(cx, KeyframeReason::QueueBound);
                return;
            }
            // The receiver already restarted the chain at this keyframe; the
            // flushed session holds none of the LTR frames it decoded.
            self.receiver.forget_ltr();
        }
        if self.busy || self.held.is_some() {
            // The decoder is busy: the newest sample waits for input_ready.
            self.note_waiting(cx);
            match self.held.take() {
                Some(old) if !old.meta.discardable && !sample.meta.keyframe => {
                    // Replacing a reference frame breaks the chain: drop both.
                    self.drop_sample(&old);
                    self.drop_sample(&sample);
                    self.receiver.require_keyframe();
                    self.ltr_in_use = false;
                    self.on_break(cx, KeyframeReason::Busy);
                }
                old => {
                    if let Some(old) = old {
                        self.drop_sample(&old);
                    }
                    self.held = Some(sample);
                }
            }
            return;
        }
        self.decode(cx, sample);
    }

    fn decode(&mut self, cx: &Arc<ClientShared>, sample: Sample) {
        let Some(decoder) = self.decoder.clone() else {
            return;
        };
        let session = self.session;
        let Sample { data, meta } = sample;
        // Rust keeps the bytes until the decoder takes them: a `Busy` sample
        // is retried at `input_ready`.
        let result = decoder.decode(data.clone(), meta);
        if self.session != session || self.closed.is_some() {
            return;
        }
        match result {
            DecodeResult::Accepted => {
                self.note_waiting(cx);
                self.stats.submitted += 1;
                self.in_flight.push_back(InFlight {
                    seq: meta.seq,
                    generation: meta.generation,
                    ltr_token: meta.ltr_token,
                    ltr_flags: meta.ltr_flags,
                });
            }
            DecodeResult::Busy => {
                self.stats.busy += 1;
                self.busy = true;
                self.note_waiting(cx);
                self.held = Some(Sample { data, meta });
            }
            DecodeResult::Failed { error } => {
                self.stats.decode_failures += 1;
                self.drop_sample(&Sample { data, meta });
                if error == MediaError::DeviceLost {
                    self.recreate(cx, RecreateReason::Failed { error });
                } else {
                    self.receiver.require_keyframe();
                    self.ltr_in_use = false;
                    self.on_break(cx, KeyframeReason::DecodeFailed);
                }
            }
        }
    }

    /// A frame dropped unshown (skip cause e).
    fn drop_sample(&mut self, sample: &Sample) {
        self.count_flushed(1);
        self.finish(sample.meta.generation, sample.meta.seq);
    }

    fn count_flushed(&mut self, frames: u64) {
        self.stats.flushed += frames;
        self.flushes = self
            .flushes
            .saturating_add(u32::try_from(frames).unwrap_or(u32::MAX));
        self.dirty = true;
    }

    /// Frames start waiting after none did: the stall clock starts now.
    fn note_waiting(&mut self, cx: &Arc<ClientShared>) {
        if self.waiting() == 0 {
            self.last_progress_ns = cx.clock.now_ns();
        }
        if !self.has_timer(TimerKind::Stall) {
            let deadline = self.last_progress_ns.saturating_add(self.timings.stall_ns);
            let delay = deadline.saturating_sub(cx.clock.now_ns());
            self.set_timer(cx, delay, TimerKind::Stall);
        }
    }

    fn check_stall(&mut self, cx: &Arc<ClientShared>) {
        if !self.live || self.waiting() == 0 || self.phase == ClientPhase::Failed {
            return;
        }
        let now = cx.clock.now_ns();
        let deadline = self.last_progress_ns.saturating_add(self.timings.stall_ns);
        if now >= deadline {
            self.stats.stalls += 1;
            self.recreate(cx, RecreateReason::Stalled);
        } else {
            self.set_timer(cx, deadline - now, TimerKind::Stall);
        }
    }

    /// Flush the current session: every waiting frame is dropped unshown.
    /// False when the flush timed out or overran its bound (the session is
    /// hung).
    fn flush_decoder(&mut self, cx: &Arc<ClientShared>) -> bool {
        let dropped = self.waiting() as u64;
        for entry in std::mem::take(&mut self.in_flight) {
            self.finish(entry.generation, entry.seq);
        }
        if let Some(held) = self.held.take() {
            self.finish(held.meta.generation, held.meta.seq);
        }
        if dropped > 0 {
            self.count_flushed(dropped);
        }
        self.busy = false;
        for (generation, seq) in std::mem::take(&mut self.decoded) {
            self.finish(generation, seq);
        }
        self.cancel_timer(TimerKind::Stall);
        let Some(decoder) = self.decoder.clone() else {
            return true;
        };
        if !self.live {
            return true;
        }
        let started = cx.clock.now_ns();
        let outcome = decoder.flush();
        let overran = cx.clock.now_ns().saturating_sub(started) > self.timings.flush_bound_ns;
        if outcome == InvalidateOutcome::TimedOut || overran {
            self.stats.flush_timeouts += 1;
            self.live = false;
            return false;
        }
        true
    }

    /// Replace the session: flush it, `configure` again with this
    /// generation's configuration, and start again at a keyframe. More than
    /// 3 re-creations in 10 s fail the stream.
    fn recreate(&mut self, cx: &Arc<ClientShared>, reason: RecreateReason) {
        self.stats.recreates += 1;
        if !self.note_recreate(cx) {
            let error = match reason {
                RecreateReason::Failed { error } => error,
                _ => MediaError::Timeout,
            };
            self.fail(cx, error);
            return;
        }
        self.notify(cx, ClientJob::DecoderRecreated { reason });
        // A hung session is not flushed again (`flush_decoder` only retires
        // its frames): configure replaces it either way.
        let _ = self.flush_decoder(cx);
        self.live = false;
        self.receiver.require_keyframe();
        self.ltr_in_use = false;
        match self.config.clone() {
            Some(config) => {
                if self.open_session(cx, config) {
                    self.on_break(cx, KeyframeReason::NewSession);
                }
            }
            None => {
                // No configuration yet: the host's next one starts a session.
                self.receiver.reject_config();
                self.on_break(cx, KeyframeReason::NewSession);
            }
        }
    }

    /// Count a re-creation (or a failed configure); false when the budget is
    /// spent.
    fn note_recreate(&mut self, cx: &ClientShared) -> bool {
        let now = cx.clock.now_ns();
        while self
            .recreates
            .front()
            .is_some_and(|at| now.saturating_sub(*at) > RECREATE_WINDOW_NS)
        {
            self.recreates.pop_front();
        }
        self.recreates.push_back(now);
        self.recreates.len() <= RECREATE_BUDGET
    }

    fn fail(&mut self, cx: &Arc<ClientShared>, error: MediaError) {
        if self.phase == ClientPhase::Failed {
            return;
        }
        let _ = self.flush_decoder(cx);
        self.live = false;
        self.phase = ClientPhase::Failed;
        self.timers
            .retain(|_, (_, kind)| *kind == TimerKind::Feedback);
        self.notify(cx, ClientJob::Failed { error });
    }

    // -----------------------------------------------------------------------
    // Recovery (6.5).

    /// The chain is broken: wait for an LTR refresh if LTR is in use,
    /// otherwise ask for a keyframe (rate limited).
    fn on_break(&mut self, cx: &Arc<ClientShared>, reason: KeyframeReason) {
        self.broken = true;
        if reason == KeyframeReason::Broken && self.ltr_in_use {
            if !self.has_timer(TimerKind::LtrWait) && !self.has_timer(TimerKind::Keyframe) {
                self.stats.ltr_waits += 1;
                self.set_timer(cx, self.timings.ltr_wait_ns, TimerKind::LtrWait);
            }
            return;
        }
        self.request_keyframe(cx, reason);
    }

    /// At most one request per `keyframe_interval_ns`; a request inside the
    /// interval is deferred to its end and sent then only if the chain is
    /// still broken.
    fn request_keyframe(&mut self, cx: &Arc<ClientShared>, reason: KeyframeReason) {
        let now = cx.clock.now_ns();
        if let Some(last) = self.last_keyframe_request_ns {
            let next = last.saturating_add(self.timings.keyframe_interval_ns);
            if now < next {
                if !self.has_timer(TimerKind::Keyframe) {
                    self.stats.keyframe_requests_deferred += 1;
                    self.deferred_reason = reason;
                    self.set_timer(cx, next - now, TimerKind::Keyframe);
                }
                return;
            }
        }
        self.cancel_timer(TimerKind::Keyframe);
        self.cancel_timer(TimerKind::LtrWait);
        self.last_keyframe_request_ns = Some(now);
        self.stats.keyframe_requests += 1;
        self.notify(cx, ClientJob::RequestKeyframe { reason });
    }

    // -----------------------------------------------------------------------
    // Decoder events.

    fn current(&mut self, session: u64) -> bool {
        let current = session == self.session && self.live;
        if !current {
            self.stats.stale_events += 1;
        }
        current
    }

    fn on_decoded(&mut self, cx: &Arc<ClientShared>, session: u64, seq: u64, c3_ns: u64) {
        if !self.current(session) {
            return;
        }
        let Some(index) = self.in_flight.iter().position(|f| f.seq == seq) else {
            self.stats.stale_events += 1;
            return;
        };
        // Frames accepted before it got no output.
        let skipped: Vec<_> = self.in_flight.drain(..index).collect();
        if !skipped.is_empty() {
            self.count_flushed(skipped.len() as u64);
            for entry in skipped {
                self.finish(entry.generation, entry.seq);
            }
        }
        let Some(entry) = self.in_flight.pop_front() else {
            return;
        };
        self.stats.decoded += 1;
        self.last_progress_ns = cx.clock.now_ns();
        let c3_us = self.wire_us(c3_ns);
        if let Some(record) = self.record(entry.generation, entry.seq) {
            record.c3_us = c3_us;
        }
        if self.decoded.len() == DECODED_KEPT
            && let Some((generation, seq)) = self.decoded.pop_front()
        {
            self.finish(generation, seq);
        }
        self.decoded.push_back((entry.generation, entry.seq));
        if entry.ltr_token != 0 && entry.ltr_flags & LtrInfo::MARK != 0 {
            // 5: acknowledged after decode, never on submit.
            self.receiver
                .note_ltr_decoded(entry.generation, entry.ltr_token);
            if entry.generation == self.generation {
                self.pending_ack = Some(entry.ltr_token);
                self.ltr_in_use = true;
                self.stats.ltr_acks += 1;
            }
        }
        self.cancel_timer(TimerKind::Stall);
        if self.waiting() > 0 {
            self.note_waiting(cx);
        }
        self.mark_dirty(cx);
    }

    fn on_presented(
        &mut self,
        cx: &Arc<ClientShared>,
        session: u64,
        seq: u64,
        c5_ns: u64,
        estimated: bool,
    ) {
        if !self.current(session) {
            return;
        }
        let Some(index) = self.decoded.iter().position(|(_, s)| *s == seq) else {
            self.stats.stale_events += 1;
            return;
        };
        // Decoded before it and never presented: replaced at the vsync.
        let replaced: Vec<_> = self.decoded.drain(..index).collect();
        for (generation, seq) in replaced {
            self.stats.present_drops += 1;
            self.present_drops = self.present_drops.saturating_add(1);
            self.finish(generation, seq);
        }
        let Some((generation, seq)) = self.decoded.pop_front() else {
            return;
        };
        self.stats.presented += 1;
        if estimated {
            self.stats.presented_estimated += 1;
        }
        let c5_us = self.wire_us(c5_ns);
        if let Some(record) = self.record(generation, seq) {
            record.c5_us = c5_us;
            record.finished = true;
        }
        self.mark_dirty(cx);
    }

    fn on_input_ready(&mut self, cx: &Arc<ClientShared>, session: u64) {
        if !self.current(session) {
            return;
        }
        self.busy = false;
        if let Some(sample) = self.held.take() {
            self.decode(cx, sample);
        }
    }

    fn on_failed(
        &mut self,
        cx: &Arc<ClientShared>,
        session: u64,
        error: MediaError,
        recoverable: bool,
    ) {
        if !self.current(session) {
            return;
        }
        self.stats.decoder_failures += 1;
        if recoverable || error == MediaError::DeviceLost {
            self.recreate(cx, RecreateReason::Failed { error });
        } else {
            self.fail(cx, error);
        }
    }

    // -----------------------------------------------------------------------
    // Feedback (9.4).

    fn record(&mut self, generation: u64, seq: u64) -> Option<&mut FrameRecord> {
        self.records
            .iter_mut()
            .rev()
            .find(|r| r.generation == generation && r.seq == seq)
    }

    fn finish(&mut self, generation: u64, seq: u64) {
        if let Some(record) = self.record(generation, seq) {
            record.finished = true;
        }
    }

    /// Something to report: a report goes out one interval after the first
    /// change since the last one.
    fn mark_dirty(&mut self, cx: &ClientShared) {
        self.dirty = true;
        if !self.has_timer(TimerKind::Feedback) && self.closed.is_none() {
            self.set_timer(cx, self.timings.feedback_interval_ns, TimerKind::Feedback);
        }
    }

    /// One `videoFeedback` for the current generation, if anything changed:
    /// frames that finished or waited one interval since C1 (each reported
    /// once, at most 32, the newest), missing sequences, the counters and a
    /// new LTR acknowledgement.
    fn send_feedback(&mut self, cx: &Arc<ClientShared>) {
        self.cancel_timer(TimerKind::Feedback);
        if !self.dirty || self.generation == 0 {
            return;
        }
        let now_us = Self::now_us(cx);
        let interval_us = self.timings.feedback_interval_ns / 1_000;
        let generation = self.generation;
        let mut frames = Vec::new();
        self.records.retain(|r| {
            let ready = r.finished || now_us.saturating_sub(r.c1_us) >= interval_us;
            if ready && r.generation == generation && r.seq != 0 {
                frames.push(FramePresentation {
                    sequence: r.seq,
                    receive_us: r.c1_us,
                    decode_us: r.c3_us,
                    present_us: r.c5_us,
                });
            }
            !ready
        });
        if frames.len() > VideoFeedback::MAX_FRAMES {
            frames.drain(..frames.len() - VideoFeedback::MAX_FRAMES);
        }
        let loss = self.loss_bitmap();
        let feedback = VideoFeedback {
            stream_id: self.stream_id,
            generation,
            highest_complete_sequence: self.highest_seq,
            consumed_bytes: self.consumed_bytes,
            flushes: self.flushes,
            present_drops: self.present_drops,
            loss,
            ltr_ack: self.pending_ack.take(),
            frames,
        };
        self.dirty = !self.records.is_empty() || !self.loss.is_empty();
        if self.dirty {
            self.set_timer(cx, self.timings.feedback_interval_ns, TimerKind::Feedback);
        }
        let Ok(body) = feedback.encode() else {
            return;
        };
        let Ok(message) = (Frame {
            kind: KIND_VIDEO_FEEDBACK,
            payload: &body,
        })
        .encode() else {
            return;
        };
        match self.transport.clone().filter(|t| t.frames_v4()) {
            Some(transport) => {
                if transport.send(message) {
                    self.stats.feedback_sent += 1;
                } else {
                    self.stats.feedback_refused += 1;
                }
            }
            None => {
                self.stats.feedback_posted += 1;
                self.notify(cx, ClientJob::Feedback { message });
            }
        }
    }

    /// Up to 1024 missing sequences from the oldest unreported one.
    fn loss_bitmap(&mut self) -> Option<LossBitmap> {
        let base = *self.loss.front()?;
        let span = (LossBitmap::MAX_BYTES * 8) as u64;
        let mut bits = Vec::new();
        while let Some(&seq) = self.loss.front() {
            let offset = seq - base;
            if offset >= span {
                break;
            }
            self.loss.pop_front();
            let byte = (offset / 8) as usize;
            if bits.len() <= byte {
                bits.resize(byte + 1, 0);
            }
            bits[byte] |= 1 << (offset % 8);
        }
        Some(LossBitmap {
            base_sequence: base,
            bits,
        })
    }

    // -----------------------------------------------------------------------
    // Close.

    fn on_close(&mut self, cx: &Arc<ClientShared>) {
        if self.closed.is_some() {
            return;
        }
        let _ = self.flush_decoder(cx);
        self.live = false;
        self.timers.clear();
        self.armed = None;
        self.notify(
            cx,
            ClientJob::Closed {
                outcome: CloseOutcome::Closed,
            },
        );
        self.closed = Some(CloseOutcome::Closed);
        self.phase = ClientPhase::Closed;
        self.decoder = None;
        self.events = None;
        self.transport = None;
    }
}
