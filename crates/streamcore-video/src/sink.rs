//! `VideoSink` (STREAMCORE-VIDEO.md 3.4, 3.5, 6.2, 6.4, 9.4): encoded video
//! leaves Rust here, directly on lane 1 of a [`SecureChannel`], as kind `0x10`.
//!
//! The executor calls [`VideoSink::push`] right after `Encoded` (R1). Each frame
//! (its configuration packet, if any, and its payload) is built as one
//! contiguous enveloped message per packet, so the multiplexer writes every
//! fragment record with one write. At most one frame is in the multiplexer and
//! not yet written; later frames wait in the sink's own queue, where the 6.4
//! rules can still shed them:
//!
//! 1. a discardable frame still queued when a newer one arrives is dropped;
//! 2. reference deltas are never shed, except by the delta age bound
//!    (`max(100 ms, 3 × credit RTT)` from H5), which runs only when
//!    [`SinkPolicy::delta_age_bound`] is on **and** the plan confirms LTR or
//!    temporal layers on the encoder;
//! 3. configuration and keyframes are never shed, and a newer keyframe
//!    supersedes every older queued frame;
//! 4. a frame handed to the multiplexer is never withdrawn (a fragment being
//!    written is never pre-empted);
//! 5. while the sink carries video, it supplies the channel's bulk token bucket
//!    with rate inputs ([`SecureChannel::set_bulk_rate_inputs`]).
//!
//! When the frame in the multiplexer is written (on `streamcore-network`), the
//! continuation enqueues the next queued frame itself, without an executor hop,
//! publishes [`SinkState`] and posts the result to the executor. An idle sink
//! enqueues inside `push`, on the executor, with no hop at all.
//!
//! Locks: the sink lock is taken before any `streamcore-transport` lock, never
//! the reverse (enqueue runs inside the sink lock). A transport that completes
//! or drops a ticket on the thread that enqueues it (inside the sink lock) has
//! the completion deferred until the lock holder finishes, never re-entered.
//! `revoke` and `state` are leaf calls.

use crate::executor::{Clock, Event, Shared, lock};
use crate::types::{FrameExtension, RecoveryStep, StreamPlan};
use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use streamcore_media::media::{
    FrameTiming as WireTiming, LtrInfo as WireLtr, MAX_FRAME_LENGTH, VIDEO_FLAG_EXTENSION,
    VIDEO_HEADER_LENGTH, VideoExtensions, VideoMetadata,
};
use streamcore_media::video_control::{FrameTimingReport, KIND_FRAME_TIMING};
use streamcore_transport::channel::SecureChannel;

/// The sink's backlog, published after every change; read anywhere as a leaf
/// call ([`crate::StreamController::sink_state`]). The pacer admits a frame to
/// the encoder only while `unsent_frames` is 0 and `unconsumed_frames` is below
/// its bound (6.2); recovery waits while `backpressured`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Record)]
pub struct SinkState {
    /// Frames the sink accepted whose last fragment is not yet written to the
    /// transport: its own queue plus the frame in the multiplexer. Counted by
    /// Rust on every platform; kernel buffers are not included.
    pub unsent_frames: u32,
    /// Age of the oldest unsent frame since H5 (its `push`), in milliseconds.
    pub oldest_unsent_age_ms: u32,
    /// Frames written whose message the peer's consumer has not yet taken.
    pub unconsumed_frames: u32,
    /// Consumed-credit RTT: the windowed minimum (2 s) of written → consumed,
    /// in milliseconds. It includes the peer's consume delay on purpose. 0
    /// until the first sample.
    pub credit_rtt_ms: u32,
    /// An unsent frame is older than one frame interval, or more frames are
    /// unconsumed than `max(2, RTT × rate + 1)`.
    pub backpressured: bool,
}

/// The sink's switches, each a rollback flag (11.1). [`Default`] turns all on
/// except the delta age bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct SinkPolicy {
    /// The 6.2 bounds: one frame in the multiplexer at a time, the queue and its
    /// 6.4 shedding, and pacer admission gated on [`SinkState`]. Off: every frame
    /// is enqueued at once and nothing is shed (P1's behaviour, on lane 1).
    pub bounds: bool,
    /// 6.4 rule 1: drop a queued discardable frame when a newer one arrives.
    pub shed_discardable: bool,
    /// 6.4 rule 2's delta age bound. Effective only when the plan confirms LTR
    /// (`RecoveryStep::LtrRefresh`) or temporal layers
    /// (`FrameExtension::TemporalId`); without them every shed delta forces an
    /// IDR, so it stays off by default.
    pub delta_age_bound: bool,
    /// 6.4 rule 5: supply the channel's bulk token bucket with rate inputs. The
    /// bucket itself runs only if `TransportTuning::bulk_token_bucket` was on
    /// when the channel was created (`StatsSnapshot::bulk_bucket_inactive`).
    pub bulk_bucket: bool,
    /// Under frames.v4, add the `timing` TLV to every Nth frame (0: never).
    pub timing_every: u32,
    /// Under frames.v4, send `frameTiming` (`0x14`, H5 and H6) on lane 0 for
    /// every Nth written frame (0: never).
    pub frame_timing_every: u32,
}

impl Default for SinkPolicy {
    fn default() -> Self {
        Self {
            bounds: true,
            shed_discardable: true,
            delta_age_bound: false,
            bulk_bucket: true,
            timing_every: 30,
            frame_timing_every: 30,
        }
    }
}

// ---------------------------------------------------------------------------
// Transport.

/// What the sink sends on. Implemented for [`SecureChannel`]; tests and the
/// discard output implement it too.
#[doc(hidden)]
pub trait SinkTransport: Send + Sync {
    /// Both peers negotiated frames.v4.
    fn frames_v4(&self) -> bool;
    /// The transport runs the bulk token bucket.
    fn bulk_bucket_enabled(&self) -> bool;
    /// Rate inputs in bits per second; a zero capacity removes them.
    fn set_bulk_rate_inputs(&self, capacity_bps: u64, video_bps: u64);
    /// The transport's monotonic clock in microseconds (the clock of
    /// `capture_us`, H5 and H6 on the wire).
    fn now_us(&self) -> u64;
    /// Enqueue one complete enveloped frame on the lane its kind selects.
    /// Complete `ticket` when the message's last fragment is written and when
    /// the peer consumed it, from any thread, possibly inside this call;
    /// dropping it before `written` counts as failed. On refusal the ticket
    /// is handed back.
    fn enqueue(&self, frame: Vec<u8>, ticket: SendTicket) -> Result<(), SendTicket>;
    /// A transport-side backpressure signal (kernel or socket state), ORed
    /// into [`SinkState::backpressured`]. It never redefines `unsent_frames`.
    /// A leaf call: no lock that the sink's own paths take.
    fn backpressured(&self) -> bool {
        false
    }
}

/// One enqueued message's completions. Dropping it before [`Self::written`]
/// fails the message; dropping it after, before [`Self::consumed`], ends its
/// consumption tracking without an RTT sample.
#[doc(hidden)]
pub struct SendTicket {
    sink: Weak<Inner>,
    id: u64,
    /// 0 new, 1 written, 2 consumed or ended.
    stage: AtomicU8,
}

impl SendTicket {
    fn new(sink: Weak<Inner>, id: u64) -> Self {
        Self {
            sink,
            id,
            stage: AtomicU8::new(0),
        }
    }

    /// The message's last fragment was handed to the transport at `at_us`
    /// (the transport clock; `None` when unknown).
    pub fn written(&self, at_us: Option<u64>) {
        if self
            .stage
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.report(Completion::Written(at_us));
        }
    }

    /// The peer's consumer took the message.
    pub fn consumed(&self) {
        if self
            .stage
            .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.report(Completion::Consumed);
        }
    }

    /// Ends the ticket without reporting (a refusal its sender handles).
    fn disarm(self) {
        self.stage.store(2, Ordering::Release);
    }

    fn report(&self, completion: Completion) {
        if let Some(sink) = self.sink.upgrade() {
            sink.complete(self.id, completion);
        }
    }
}

impl Drop for SendTicket {
    fn drop(&mut self) {
        match self.stage.swap(2, Ordering::AcqRel) {
            0 => self.report(Completion::Failed),
            1 => self.report(Completion::ConsumeLost),
            _ => {}
        }
    }
}

struct ChannelTransport(Arc<SecureChannel>);

impl SinkTransport for ChannelTransport {
    fn frames_v4(&self) -> bool {
        self.0.frames_v4()
    }
    fn bulk_bucket_enabled(&self) -> bool {
        self.0.bulk_bucket_enabled()
    }
    fn set_bulk_rate_inputs(&self, capacity_bps: u64, video_bps: u64) {
        let _ = self.0.set_bulk_rate_inputs(capacity_bps, video_bps);
    }
    fn now_us(&self) -> u64 {
        streamcore_transport::clock::monotonic_now_us()
    }
    fn enqueue(&self, frame: Vec<u8>, ticket: SendTicket) -> Result<(), SendTicket> {
        let pending = match self.0.enqueue_application_frame(frame) {
            Ok(pending) => pending,
            Err(_) => return Err(ticket),
        };
        // If the runtime cannot run it, the future (and the ticket) is dropped
        // here: a failure, deferred by the sink while its lock is held.
        streamcore_transport::spawn(async move {
            if pending.written().await.is_ok() {
                ticket.written(pending.written_at_us());
                if pending.consumed().await.is_ok() {
                    ticket.consumed();
                }
            }
            drop(ticket);
        });
        Ok(())
    }
}

/// Counts every frame as written and consumed at once (benches without a link).
struct DiscardTransport;

impl SinkTransport for DiscardTransport {
    fn frames_v4(&self) -> bool {
        false
    }
    fn bulk_bucket_enabled(&self) -> bool {
        false
    }
    fn set_bulk_rate_inputs(&self, _: u64, _: u64) {}
    fn now_us(&self) -> u64 {
        streamcore_transport::clock::monotonic_now_us()
    }
    fn enqueue(&self, _frame: Vec<u8>, ticket: SendTicket) -> Result<(), SendTicket> {
        ticket.written(Some(self.now_us()));
        ticket.consumed();
        Ok(())
    }
}

/// Where a stream's encoded video goes (`StreamParts::output`): lane 1 of a
/// secure channel, through the stream's `VideoSink`.
#[derive(uniffi::Object)]
pub struct VideoOutput {
    pub(crate) transport: Arc<dyn SinkTransport>,
    pub(crate) policy: SinkPolicy,
    pub(crate) capacity_bps: Option<u64>,
    pub(crate) discard: bool,
}

#[uniffi::export]
impl VideoOutput {
    /// Video on `channel`'s lane 1. `capacity_bps` is the link capacity for the
    /// bulk bucket when the consumer knows it; else the plan's medium cap, else
    /// an unmeasured estimate (`StatsSnapshot::bulk_capacity_estimated`).
    #[uniffi::constructor]
    pub fn channel(
        channel: Arc<SecureChannel>,
        policy: SinkPolicy,
        capacity_bps: Option<u64>,
    ) -> Arc<Self> {
        Arc::new(Self {
            transport: Arc::new(ChannelTransport(channel)),
            policy,
            capacity_bps,
            discard: false,
        })
    }

    /// No link: every frame counts as written and consumed at once. For
    /// benches; its sink figures measure nothing.
    #[doc(hidden)]
    #[uniffi::constructor]
    pub fn discard(policy: SinkPolicy) -> Arc<Self> {
        Arc::new(Self {
            transport: Arc::new(DiscardTransport),
            policy,
            capacity_bps: None,
            discard: true,
        })
    }

    /// Whether this output discards (see [`Self::discard`]).
    pub fn is_discard(&self) -> bool {
        self.discard
    }
}

impl VideoOutput {
    /// A custom transport (tests).
    #[doc(hidden)]
    pub fn with_transport(
        transport: Arc<dyn SinkTransport>,
        policy: SinkPolicy,
        capacity_bps: Option<u64>,
    ) -> Arc<Self> {
        Arc::new(Self {
            transport,
            policy,
            capacity_bps,
            discard: false,
        })
    }
}

// ---------------------------------------------------------------------------
// Frames.

/// One encoded frame for the sink: its packets, already enveloped.
pub(crate) struct SinkUnit {
    /// The encoder generation (for the executor's current-encoder check).
    pub(crate) encoder: u64,
    pub(crate) generation: u64,
    pub(crate) sequence: u64,
    pub(crate) keyframe: bool,
    pub(crate) discardable: bool,
    /// The configuration message (if any), then the payload message.
    pub(crate) messages: Vec<Vec<u8>>,
}

/// Why a frame left the queue unsent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shed {
    /// Rule 1.
    Discardable,
    /// Rule 3: a newer keyframe arrived.
    Superseded,
    /// Rule 2's delta age bound.
    Aged,
}

/// What the sink reports to the executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SinkEvent {
    Written {
        encoder: u64,
        sequence: u64,
        keyframe: bool,
    },
    /// Refused or failed by the transport (a loss).
    Failed {
        encoder: u64,
        sequence: u64,
        keyframe: bool,
    },
    Shed {
        encoder: u64,
        sequence: u64,
        why: Shed,
    },
    /// The pacer was held by the sink (6.2) and the sink may admit now.
    Ready,
}

/// Builds one `0x10` message, envelope included, with one copy of `data`. The
/// extension block is added only under frames.v4 and when not empty.
pub(crate) fn video_message(
    header: VideoMetadata,
    extensions: &VideoExtensions,
    frames_v4: bool,
    data: &[u8],
) -> Option<Vec<u8>> {
    let header = header.validate().ok()?;
    let block = if frames_v4 && !extensions.is_empty() && header.flags != 2 {
        Some(extensions.encode().ok()?)
    } else {
        None
    };
    let extension_len = block.as_ref().map_or(0, |b| 2 + b.len());
    let length = VIDEO_HEADER_LENGTH + extension_len + data.len();
    if data.is_empty() || length > MAX_FRAME_LENGTH {
        return None;
    }
    let mut out = Vec::with_capacity(5 + length);
    out.push(0x10);
    out.extend(u32::try_from(length).ok()?.to_be_bytes());
    out.extend(header.stream_id.to_le_bytes());
    out.push(if block.is_some() {
        header.flags | VIDEO_FLAG_EXTENSION
    } else {
        header.flags
    });
    out.extend(header.generation.to_le_bytes());
    out.extend(header.sequence.to_le_bytes());
    out.extend(header.pts_micros.to_le_bytes());
    if let Some(block) = block {
        out.extend(u16::try_from(block.len()).ok()?.to_le_bytes());
        out.extend(block);
    }
    out.extend_from_slice(data);
    Some(out)
}

/// The frames.v4 fields of one frame, before the plan's selection.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FrameFields {
    /// H0, H1, H3, H4 (host ns).
    pub(crate) h0_ns: Option<u64>,
    pub(crate) h1_ns: u64,
    pub(crate) h3_ns: Option<u64>,
    pub(crate) h4_ns: Option<u64>,
    /// H0 on the transport clock.
    pub(crate) capture_us: Option<u64>,
    pub(crate) discardable: bool,
    pub(crate) temporal_id: Option<u8>,
    pub(crate) ltr: Option<crate::types::LtrInfo>,
    pub(crate) layout_epoch: u64,
}

fn offset_us(from: u64, to: Option<u64>) -> u32 {
    to.and_then(|to| to.checked_sub(from))
        .map_or(WireTiming::NOT_MEASURED, |ns| {
            u32::try_from(ns / 1_000)
                .unwrap_or(WireTiming::NOT_MEASURED - 1)
                .min(WireTiming::NOT_MEASURED - 1)
        })
}

/// The extension TLVs `plan` selects for one frame (9.4). `timing` is sampled.
pub(crate) fn extensions(
    plan: &[FrameExtension],
    fields: &FrameFields,
    with_timing: bool,
) -> VideoExtensions {
    let mut ext = VideoExtensions::default();
    for kind in plan {
        match kind {
            FrameExtension::Timing if with_timing => {
                if let Some(h0) = fields.h0_ns {
                    let h1 = i64::try_from(fields.h1_ns).unwrap_or(i64::MAX)
                        - i64::try_from(h0).unwrap_or(i64::MAX);
                    ext.timing = Some(WireTiming {
                        h2_us: WireTiming::NOT_MEASURED,
                        h3_us: offset_us(h0, fields.h3_ns),
                        h4_us: offset_us(h0, fields.h4_ns),
                        h1_minus_h0_us: i32::try_from(h1 / 1_000).unwrap_or(if h1 < 0 {
                            i32::MIN
                        } else {
                            i32::MAX
                        }),
                    });
                }
            }
            FrameExtension::Timing => {}
            FrameExtension::CaptureUs => ext.capture_us = fields.capture_us,
            FrameExtension::Discardable => ext.discardable = fields.discardable,
            FrameExtension::TemporalId => ext.temporal_id = fields.temporal_id.unwrap_or(0),
            FrameExtension::Ltr => {
                ext.ltr = fields.ltr.filter(|ltr| ltr.token != 0).map(|ltr| WireLtr {
                    token: ltr.token,
                    flags: ltr.flags & (WireLtr::MARK | WireLtr::REFRESH),
                });
            }
            FrameExtension::Content => {
                ext.layout_epoch = u32::try_from(fields.layout_epoch).ok();
            }
        }
    }
    ext
}

// ---------------------------------------------------------------------------
// The sink.

thread_local! {
    /// This thread holds a sink lock: completions are deferred, not re-entered.
    static IN_SINK: Cell<bool> = const { Cell::new(false) };
}

#[derive(Debug, Clone, Copy)]
enum Completion {
    Written(Option<u64>),
    Consumed,
    Failed,
    ConsumeLost,
}

struct Queued {
    id: u64,
    unit: SinkUnit,
    h5_ns: u64,
    h5_us: u64,
    bytes: usize,
}

struct Transit {
    id: u64,
    encoder: u64,
    generation: u64,
    sequence: u64,
    keyframe: bool,
    h5_ns: u64,
    h5_us: u64,
    enqueued_ns: u64,
    bytes: usize,
    /// Nothing else of this sink was in the multiplexer when it was enqueued.
    idle: bool,
}

struct Unconsumed {
    id: u64,
    written_ns: u64,
}

const RTT_WINDOW_NS: u64 = 2_000_000_000;
const RATE_WINDOW_NS: u64 = 1_000_000_000;
const CAPACITY_WINDOW_NS: u64 = 2_000_000_000;
const RATE_UPDATE_NS: u64 = 250_000_000;
/// Writes shorter than this say nothing about the link.
const MIN_WRITE_NS: u64 = 200_000;
/// The capacity estimate's ceiling.
const MAX_CAPACITY_BPS: u64 = 10_000_000_000;

#[derive(Default)]
struct Core {
    revoked: bool,
    next_id: u64,
    queue: VecDeque<Queued>,
    transit: VecDeque<Transit>,
    unconsumed: VecDeque<Unconsumed>,
    /// (when, written → consumed) samples of the RTT window.
    rtt: VecDeque<(u64, u64)>,
    pushed: VecDeque<(u64, usize)>,
    capacity: VecDeque<(u64, u64)>,
    written_frames: u64,
    events: Vec<SinkEvent>,
    stats: SinkCounters,
}

/// The sink's counters, read into `StatsSnapshot` on the executor.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SinkCounters {
    pub(crate) messages: u64,
    pub(crate) discardable_shed: u64,
    pub(crate) superseded: u64,
    pub(crate) aged_shed: u64,
    pub(crate) refused: u64,
    pub(crate) unsent_max: u32,
    pub(crate) frame_timing_sent: u64,
    pub(crate) video_bps: u64,
    pub(crate) capacity_bps: u64,
    pub(crate) capacity_estimated: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct Snapshot {
    unsent: u32,
    oldest_h5_ns: Option<u64>,
    unconsumed: u32,
    rtt_ns: Option<u64>,
}

struct Inner {
    transport: Arc<dyn SinkTransport>,
    clock: Arc<dyn Clock>,
    executor: OnceLock<Weak<Shared>>,
    policy: SinkPolicy,
    capacity_hint: Option<u64>,
    frames_v4: bool,
    bucket_enabled: bool,
    age_bound: bool,
    stream_id: u32,
    fps: AtomicU32,
    /// The pacer found the gate closed: the next write or consumption posts
    /// `Ready`. Otherwise an accepted delta posts nothing (one wake per frame).
    waiting: AtomicBool,
    core: Mutex<Core>,
    /// Completions reported while this thread held the core lock.
    deferred: Mutex<Vec<(u64, Completion)>>,
    snapshot: Mutex<Snapshot>,
    rates: Mutex<Option<(u64, u64, u64)>>,
    /// This sink, for its tickets. Frame messages have even ids; configuration
    /// and `frameTiming` messages odd ones.
    me: Weak<Inner>,
}

/// The stream's sink. See the module documentation.
pub(crate) struct VideoSink {
    inner: Arc<Inner>,
}

impl VideoSink {
    pub(crate) fn new(
        output: &VideoOutput,
        clock: Arc<dyn Clock>,
        stream_id: u32,
        plan: &StreamPlan,
    ) -> Self {
        let confirmed = plan.recovery.contains(&RecoveryStep::LtrRefresh)
            || plan.extensions.contains(&FrameExtension::TemporalId);
        Self {
            inner: Arc::new_cyclic(|me| Inner {
                frames_v4: output.transport.frames_v4(),
                bucket_enabled: output.transport.bulk_bucket_enabled(),
                transport: output.transport.clone(),
                clock,
                executor: OnceLock::new(),
                policy: output.policy,
                capacity_hint: output.capacity_bps.or(plan.medium_cap_bps),
                age_bound: output.policy.delta_age_bound && confirmed,
                stream_id,
                fps: AtomicU32::new(plan.fps.max(1)),
                waiting: AtomicBool::new(false),
                core: Mutex::default(),
                deferred: Mutex::default(),
                snapshot: Mutex::default(),
                rates: Mutex::new(None),
                me: me.clone(),
            }),
        }
    }

    pub(crate) fn bind(&self, executor: Weak<Shared>) {
        let _ = self.inner.executor.set(executor);
    }

    pub(crate) fn policy(&self) -> SinkPolicy {
        self.inner.policy
    }

    pub(crate) fn frames_v4(&self) -> bool {
        self.inner.frames_v4
    }

    /// The bulk bucket is wanted but the channel does not run it.
    pub(crate) fn bucket_inactive(&self) -> bool {
        self.inner.policy.bulk_bucket && !self.inner.bucket_enabled
    }

    /// The delta age bound is in effect.
    pub(crate) fn age_bound_active(&self) -> bool {
        self.inner.age_bound
    }

    pub(crate) fn now_us(&self) -> u64 {
        self.inner.transport.now_us()
    }

    pub(crate) fn set_fps(&self, fps: u32) {
        self.inner.fps.store(fps.max(1), Ordering::Relaxed);
    }

    pub(crate) fn counters(&self) -> SinkCounters {
        lock(&self.inner.core).stats
    }

    /// The published state. A leaf call.
    pub(crate) fn state(&self) -> SinkState {
        self.inner.state()
    }

    /// The pacer's gate (6.2): nothing unsent and fewer unconsumed frames than
    /// the bound. Always open with the bounds off.
    pub(crate) fn admits(&self) -> bool {
        if !self.inner.policy.bounds {
            return true;
        }
        // Announce the wait before reading: a completion publishes its snapshot
        // before it checks `waiting`, so one of the two sees the other.
        self.inner.waiting.store(true, Ordering::SeqCst);
        let snapshot = *lock(&self.inner.snapshot);
        let open = snapshot.unsent == 0
            && snapshot.unconsumed < self.inner.unconsumed_bound(snapshot.rtt_ns);
        if open {
            self.inner.waiting.store(false, Ordering::SeqCst);
        }
        open
    }

    /// On the executor, right after `Encoded` (R1). Returns what happened to
    /// queued frames (and to this one, if refused at once).
    pub(crate) fn push(&self, unit: SinkUnit) -> Vec<SinkEvent> {
        let inner = &self.inner;
        let now = inner.clock.now_ns();
        let h5_us = inner.transport.now_us();
        inner.with_core(|core| {
            if core.revoked {
                core.events.push(SinkEvent::Failed {
                    encoder: unit.encoder,
                    sequence: unit.sequence,
                    keyframe: unit.keyframe,
                });
                return;
            }
            let bytes = unit.messages.iter().map(Vec::len).sum();
            core.pushed.push_back((now, bytes));
            core.next_id += 2;
            let id = core.next_id;
            if inner.policy.bounds {
                inner.shed(core, &unit, now);
            }
            core.queue.push_back(Queued {
                id,
                unit,
                h5_ns: now,
                h5_us,
                bytes,
            });
            inner.pump(core, now);
        })
    }

    /// Stop enqueuing for good: queued frames are dropped. Once this returns,
    /// nothing more is enqueued. A leaf call (any thread).
    pub(crate) fn revoke(&self) {
        let inner = &self.inner;
        inner.with_core(|core| {
            core.revoked = true;
            core.queue.clear();
            core.events.clear();
        });
        if inner.policy.bulk_bucket && inner.bucket_enabled {
            inner.transport.set_bulk_rate_inputs(0, 0);
        }
    }
}

impl Inner {
    fn interval_ns(&self) -> u64 {
        1_000_000_000 / u64::from(self.fps.load(Ordering::Relaxed).max(1))
    }

    /// `max(2 frames, RTT × rate + 1 frame)`.
    fn unconsumed_bound(&self, rtt_ns: Option<u64>) -> u32 {
        let interval = self.interval_ns();
        let in_flight = rtt_ns.map_or(0, |rtt| rtt.div_ceil(interval));
        u32::try_from(in_flight + 1).unwrap_or(u32::MAX).max(2)
    }

    fn state(&self) -> SinkState {
        let snapshot = *lock(&self.snapshot);
        let now = self.clock.now_ns();
        let age_ns = snapshot.oldest_h5_ns.map_or(0, |h5| now.saturating_sub(h5));
        let backpressured = (snapshot.unsent > 0 && age_ns > self.interval_ns())
            || snapshot.unconsumed > self.unconsumed_bound(snapshot.rtt_ns)
            || self.transport.backpressured();
        SinkState {
            unsent_frames: snapshot.unsent,
            oldest_unsent_age_ms: u32::try_from(age_ns / 1_000_000).unwrap_or(u32::MAX),
            unconsumed_frames: snapshot.unconsumed,
            credit_rtt_ms: snapshot.rtt_ns.map_or(0, |rtt| {
                u32::try_from(rtt.div_ceil(1_000_000)).unwrap_or(u32::MAX)
            }),
            backpressured,
        }
    }

    /// Runs `f` under the core lock, then applies the completions this thread
    /// deferred meanwhile, publishes the snapshot, and returns the events.
    fn with_core(&self, f: impl FnOnce(&mut Core)) -> Vec<SinkEvent> {
        let nested = IN_SINK.with(|flag| flag.replace(true));
        debug_assert!(!nested, "sink lock re-entered");
        let (events, snapshot, rates) = {
            let mut core = lock(&self.core);
            f(&mut core);
            loop {
                let deferred = std::mem::take(&mut *lock(&self.deferred));
                if deferred.is_empty() {
                    break;
                }
                for (id, completion) in deferred {
                    self.apply(&mut core, id, completion);
                }
            }
            let now = self.clock.now_ns();
            let snapshot = Snapshot {
                unsent: u32::try_from(core.queue.len() + core.transit.len()).unwrap_or(u32::MAX),
                oldest_h5_ns: core
                    .transit
                    .iter()
                    .map(|t| t.h5_ns)
                    .chain(core.queue.iter().map(|q| q.h5_ns))
                    .min(),
                unconsumed: u32::try_from(core.unconsumed.len()).unwrap_or(u32::MAX),
                rtt_ns: core.rtt.iter().map(|(_, rtt)| *rtt).min(),
            };
            core.stats.unsent_max = core.stats.unsent_max.max(snapshot.unsent);
            let rates = self.rates(&mut core, now);
            (std::mem::take(&mut core.events), snapshot, rates)
        };
        IN_SINK.with(|flag| flag.set(nested));
        *lock(&self.snapshot) = snapshot;
        if let Some((capacity, video)) = rates {
            self.transport.set_bulk_rate_inputs(capacity, video);
        }
        events
    }

    /// A ticket's completion: applied now, or deferred while this thread holds
    /// the core lock. Results are posted to the executor.
    fn complete(&self, id: u64, completion: Completion) {
        if IN_SINK.with(Cell::get) {
            lock(&self.deferred).push((id, completion));
            return;
        }
        let mut events = self.with_core(|core| self.apply(core, id, completion));
        if self.waiting.swap(false, Ordering::SeqCst) {
            events.push(SinkEvent::Ready);
        }
        self.post(events);
    }

    fn post(&self, events: Vec<SinkEvent>) {
        if events.is_empty() {
            return;
        }
        let Some(executor) = self.executor.get().and_then(Weak::upgrade) else {
            return;
        };
        for event in events {
            executor.post(Event::Sink(event));
        }
    }

    /// 6.4 rules 1–3 on the queue before `unit` joins it.
    fn shed(&self, core: &mut Core, unit: &SinkUnit, now: u64) {
        let mut shed = Vec::new();
        if unit.keyframe {
            // Rule 3: a newer keyframe supersedes every older queued frame.
            for queued in core.queue.drain(..) {
                shed.push((queued.unit.encoder, queued.unit.sequence, Shed::Superseded));
            }
        } else {
            let rtt = core.rtt.iter().map(|(_, rtt)| *rtt).min().unwrap_or(0);
            let bound = (3 * rtt).max(100_000_000);
            core.queue.retain(|queued| {
                let q = &queued.unit;
                // Rule 1.
                if self.policy.shed_discardable && q.discardable {
                    shed.push((q.encoder, q.sequence, Shed::Discardable));
                    return false;
                }
                // Rule 2's hook: only with confirmed LTR or temporal layers.
                if self.age_bound && !q.keyframe && now.saturating_sub(queued.h5_ns) > bound {
                    shed.push((q.encoder, q.sequence, Shed::Aged));
                    return false;
                }
                true
            });
        }
        for (encoder, sequence, why) in shed {
            match why {
                Shed::Discardable => core.stats.discardable_shed += 1,
                Shed::Superseded => core.stats.superseded += 1,
                Shed::Aged => core.stats.aged_shed += 1,
            }
            core.events.push(SinkEvent::Shed {
                encoder,
                sequence,
                why,
            });
        }
    }

    /// Enqueue queued frames while the multiplexer holds none of this sink's
    /// (bounds on), or all of them (bounds off).
    fn pump(&self, core: &mut Core, now: u64) {
        while !core.revoked && (!self.policy.bounds || core.transit.is_empty()) {
            let Some(queued) = core.queue.pop_front() else {
                return;
            };
            let idle = core.transit.is_empty();
            let Queued {
                id,
                unit,
                h5_ns,
                h5_us,
                bytes,
            } = queued;
            let count = unit.messages.len();
            let mut refused = false;
            for (index, message) in unit.messages.into_iter().enumerate() {
                // The payload (last) message carries the frame's ticket; a
                // configuration message's ticket only reports refusal.
                let ticket_id = if index + 1 == count { id } else { id + 1 };
                let ticket = SendTicket::new(self.me.clone(), ticket_id);
                core.stats.messages += 1;
                if let Err(ticket) = self.transport.enqueue(message, ticket) {
                    // Refused at once: the frame is lost; nothing else of it
                    // is enqueued.
                    ticket.disarm();
                    refused = true;
                    break;
                }
            }
            if refused {
                core.stats.refused += 1;
                core.events.push(SinkEvent::Failed {
                    encoder: unit.encoder,
                    sequence: unit.sequence,
                    keyframe: unit.keyframe,
                });
                continue;
            }
            core.transit.push_back(Transit {
                id,
                encoder: unit.encoder,
                generation: unit.generation,
                sequence: unit.sequence,
                keyframe: unit.keyframe,
                h5_ns,
                h5_us,
                enqueued_ns: now,
                bytes,
                idle,
            });
        }
    }

    fn apply(&self, core: &mut Core, id: u64, completion: Completion) {
        let now = self.clock.now_ns();
        if id % 2 == 1 {
            // A configuration or frameTiming message: only a failure matters,
            // and its frame's own ticket reports that too.
            return;
        }
        match completion {
            Completion::Written(at_us) => {
                let Some(index) = core.transit.iter().position(|t| t.id == id) else {
                    return;
                };
                let Some(transit) = core.transit.remove(index) else {
                    return;
                };
                core.written_frames += 1;
                core.unconsumed.push_back(Unconsumed {
                    id,
                    written_ns: now,
                });
                let took = now.saturating_sub(transit.enqueued_ns);
                if transit.idle && took >= MIN_WRITE_NS {
                    let bps = (transit.bytes as u128 * 8 * 1_000_000_000 / u128::from(took))
                        .min(u128::from(MAX_CAPACITY_BPS)) as u64;
                    core.capacity.push_back((now, bps));
                }
                // Only a keyframe matters to the executor (it can end a
                // recovery); a written delta posts nothing.
                if transit.keyframe {
                    core.events.push(SinkEvent::Written {
                        encoder: transit.encoder,
                        sequence: transit.sequence,
                        keyframe: true,
                    });
                }
                self.frame_timing(core, &transit, at_us);
                self.pump(core, now);
            }
            Completion::Failed => {
                let Some(index) = core.transit.iter().position(|t| t.id == id) else {
                    return;
                };
                let Some(transit) = core.transit.remove(index) else {
                    return;
                };
                core.stats.refused += 1;
                core.events.push(SinkEvent::Failed {
                    encoder: transit.encoder,
                    sequence: transit.sequence,
                    keyframe: transit.keyframe,
                });
                self.pump(core, now);
            }
            Completion::Consumed | Completion::ConsumeLost => {
                let Some(index) = core.unconsumed.iter().position(|u| u.id == id) else {
                    return;
                };
                let Some(entry) = core.unconsumed.remove(index) else {
                    return;
                };
                if matches!(completion, Completion::Consumed) {
                    core.rtt
                        .push_back((now, now.saturating_sub(entry.written_ns).max(1)));
                    while core
                        .rtt
                        .front()
                        .is_some_and(|(at, _)| now.saturating_sub(*at) > RTT_WINDOW_NS)
                    {
                        core.rtt.pop_front();
                    }
                }
            }
        }
    }

    /// `frameTiming` (`0x14`) for every Nth written frame, under frames.v4.
    fn frame_timing(&self, core: &mut Core, transit: &Transit, at_us: Option<u64>) {
        let every = u64::from(self.policy.frame_timing_every);
        if !self.frames_v4 || every == 0 || core.written_frames % every != 0 || core.revoked {
            return;
        }
        let h6_us = at_us
            .unwrap_or_else(|| self.transport.now_us())
            .max(transit.h5_us);
        let backpressured = core
            .queue
            .iter()
            .any(|q| self.clock.now_ns().saturating_sub(q.h5_ns) > self.interval_ns());
        let Ok(body) = (FrameTimingReport {
            stream_id: self.stream_id,
            generation: transit.generation,
            sequence: transit.sequence,
            h5_us: transit.h5_us,
            h6_us,
            backpressured,
        })
        .encode() else {
            return;
        };
        let mut message = Vec::with_capacity(5 + body.len());
        message.push(KIND_FRAME_TIMING);
        message.extend((body.len() as u32).to_be_bytes());
        message.extend(body);
        core.next_id += 2;
        let ticket = SendTicket::new(self.me.clone(), core.next_id + 1);
        match self.transport.enqueue(message, ticket) {
            Ok(()) => core.stats.frame_timing_sent += 1,
            Err(ticket) => ticket.disarm(),
        }
    }

    /// The bulk bucket's rate inputs, when due: at most every 250 ms, and only
    /// when either moved by more than 10 %.
    fn rates(&self, core: &mut Core, now: u64) -> Option<(u64, u64)> {
        while core
            .pushed
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) > RATE_WINDOW_NS)
        {
            core.pushed.pop_front();
        }
        while core
            .capacity
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) > CAPACITY_WINDOW_NS)
        {
            core.capacity.pop_front();
        }
        let video = core
            .pushed
            .iter()
            .map(|(_, bytes)| *bytes as u64)
            .sum::<u64>()
            * 8;
        let (capacity, estimated) = match self.capacity_hint {
            Some(hint) => (hint, false),
            None => (
                core.capacity.iter().map(|(_, bps)| *bps).max().unwrap_or(0),
                true,
            ),
        };
        // Unmeasured estimate: never below twice the video rate, so a low
        // sample cannot squeeze bulk to the transport's floor.
        let capacity = if estimated {
            capacity.max(video.saturating_mul(2))
        } else {
            capacity
        };
        core.stats.video_bps = video;
        core.stats.capacity_bps = capacity;
        core.stats.capacity_estimated = estimated;
        if !self.policy.bulk_bucket || !self.bucket_enabled || core.revoked || capacity == 0 {
            return None;
        }
        let mut last = lock(&self.rates);
        let moved = |old: u64, new: u64| old.abs_diff(new) * 10 > old.max(1);
        let due = match *last {
            None => true,
            Some((at, old_capacity, old_video)) => {
                now.saturating_sub(at) >= RATE_UPDATE_NS
                    && (moved(old_capacity, capacity) || moved(old_video, video))
            }
        };
        if !due {
            return None;
        }
        *last = Some((now, capacity, video));
        Some((capacity, video))
    }
}
