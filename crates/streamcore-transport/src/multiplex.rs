//! Bounded ordered lanes. QUIC gets one stream per lane; TLS interleaves 16KiB
//! records. Credits are returned on application consumption, not socket receipt.
use crate::{
    channel::ChannelError,
    clock::{Clock, monotonic_now_us},
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use streamcore_media::{
    binary::{ClockPing, ClockPong, Frame},
    media::VIDEO_FLAG_EXTENSION,
    video_control::{is_video_control_kind, validate_video_control},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Mutex as AsyncMutex, Notify, oneshot, watch},
};

pub const LANES: usize = 5;
const CHUNK: usize = 16 * 1024;
const MIB: usize = 1024 * 1024;
// Includes the application's 5-byte envelope. These windows bound both queued
// receive bytes and the largest message admitted on each lane.
const WINDOWS: [usize; LANES] = [4 * MIB, 32 * MIB + 5, 2 * MIB, 8 * MIB, 16 * MIB];
const MESSAGES: usize = 64;
const MAX_PENDING: usize = 128;
const MAX_QUEUED: usize = 64 * MIB;
// Weighted round robin guarantees bulk progress while giving interactive work
// frequent opportunities. A fragment already writing cannot be preempted on TCP.
const ORDER: [usize; 16] = [0, 2, 0, 3, 0, 1, 0, 4, 0, 2, 0, 3, 0, 1, 0, 1];
const VIDEO_LANE: usize = 1;
const BULK_LANE: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
#[repr(u8)]
pub enum ChannelLane {
    Interactive,
    Video,
    Audio,
    Terminal,
    Bulk,
}
impl ChannelLane {
    fn index(self) -> usize {
        self as usize
    }
}

/// The product's lane rule for one frame (`kind`, `payload`): `Some` picks the
/// lane, `None` falls back to the StreamCore binary kinds below, and an error
/// rejects the frame. Applications use it to keep a control message on the lane
/// of the data it describes (a file offer travels with its chunks).
pub type LanePolicy = fn(u8, &[u8]) -> Result<Option<ChannelLane>, ChannelError>;

/// StreamCore's own rule: only the binary kinds pick a lane.
pub fn binary_lanes(_kind: u8, _payload: &[u8]) -> Result<Option<ChannelLane>, ChannelError> {
    Ok(None)
}

pub fn application_lane(bytes: &[u8], policy: LanePolicy) -> Result<ChannelLane, ChannelError> {
    let (frame, end) = Frame::read(bytes)
        .map_err(|_| ChannelError::InvalidConfiguration)?
        .ok_or(ChannelError::InvalidConfiguration)?;
    if end != bytes.len() {
        return Err(ChannelError::InvalidConfiguration);
    }
    if let Some(lane) = policy(frame.kind, frame.payload)? {
        return Ok(lane);
    }
    Ok(match frame.kind {
        0x10 => ChannelLane::Video,
        0x11 => ChannelLane::Audio,
        0x30 => ChannelLane::Terminal,
        0x40 => ChannelLane::Bulk,
        _ => ChannelLane::Interactive,
    })
}

/// Sender side of `frames.v4`: without it, a lane-0 video kind or a video frame
/// with the extension flag is refused; with it, lane-0 video bodies must be valid.
pub fn check_frames_version(bytes: &[u8], frames_v4: bool) -> Result<(), ChannelError> {
    let (frame, _) = Frame::read(bytes)
        .map_err(|_| ChannelError::InvalidConfiguration)?
        .ok_or(ChannelError::InvalidConfiguration)?;
    if is_video_control_kind(frame.kind) {
        if !frames_v4 {
            return Err(ChannelError::InvalidConfiguration);
        }
        validate_video_control(frame.kind, frame.payload)
            .map_err(|_| ChannelError::InvalidConfiguration)?;
    }
    if frame.kind == 0x10
        && !frames_v4
        && frame
            .payload
            .get(4)
            .is_some_and(|flags| flags & VIDEO_FLAG_EXTENSION != 0)
    {
        return Err(ChannelError::InvalidConfiguration);
    }
    Ok(())
}

#[derive(uniffi::Object)]
pub struct PendingSend {
    consumed: AsyncMutex<Option<oneshot::Receiver<Result<(), ChannelError>>>>,
    result: AsyncMutex<Option<oneshot::Receiver<Result<u64, ChannelError>>>>,
    written_us: AtomicU64,
    closed: watch::Sender<bool>,
}
#[uniffi::export]
impl PendingSend {
    /// Completion means handed to the transport, not applied by the peer.
    pub async fn written(&self) -> Result<(), ChannelError> {
        let result = self
            .result
            .lock()
            .await
            .take()
            .ok_or(ChannelError::InvalidConfiguration)?;
        let mut closed = self.closed.subscribe();
        if *closed.borrow() {
            return Err(ChannelError::Closed);
        }
        tokio::select! {
            biased;
            _ = closed.changed() => Err(ChannelError::Closed),
            result = result => {
                let at = result.unwrap_or(Err(ChannelError::Closed))?;
                self.written_us.store(at, Ordering::Relaxed);
                Ok(())
            }
        }
    }
    /// When the last fragment was handed to the transport (`clock::monotonic_now_us`),
    /// once [`Self::written`] has completed successfully.
    pub fn written_at_us(&self) -> Option<u64> {
        Some(self.written_us.load(Ordering::Relaxed)).filter(|at| *at != 0)
    }
    /// The peer's message consumer took this complete message. This is not an
    /// acknowledgement that a command was executed or a file was committed.
    pub async fn consumed(&self) -> Result<(), ChannelError> {
        let result = self
            .consumed
            .lock()
            .await
            .take()
            .ok_or(ChannelError::InvalidConfiguration)?;
        let mut closed = self.closed.subscribe();
        if *closed.borrow() {
            return Err(ChannelError::Closed);
        }
        tokio::select! { biased; _ = closed.changed() => Err(ChannelError::Closed), result = result => result.unwrap_or(Err(ChannelError::Closed)) }
    }
}
/// A lane-0 clock frame whose send time is written just before its first
/// fragment goes to the writer (`clock.v1`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stamp {
    None,
    Ping(u64),
    Pong,
}
// Byte offsets of the send time inside the enveloped frame: 5-byte envelope, then
// the typed body (protocol/BINARY.md).
const PING_SEND_AT: usize = 5 + 8;
const PONG_SEND_AT: usize = 5 + 16;
enum ClockMessage {
    Ping(ClockPing),
    Pong(ClockPong),
}
struct Outgoing {
    stamp: Stamp,
    consumed: Option<oneshot::Sender<Result<(), ChannelError>>>,
    sequence: u64,
    data: Arc<Vec<u8>>,
    offset: usize,
    done: oneshot::Sender<Result<u64, ChannelError>>,
}
struct Outstanding {
    sequence: u64,
    count: usize,
    consumed: oneshot::Sender<Result<(), ChannelError>>,
}
/// The bulk token bucket (docs/STREAMCORE-VIDEO.md 6.4 item 5). While video is
/// active (a video message queued, or one enqueued within [`VIDEO_ACTIVE_US`]),
/// bulk fragments spend tokens refilled at (capacity − video rate) × 0.8, never
/// below [`MIN_BULK_BPS`]; otherwise bulk is unlimited. A fragment may take the
/// bucket negative; the next waits until it refills to zero. The per-connection
/// message and byte limits stay as safety nets.
#[derive(Default)]
pub(crate) struct BulkBucket {
    /// Refill rate in bytes per second; zero until rate inputs are set.
    rate: u64,
    tokens: i64,
    refilled_us: u64,
    video_us: u64,
}
/// How long after the last video message video counts as active.
pub(crate) const VIDEO_ACTIVE_US: u64 = 1_000_000;
/// Bulk keeps at least this rate (bits per second) while video is active.
pub(crate) const MIN_BULK_BPS: u64 = 1_000_000;
impl BulkBucket {
    fn burst(&self) -> i64 {
        // 20 ms at the refill rate, and at least two fragments.
        i64::try_from(self.rate / 50)
            .unwrap_or(i64::MAX)
            .max(2 * CHUNK as i64)
    }
    /// Rate inputs in bits per second; a zero capacity removes them.
    pub(crate) fn set_rates(&mut self, capacity_bps: u64, video_bps: u64) {
        if capacity_bps == 0 {
            *self = Self {
                video_us: self.video_us,
                ..Self::default()
            };
            return;
        }
        let bulk_bps = (capacity_bps.saturating_sub(video_bps) / 5 * 4).max(MIN_BULK_BPS);
        let first = self.rate == 0;
        self.rate = bulk_bps / 8;
        // New inputs start with a full burst; later ones keep the current level.
        self.tokens = if first {
            self.burst()
        } else {
            self.tokens.min(self.burst())
        };
    }
    pub(crate) fn rate_bytes_per_second(&self) -> u64 {
        self.rate
    }
    fn video(&mut self, now_us: u64) {
        self.video_us = now_us.max(1);
    }
    fn active(&self, now_us: u64, video_queued: bool) -> bool {
        self.rate != 0
            && (video_queued
                || (self.video_us != 0 && now_us.saturating_sub(self.video_us) <= VIDEO_ACTIVE_US))
    }
    /// Spends `bytes` for one bulk fragment, or returns how many microseconds to
    /// wait before trying again.
    pub(crate) fn admit(
        &mut self,
        now_us: u64,
        bytes: usize,
        video_queued: bool,
    ) -> Result<(), u64> {
        if !self.active(now_us, video_queued) {
            self.tokens = self.burst();
            self.refilled_us = now_us.max(1);
            return Ok(());
        }
        // No refill before the first admission under these inputs.
        let elapsed = if self.refilled_us == 0 {
            0
        } else {
            now_us.saturating_sub(self.refilled_us)
        };
        let refill = u128::from(elapsed) * u128::from(self.rate) / 1_000_000;
        self.tokens =
            (i128::from(self.tokens) + refill as i128).min(i128::from(self.burst())) as i64;
        self.refilled_us = now_us.max(1);
        if self.tokens < 0 {
            let wait = (self.tokens.unsigned_abs() * 1_000_000).div_ceil(self.rate);
            // Video going idle also ends the limit.
            let idle = if video_queued {
                wait
            } else {
                (self.video_us + VIDEO_ACTIVE_US + 1).saturating_sub(now_us)
            };
            return Err(wait.min(idle).max(1));
        }
        self.tokens -= bytes as i64;
        Ok(())
    }
}

struct Sender {
    queues: [VecDeque<Outgoing>; LANES],
    outstanding: [VecDeque<Outstanding>; LANES],
    credits: [usize; LANES],
    sequences: [u64; LANES],
    grants: VecDeque<(usize, u64, usize)>,
    credit_streak: usize,
    count: usize,
    bytes: usize,
    cursor: usize,
    bulk: BulkBucket,
    /// Set by `next` when the bucket held back a bulk fragment: retry after this
    /// many microseconds even without a wake-up.
    bulk_retry_us: Option<u64>,
    /// Bulk payload bytes handed to the writer so far.
    bulk_bytes: u64,
}
impl Default for Sender {
    fn default() -> Self {
        Self {
            queues: std::array::from_fn(|_| VecDeque::new()),
            outstanding: std::array::from_fn(|_| VecDeque::new()),
            credits: WINDOWS,
            sequences: [0; LANES],
            grants: VecDeque::new(),
            credit_streak: 0,
            count: 0,
            bytes: 0,
            cursor: 0,
            bulk: BulkBucket::default(),
            bulk_retry_us: None,
            bulk_bytes: 0,
        }
    }
}
struct Assembly {
    sequence: u64,
    total: usize,
    data: Vec<u8>,
    first_us: u64,
}
struct Ready {
    sequence: u64,
    data: Vec<u8>,
    first_us: u64,
    complete_us: u64,
}
/// One received message and when its first and last fragments arrived
/// (`clock::monotonic_now_us`).
pub(crate) struct Received {
    pub(crate) data: Vec<u8>,
    pub(crate) first_us: u64,
    pub(crate) complete_us: u64,
}
struct Receiver {
    partial: [Option<Assembly>; LANES],
    ready: [VecDeque<Ready>; LANES],
    bytes: [usize; LANES],
    count: [usize; LANES],
    sequences: [u64; LANES],
    cursor: usize,
}
impl Default for Receiver {
    fn default() -> Self {
        Self {
            partial: std::array::from_fn(|_| None),
            ready: std::array::from_fn(|_| VecDeque::new()),
            bytes: [0; LANES],
            count: [0; LANES],
            sequences: [0; LANES],
            cursor: 0,
        }
    }
}
struct Fragment {
    sequence: u64,
    lane: usize,
    data: Arc<Vec<u8>>,
    offset: usize,
    end: usize,
}
enum Record {
    Data(Fragment),
    Credit(usize, u64, usize),
}

pub(crate) struct Multiplexer {
    sender: Mutex<Sender>,
    receiver: Mutex<Receiver>,
    wake: [Notify; LANES],
    ready: Notify,
    receive_lock: AsyncMutex<()>,
    pub(crate) closed: watch::Sender<bool>,
    pub(crate) quic: Option<quinn::Connection>,
    pub(crate) endpoint: Option<quinn::Endpoint>,
    /// Present when both peers negotiated `clock.v1`.
    pub(crate) clock: Option<Clock>,
    pub(crate) features: Features,
}

/// What a channel negotiated or was configured with when it was created.
#[derive(Clone, Copy, Default)]
pub(crate) struct Features {
    /// Both peers negotiated `clock.v1`.
    pub(crate) clock: bool,
    /// Both peers negotiated `frames.v4`: the video extension flag and the lane-0
    /// video kinds may be sent and received.
    pub(crate) frames_v4: bool,
    /// The bulk token bucket flag ([`crate::tuning::TransportTuning`]).
    pub(crate) bulk_token_bucket: bool,
}
impl Multiplexer {
    #[cfg(test)]
    pub(crate) fn new(
        quic: Option<quinn::Connection>,
        endpoint: Option<quinn::Endpoint>,
    ) -> Arc<Self> {
        Self::with_features(quic, endpoint, Features::default())
    }
    pub(crate) fn with_features(
        quic: Option<quinn::Connection>,
        endpoint: Option<quinn::Endpoint>,
        features: Features,
    ) -> Arc<Self> {
        Arc::new(Self {
            sender: Mutex::new(Sender::default()),
            receiver: Mutex::new(Receiver::default()),
            wake: std::array::from_fn(|_| Notify::new()),
            ready: Notify::new(),
            receive_lock: AsyncMutex::new(()),
            closed: watch::channel(false).0,
            quic,
            endpoint,
            clock: features.clock.then(Clock::default),
            features,
        })
    }
    pub(crate) fn close(&self) {
        self.shutdown(true);
    }
    fn shutdown(&self, discard_received: bool) {
        let already_closed = self.closed.send_replace(true);
        if !already_closed && let Some(quic) = &self.quic {
            quic.close(0_u32.into(), b"channel closed");
        }
        // Match next()'s sender -> receiver lock order. In particular a local
        // close must not clear receive accounting under a scheduled credit.
        if let Ok(mut sender) = self.sender.lock() {
            if !already_closed {
                for queue in &mut sender.queues {
                    for packet in queue.drain(..) {
                        let _ = packet.done.send(Err(ChannelError::Closed));
                    }
                }
                *sender = Sender::default();
            }
            if let Ok(mut receiver) = self.receiver.lock() {
                if discard_received {
                    *receiver = Receiver::default();
                } else if !already_closed {
                    // Deliver complete messages received before peer shutdown,
                    // including its final refusal/goodbye; discard partial data.
                    receiver.partial = std::array::from_fn(|_| None);
                }
            }
        }
    }
    fn notify(&self, lane: usize) {
        self.wake[0].notify_one();
        if lane != 0 {
            self.wake[lane].notify_one();
        }
    }
    pub(crate) fn enqueue(
        &self,
        lane: ChannelLane,
        data: Vec<u8>,
    ) -> Result<Arc<PendingSend>, ChannelError> {
        self.enqueue_stamped(lane, data, Stamp::None)
    }
    /// The bulk bucket's rate inputs in bits per second (a zero capacity removes
    /// them). They take effect only when the bucket flag is on.
    pub(crate) fn set_bulk_rates(
        &self,
        capacity_bps: u64,
        video_bps: u64,
    ) -> Result<(), ChannelError> {
        let mut s = self.sender.lock().map_err(|_| ChannelError::Closed)?;
        s.bulk.set_rates(capacity_bps, video_bps);
        drop(s);
        self.notify(BULK_LANE);
        Ok(())
    }
    /// The bulk limit in bytes per second while video is active, if the bucket
    /// flag is on and rate inputs are set.
    pub(crate) fn bulk_limit(&self) -> Option<u64> {
        let s = self.sender.lock().ok()?;
        let rate = s.bulk.rate_bytes_per_second();
        (self.features.bulk_token_bucket && rate != 0).then_some(rate)
    }
    /// Bulk payload bytes handed to the writer so far.
    pub(crate) fn bulk_bytes(&self) -> u64 {
        self.sender.lock().map_or(0, |s| s.bulk_bytes)
    }
    /// Sends a typed ping on lane 0; its send time is stamped as it is written.
    pub(crate) fn ping(&self) -> Result<(), ChannelError> {
        let clock = self
            .clock
            .as_ref()
            .ok_or(ChannelError::InvalidConfiguration)?;
        let nonce = clock.next_nonce();
        let body = ClockPing { nonce, send_us: 0 }.encode();
        let frame = Frame {
            kind: 2,
            payload: &body,
        }
        .encode()
        .map_err(|_| ChannelError::InvalidConfiguration)?;
        self.enqueue_stamped(ChannelLane::Interactive, frame, Stamp::Ping(nonce))
            .map(|_| ())
    }
    fn enqueue_stamped(
        &self,
        lane: ChannelLane,
        data: Vec<u8>,
        stamp: Stamp,
    ) -> Result<Arc<PendingSend>, ChannelError> {
        let lane = lane.index();
        if data.is_empty() || data.len() > WINDOWS[lane] {
            return Err(ChannelError::InvalidConfiguration);
        }
        let (tx, rx) = oneshot::channel();
        let (consumed, consumed_rx) = oneshot::channel();
        let mut s = self.sender.lock().map_err(|_| ChannelError::Closed)?;
        if *self.closed.borrow() {
            return Err(ChannelError::Closed);
        }
        // Reserve queue capacity for control even while bulk/video are saturated.
        let count_limit = if lane == 0 {
            MAX_PENDING
        } else {
            MAX_PENDING - 16
        };
        let byte_limit = if lane == 0 {
            MAX_QUEUED
        } else {
            MAX_QUEUED - WINDOWS[0]
        };
        if s.count >= count_limit || s.bytes + data.len() > byte_limit {
            return Err(ChannelError::Backpressure);
        }
        s.bytes += data.len();
        s.count += 1;
        if lane == VIDEO_LANE {
            s.bulk.video(monotonic_now_us());
        }
        s.queues[lane].push_back(Outgoing {
            stamp,
            consumed: Some(consumed),
            sequence: 0,
            data: Arc::new(data),
            offset: 0,
            done: tx,
        });
        drop(s);
        self.notify(lane);
        Ok(Arc::new(PendingSend {
            consumed: AsyncMutex::new(Some(consumed_rx)),
            result: AsyncMutex::new(Some(rx)),
            written_us: AtomicU64::new(0),
            closed: self.closed.clone(),
        }))
    }
    fn next(&self, only: Option<usize>) -> Result<Option<Record>, ChannelError> {
        let mut s = self.sender.lock().map_err(|_| ChannelError::Closed)?;
        if *self.closed.borrow() {
            return Err(ChannelError::Closed);
        }
        s.bulk_retry_us = None;
        if (only.is_none() || only == Some(0))
            && s.credit_streak < 8
            && let Some(record) = self.take_credit(&mut s)?
        {
            return Ok(Some(record));
        }
        for _ in 0..if only.is_some() { 1 } else { ORDER.len() } {
            let lane = only.unwrap_or_else(|| {
                let lane = ORDER[s.cursor];
                s.cursor = (s.cursor + 1) % ORDER.len();
                lane
            });
            let Some(front) = s.queues[lane].front() else {
                continue;
            };
            let (offset, length) = (front.offset, front.data.len());
            if offset == 0 && (s.credits[lane] < length || s.outstanding[lane].len() >= MESSAGES) {
                continue;
            }
            if lane == BULK_LANE && self.features.bulk_token_bucket {
                let video_queued = !s.queues[VIDEO_LANE].is_empty();
                let fragment = (length - offset).min(CHUNK);
                if let Err(wait) = s.bulk.admit(monotonic_now_us(), fragment, video_queued) {
                    s.bulk_retry_us = Some(wait);
                    continue;
                }
            }
            let front = s.queues[lane].front_mut().ok_or(ChannelError::Closed)?;
            if offset == 0 && front.stamp != Stamp::None {
                self.stamp(front)?;
            }
            let data = front.data.clone();
            let mut sequence = front.sequence;
            if offset == 0 {
                s.credits[lane] -= data.len();
                sequence = s.sequences[lane].checked_add(1).ok_or(ChannelError::Io)?;
                s.sequences[lane] = sequence;
                let consumed = s.queues[lane]
                    .front_mut()
                    .and_then(|p| p.consumed.take())
                    .ok_or(ChannelError::Io)?;
                s.outstanding[lane].push_back(Outstanding {
                    sequence,
                    count: data.len(),
                    consumed,
                });
                s.queues[lane]
                    .front_mut()
                    .ok_or(ChannelError::Closed)?
                    .sequence = sequence;
            }
            let end = (offset + CHUNK).min(data.len());
            if lane == BULK_LANE {
                s.bulk_bytes = s.bulk_bytes.saturating_add((end - offset) as u64);
            }
            s.queues[lane]
                .front_mut()
                .ok_or(ChannelError::Closed)?
                .offset = end;
            s.credit_streak = 0;
            return Ok(Some(Record::Data(Fragment {
                sequence,
                lane,
                data,
                offset,
                end,
            })));
        }
        if only.is_none() || only == Some(0) {
            return self.take_credit(&mut s);
        }
        Ok(None)
    }
    /// Writes the send time into a clock frame that has not been fragmented yet;
    /// the queue holds its only reference until then.
    fn stamp(&self, packet: &mut Outgoing) -> Result<(), ChannelError> {
        let now = monotonic_now_us();
        let data = Arc::get_mut(&mut packet.data).ok_or(ChannelError::Io)?;
        let at = match packet.stamp {
            Stamp::None => return Ok(()),
            Stamp::Ping(nonce) => {
                if let Some(clock) = &self.clock {
                    clock.sent(nonce, now);
                }
                PING_SEND_AT
            }
            Stamp::Pong => PONG_SEND_AT,
        };
        data.get_mut(at..at + 8)
            .ok_or(ChannelError::Io)?
            .copy_from_slice(&now.to_le_bytes());
        packet.stamp = Stamp::None;
        Ok(())
    }
    /// Lane 0 under `clock.v1`: validates a typed ping or pong before it is
    /// delivered. Malformed bodies fail closed.
    fn clock_message(&self, message: &[u8]) -> Result<Option<ClockMessage>, ChannelError> {
        if self.clock.is_none() || !matches!(message.first(), Some(2 | 3)) {
            return Ok(None);
        }
        let Some((frame, _)) = Frame::read(message).map_err(|_| ChannelError::Io)? else {
            return Err(ChannelError::Io);
        };
        Ok(Some(if frame.kind == 2 {
            ClockMessage::Ping(ClockPing::decode(frame.payload).map_err(|_| ChannelError::Io)?)
        } else {
            ClockMessage::Pong(ClockPong::decode(frame.payload).map_err(|_| ChannelError::Io)?)
        }))
    }
    /// Lane 0: a lane-0 video kind without `frames.v4`, or with a malformed body,
    /// fails closed.
    fn check_video_control(&self, message: &[u8]) -> Result<(), ChannelError> {
        if !message.first().is_some_and(|k| is_video_control_kind(*k)) {
            return Ok(());
        }
        if !self.features.frames_v4 {
            return Err(ChannelError::Io);
        }
        let Some((frame, _)) = Frame::read(message).map_err(|_| ChannelError::Io)? else {
            return Err(ChannelError::Io);
        };
        validate_video_control(frame.kind, frame.payload).map_err(|_| ChannelError::Io)
    }
    /// Answers a typed ping and records a pong. Both are still delivered to the
    /// application.
    fn clock_answer(&self, message: ClockMessage, received_us: u64) -> Result<(), ChannelError> {
        let Some(clock) = &self.clock else {
            return Ok(());
        };
        match message {
            ClockMessage::Ping(ping) => {
                let body = ClockPong {
                    nonce: ping.nonce,
                    receive_us: received_us,
                    send_us: received_us,
                }
                .encode()
                .map_err(|_| ChannelError::Io)?;
                let pong = Frame {
                    kind: 3,
                    payload: &body,
                }
                .encode()
                .map_err(|_| ChannelError::Io)?;
                // A full queue drops this answer; the peer's estimate tolerates
                // lost pings.
                match self.enqueue_stamped(ChannelLane::Interactive, pong, Stamp::Pong) {
                    Ok(_) | Err(ChannelError::Backpressure) => Ok(()),
                    Err(error) => Err(error),
                }
            }
            ClockMessage::Pong(pong) => {
                clock.answered(pong, received_us);
                Ok(())
            }
        }
    }
    fn take_credit(&self, s: &mut Sender) -> Result<Option<Record>, ChannelError> {
        let Some((lane, sequence, bytes)) = s.grants.pop_front() else {
            return Ok(None);
        };
        let mut r = self.receiver.lock().map_err(|_| ChannelError::Closed)?;
        r.bytes[lane] -= bytes;
        r.count[lane] -= 1;
        s.credit_streak = s.credit_streak.saturating_add(1);
        Ok(Some(Record::Credit(lane, sequence, bytes)))
    }
    fn written(&self, fragment: &Fragment) -> Result<(), ChannelError> {
        if fragment.end != fragment.data.len() {
            return Ok(());
        }
        let mut s = self.sender.lock().map_err(|_| ChannelError::Closed)?;
        let packet = s.queues[fragment.lane]
            .pop_front()
            .ok_or(ChannelError::Closed)?;
        s.count -= 1;
        s.bytes -= packet.data.len();
        let _ = packet.done.send(Ok(monotonic_now_us().max(1)));
        Ok(())
    }
    fn ingest(&self, record: &[u8], only: Option<usize>) -> Result<(), ChannelError> {
        if record.len() < 2 {
            return Err(ChannelError::Io);
        }
        let lane = usize::from(record[1]);
        if lane >= LANES {
            return Err(ChannelError::Io);
        }
        if record[0] == 1 {
            if record.len() != 14 || only.is_some_and(|n| n != 0) {
                return Err(ChannelError::Io);
            }
            let count = u32::from_be_bytes(record[10..14].try_into().map_err(|_| ChannelError::Io)?)
                as usize;
            let sequence =
                u64::from_be_bytes(record[2..10].try_into().map_err(|_| ChannelError::Io)?);
            let mut s = self.sender.lock().map_err(|_| ChannelError::Closed)?;
            let outstanding = s.outstanding[lane].pop_front().ok_or(ChannelError::Io)?;
            if outstanding.sequence != sequence || outstanding.count != count {
                return Err(ChannelError::Io);
            }
            let _ = outstanding.consumed.send(Ok(()));
            s.credits[lane] += count;
            if s.credits[lane] > WINDOWS[lane] {
                return Err(ChannelError::Io);
            }
            drop(s);
            self.notify(lane);
            return Ok(());
        }
        if record[0] != 0
            || record.len() <= 18
            || record.len() > CHUNK + 18
            || only.is_some_and(|n| n != lane)
        {
            return Err(ChannelError::Io);
        }
        let sequence = u64::from_be_bytes(record[2..10].try_into().map_err(|_| ChannelError::Io)?);
        let total =
            u32::from_be_bytes(record[10..14].try_into().map_err(|_| ChannelError::Io)?) as usize;
        let offset =
            u32::from_be_bytes(record[14..18].try_into().map_err(|_| ChannelError::Io)?) as usize;
        if total == 0
            || total > WINDOWS[lane]
            || offset > total
            || record.len() - 18 > total - offset
        {
            return Err(ChannelError::Io);
        }
        let mut r = self.receiver.lock().map_err(|_| ChannelError::Closed)?;
        if *self.closed.borrow() {
            return Err(ChannelError::Closed);
        }
        if offset == 0 {
            if r.sequences[lane].checked_add(1) != Some(sequence)
                || r.partial[lane].is_some()
                || r.count[lane] >= MESSAGES
                || r.bytes[lane] + total > WINDOWS[lane]
            {
                return Err(ChannelError::Io);
            }
            r.sequences[lane] = sequence;
            r.count[lane] += 1;
            r.bytes[lane] += total;
            r.partial[lane] = Some(Assembly {
                sequence,
                total,
                data: Vec::with_capacity(total),
                first_us: monotonic_now_us(),
            });
        }
        let assembly = r.partial[lane].as_mut().ok_or(ChannelError::Io)?;
        if assembly.sequence != sequence || assembly.total != total || assembly.data.len() != offset
        {
            return Err(ChannelError::Io);
        }
        assembly.data.extend_from_slice(&record[18..]);
        if assembly.data.len() == total {
            let received_us = monotonic_now_us();
            let complete = r.partial[lane].take().ok_or(ChannelError::Io)?;
            // A typed ping or pong is checked before delivery and answered
            // outside the receiver lock (sender -> receiver is the lock order).
            let clock = if lane == 0 {
                self.check_video_control(&complete.data)?;
                self.clock_message(&complete.data)?
            } else {
                None
            };
            r.ready[lane].push_back(Ready {
                sequence: complete.sequence,
                data: complete.data,
                first_us: complete.first_us,
                complete_us: received_us,
            });
            self.ready.notify_one();
            drop(r);
            if let Some(message) = clock {
                self.clock_answer(message, received_us)?;
            }
        }
        Ok(())
    }
    pub(crate) async fn receive(&self) -> Result<Vec<u8>, ChannelError> {
        self.receive_timed().await.map(|received| received.data)
    }
    pub(crate) async fn receive_timed(&self) -> Result<Received, ChannelError> {
        let mut closed = self.closed.subscribe();
        let _reader = self.receive_lock.lock().await;
        loop {
            let packet = {
                let mut r = self.receiver.lock().map_err(|_| ChannelError::Closed)?;
                let mut packet = None;
                for _ in 0..ORDER.len() {
                    let lane = ORDER[r.cursor];
                    r.cursor = (r.cursor + 1) % ORDER.len();
                    if let Some(ready) = r.ready[lane].pop_front() {
                        packet = Some((lane, ready));
                        break;
                    }
                }
                packet
            };
            if let Some((lane, ready)) = packet {
                let mut sender = self.sender.lock().map_err(|_| ChannelError::Closed)?;
                if !*self.closed.borrow() {
                    sender
                        .grants
                        .push_back((lane, ready.sequence, ready.data.len()));
                }
                drop(sender);
                self.notify(0);
                return Ok(Received {
                    data: ready.data,
                    first_us: ready.first_us,
                    complete_us: ready.complete_us,
                });
            }
            if *closed.borrow_and_update() {
                return Err(ChannelError::Closed);
            }
            tokio::select! { _ = closed.changed() => {}, _ = self.ready.notified() => {} }
        }
    }
    pub(crate) fn start(
        self: &Arc<Self>,
        reader: Box<dyn AsyncRead + Unpin + Send>,
        writer: Box<dyn AsyncWrite + Unpin + Send>,
        only: Option<usize>,
    ) {
        let this = self.clone();
        crate::runtime::cleanup(async move {
            let mut closed = this.closed.subscribe();
            if *closed.borrow() {
                return;
            }
            tokio::select! { biased; _ = closed.changed() => {}, _ = this.read_loop(reader, only) => {} }
            this.shutdown(false);
        });
        let this = self.clone();
        crate::runtime::cleanup(async move {
            let mut closed = this.closed.subscribe();
            if *closed.borrow() {
                return;
            }
            tokio::select! { biased; _ = closed.changed() => {}, _ = this.write_loop(writer, only) => {} }
            this.shutdown(false);
        });
    }
    async fn read_loop(
        &self,
        mut reader: Box<dyn AsyncRead + Unpin + Send>,
        only: Option<usize>,
    ) -> Result<(), ChannelError> {
        loop {
            let size = reader.read_u32().await.map_err(|_| ChannelError::Io)? as usize;
            if !(2..=CHUNK + 18).contains(&size) {
                return Err(ChannelError::Io);
            }
            let mut bytes = vec![0; size];
            reader
                .read_exact(&mut bytes)
                .await
                .map_err(|_| ChannelError::Io)?;
            self.ingest(&bytes, only)?;
        }
    }
    async fn write_loop(
        &self,
        mut writer: Box<dyn AsyncWrite + Unpin + Send>,
        only: Option<usize>,
    ) -> Result<(), ChannelError> {
        loop {
            let Some(record) = self.next(only)? else {
                let retry = self
                    .sender
                    .lock()
                    .map_err(|_| ChannelError::Closed)?
                    .bulk_retry_us;
                let wake = self.wake[only.unwrap_or(0)].notified();
                match retry {
                    Some(us) => {
                        tokio::select! {
                            _ = wake => {}
                            _ = tokio::time::sleep(std::time::Duration::from_micros(us)) => {}
                        }
                    }
                    None => wake.await,
                }
                continue;
            };
            // One buffer per record, length prefix included, written with one
            // `write_all` (single-write records): over TLS each record is then one
            // TLS record instead of a 4-byte one plus the body.
            let mut bytes = Vec::with_capacity(4 + CHUNK + 18);
            bytes.extend([0; 4]);
            match &record {
                Record::Credit(lane, sequence, count) => {
                    bytes.extend([1, *lane as u8]);
                    bytes.extend(sequence.to_be_bytes());
                    bytes.extend((*count as u32).to_be_bytes());
                }
                Record::Data(f) => {
                    bytes.extend([0, f.lane as u8]);
                    bytes.extend(f.sequence.to_be_bytes());
                    bytes.extend((f.data.len() as u32).to_be_bytes());
                    bytes.extend((f.offset as u32).to_be_bytes());
                    bytes.extend_from_slice(&f.data[f.offset..f.end]);
                }
            }
            let length = (bytes.len() - 4) as u32;
            bytes[..4].copy_from_slice(&length.to_be_bytes());
            writer
                .write_all(&bytes)
                .await
                .map_err(|_| ChannelError::Io)?;
            writer.flush().await.map_err(|_| ChannelError::Io)?;
            if let Record::Data(fragment) = record {
                self.written(&fragment)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn data(lane: u8, sequence: u64, total: u32, offset: u32, payload: &[u8]) -> Vec<u8> {
        let mut b = vec![0, lane];
        b.extend(sequence.to_be_bytes());
        b.extend(total.to_be_bytes());
        b.extend(offset.to_be_bytes());
        b.extend(payload);
        b
    }
    fn credit(lane: u8, sequence: u64, count: u32) -> Vec<u8> {
        let mut b = vec![1, lane];
        b.extend(sequence.to_be_bytes());
        b.extend(count.to_be_bytes());
        b
    }
    #[test]
    fn control_preempts_bulk_between_fragments_and_bulk_is_not_starved() {
        let mux = Multiplexer::new(None, None);
        mux.enqueue(ChannelLane::Bulk, vec![1; CHUNK * 2]).unwrap();
        let Some(Record::Data(first)) = mux.next(None).unwrap() else {
            panic!("missing bulk")
        };
        assert_eq!((first.lane, first.offset, first.end), (4, 0, CHUNK));
        mux.written(&first).unwrap();
        for _ in 0..16 {
            mux.enqueue(ChannelLane::Interactive, vec![2]).unwrap();
        }
        let Some(Record::Data(input)) = mux.next(None).unwrap() else {
            panic!("missing input")
        };
        assert_eq!(input.lane, 0);
        mux.written(&input).unwrap();
        let mut finished_bulk = false;
        for _ in 0..ORDER.len() {
            let Some(Record::Data(packet)) = mux.next(None).unwrap() else {
                break;
            };
            if packet.lane == 4 {
                assert_eq!(packet.offset, CHUNK);
                finished_bulk = true;
            }
            mux.written(&packet).unwrap();
        }
        assert!(finished_bulk);
    }
    /// Records every `poll_write` call's buffer as one write.
    struct CountingWriter(Arc<Mutex<Vec<Vec<u8>>>>);
    impl AsyncWrite for CountingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.0.lock().unwrap().push(buf.to_vec());
            std::task::Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
    #[tokio::test]
    async fn every_lane_record_is_one_write_with_its_length_prefix() {
        let mux = Multiplexer::new(None, None);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let video = mux
            .enqueue(ChannelLane::Video, vec![9; 2 * CHUNK + 100])
            .unwrap();
        mux.enqueue(ChannelLane::Interactive, vec![1; 10]).unwrap();
        let writer = mux.clone();
        let sink = CountingWriter(writes.clone());
        let task = tokio::spawn(async move { writer.write_loop(Box::new(sink), None).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), video.written())
            .await
            .unwrap()
            .unwrap();
        task.abort();
        let writes = writes.lock().unwrap().clone();
        // Three video fragments and one control message, each a single write.
        assert_eq!(writes.len(), 4);
        let mut video_bytes = 0;
        for write in &writes {
            let length = u32::from_be_bytes(write[..4].try_into().unwrap()) as usize;
            assert_eq!(length, write.len() - 4, "a record split across writes");
            assert!(length <= CHUNK + 18);
            if write[5] == 1 {
                video_bytes += length - 18;
            }
        }
        assert_eq!(video_bytes, 2 * CHUNK + 100);
    }
    #[tokio::test]
    async fn consumption_credits_are_bounded_until_the_credit_is_scheduled() {
        let mux = Multiplexer::new(None, None);
        for sequence in 1..=MESSAGES as u64 {
            mux.ingest(&data(4, sequence, 1, 0, &[7]), None).unwrap();
            assert_eq!(mux.receive().await.unwrap(), vec![7]);
        }
        assert_eq!(mux.sender.lock().unwrap().grants.len(), MESSAGES);
        assert!(
            mux.ingest(&data(4, MESSAGES as u64 + 1, 1, 0, &[7]), None)
                .is_err()
        );
        assert!(matches!(
            mux.next(None).unwrap(),
            Some(Record::Credit(4, 1, 1))
        ));
        mux.ingest(&data(4, MESSAGES as u64 + 1, 1, 0, &[7]), None)
            .unwrap();
    }
    #[tokio::test]
    async fn credit_bursts_cannot_starve_local_control_and_close_discards_pending_reads() {
        let mux = Multiplexer::new(None, None);
        for sequence in 1..=16 {
            mux.ingest(&data(4, sequence, 1, 0, &[7]), None).unwrap();
            mux.receive().await.unwrap();
        }
        mux.enqueue(ChannelLane::Interactive, vec![1]).unwrap();
        for _ in 0..8 {
            assert!(matches!(mux.next(None).unwrap(), Some(Record::Credit(..))));
        }
        let Some(Record::Data(packet)) = mux.next(None).unwrap() else {
            panic!("control starved")
        };
        assert_eq!(packet.lane, 0);
        mux.written(&packet).unwrap();
        mux.ingest(&data(0, 1, 1, 0, &[8]), None).unwrap();
        mux.close();
        assert!(mux.receive().await.is_err());
    }
    #[test]
    fn sequence_offset_lane_and_credit_validation_fail_closed() {
        let mux = Multiplexer::new(None, None);
        assert!(mux.ingest(&data(1, 1, 10, 1, &[1]), None).is_err());
        assert!(mux.ingest(&data(1, 1, 10, 0, &[1]), Some(2)).is_err());
        mux.ingest(&data(1, 1, 10, 0, &[1]), Some(1)).unwrap();
        assert!(mux.ingest(&data(1, 1, 10, 0, &[1]), Some(1)).is_err());
        assert!(mux.ingest(&data(1, 2, 10, 1, &[1]), Some(1)).is_err());
        mux.enqueue(ChannelLane::Bulk, vec![0; 1]).unwrap();
        let Some(Record::Data(packet)) = mux.next(None).unwrap() else {
            panic!()
        };
        mux.written(&packet).unwrap();
        mux.ingest(&credit(4, 1, 1), Some(0)).unwrap();
        mux.enqueue(ChannelLane::Bulk, vec![0; 1]).unwrap();
        let Some(Record::Data(packet)) = mux.next(None).unwrap() else {
            panic!()
        };
        mux.written(&packet).unwrap();
        assert!(mux.ingest(&credit(4, 1, 1), Some(0)).is_err()); // equal size, wrong message
    }
    #[test]
    fn noninteractive_admission_reserves_control_slots_and_rejects_oversize() {
        let mux = Multiplexer::new(None, None);
        for _ in 0..MAX_PENDING - 16 {
            mux.enqueue(ChannelLane::Bulk, vec![1]).unwrap();
        }
        assert!(matches!(
            mux.enqueue(ChannelLane::Bulk, vec![1]),
            Err(ChannelError::Backpressure)
        ));
        mux.enqueue(ChannelLane::Interactive, vec![1]).unwrap();
        assert!(
            mux.enqueue(ChannelLane::Audio, vec![0; WINDOWS[2] + 1])
                .is_err()
        );
        mux.close();
        assert!(mux.enqueue(ChannelLane::Interactive, vec![1]).is_err());
    }

    #[test]
    fn binary_kinds_pick_their_lane() {
        for (kind, lane) in [
            (0x10, ChannelLane::Video),
            (0x11, ChannelLane::Audio),
            (0x30, ChannelLane::Terminal),
            (0x40, ChannelLane::Bulk),
            (1, ChannelLane::Interactive),
            (0x20, ChannelLane::Interactive),
        ] {
            let bytes = Frame {
                kind,
                payload: b"{}",
            }
            .encode()
            .unwrap();
            assert_eq!(application_lane(&bytes, binary_lanes).unwrap(), lane);
        }
        let partial = Frame {
            kind: 1,
            payload: b"{}",
        }
        .encode()
        .unwrap();
        assert!(application_lane(&partial[..6], binary_lanes).is_err());
        let overridden = |_: u8, _: &[u8]| Ok(Some(ChannelLane::Bulk));
        assert_eq!(
            application_lane(&partial, overridden).unwrap(),
            ChannelLane::Bulk
        );
    }

    #[test]
    fn bulk_bucket_limits_bulk_only_while_video_is_active() {
        let mut bucket = BulkBucket::default();
        // No rate inputs: unlimited.
        assert!(bucket.admit(0, CHUNK, true).is_ok());
        // 100 Mbit/s capacity, 50 Mbit/s video: bulk gets 40 Mbit/s = 5 MB/s.
        bucket.set_rates(100_000_000, 50_000_000);
        assert_eq!(bucket.rate_bytes_per_second(), 5_000_000);
        // Video idle: unlimited.
        for _ in 0..100 {
            assert!(bucket.admit(1, CHUNK, false).is_ok());
        }
        // Video active: the burst (20 ms = 100 kB) passes, then bulk waits.
        bucket.video(10);
        let mut sent = 0;
        let wait = loop {
            match bucket.admit(10, CHUNK, false) {
                Ok(()) => sent += CHUNK,
                Err(wait) => break wait,
            }
        };
        assert!((100_000..100_000 + 2 * CHUNK).contains(&sent), "{sent}");
        // The deficit (less than one fragment) refills at 5 bytes per µs.
        assert!(wait > 0 && wait <= (CHUNK as u64).div_ceil(5), "{wait}");
        assert!(bucket.admit(10 + wait, CHUNK, false).is_ok());
        // Over one second at 5 MB/s, about 5 MB passes.
        let mut now = 10 + wait;
        let mut sent = 0;
        while now < 1_000_000 {
            match bucket.admit(now, CHUNK, true) {
                Ok(()) => sent += CHUNK,
                Err(wait) => now += wait,
            }
        }
        assert!((4_800_000..5_200_000).contains(&sent), "{sent}");
        // Video stopping lifts the limit after VIDEO_ACTIVE_US.
        assert!(bucket.admit(now + VIDEO_ACTIVE_US, CHUNK, false).is_ok());
        for _ in 0..100 {
            assert!(
                bucket
                    .admit(now + 2 * VIDEO_ACTIVE_US, CHUNK, false)
                    .is_ok()
            );
        }
        // Video above capacity still leaves the floor; zero capacity clears inputs.
        bucket.set_rates(10_000_000, 20_000_000);
        assert_eq!(bucket.rate_bytes_per_second(), MIN_BULK_BPS / 8);
        bucket.set_rates(0, 0);
        assert_eq!(bucket.rate_bytes_per_second(), 0);
    }

    #[test]
    fn bulk_bucket_is_off_unless_the_flag_is_set() {
        for enabled in [false, true] {
            let mux = Multiplexer::with_features(
                None,
                None,
                Features {
                    bulk_token_bucket: enabled,
                    ..Features::default()
                },
            );
            mux.set_bulk_rates(16_000_000, 15_000_000).unwrap();
            assert_eq!(mux.bulk_limit().is_some(), enabled);
            mux.enqueue(ChannelLane::Video, vec![1]).unwrap();
            mux.enqueue(ChannelLane::Bulk, vec![2; 16 * CHUNK]).unwrap();
            let mut bulk = 0;
            while let Some(record) = mux.next(None).unwrap() {
                if let Record::Data(f) = record {
                    bulk += usize::from(f.lane == BULK_LANE);
                    mux.written(&f).unwrap();
                }
            }
            if enabled {
                // 1 Mbit/s floor: the two-fragment burst, plus at most one more.
                assert!((2..=3).contains(&bulk), "{bulk}");
                assert!(mux.sender.lock().unwrap().bulk_retry_us.is_some());
            } else {
                assert_eq!(bulk, 16);
            }
        }
    }

    #[test]
    fn frames_v4_gates_lane_zero_kinds_and_the_extension_flag() {
        let video = |flags: u8| {
            let mut payload = vec![1, 0, 0, 0, flags];
            payload.extend([0; 24]);
            payload.extend([0, 0, 1]);
            Frame {
                kind: 0x10,
                payload: &payload,
            }
            .encode()
            .unwrap()
        };
        let nack = Frame {
            kind: 0x13,
            payload: &[0; 3],
        }
        .encode()
        .unwrap();
        assert!(check_frames_version(&video(0x81), false).is_err());
        assert!(check_frames_version(&video(0x81), true).is_ok());
        assert!(check_frames_version(&video(1), false).is_ok());
        assert!(check_frames_version(&nack, false).is_err());
        assert!(check_frames_version(&nack, true).is_err()); // malformed body
        for kind in 0x12..=0x15 {
            let bytes = Frame {
                kind,
                payload: b"{}",
            }
            .encode()
            .unwrap();
            assert_eq!(
                application_lane(&bytes, binary_lanes).unwrap(),
                ChannelLane::Interactive
            );
        }
        // Receiving a lane-0 video kind without frames.v4 fails closed.
        let mux = Multiplexer::new(None, None);
        let mut record = data(0, 1, nack.len() as u32, 0, &nack);
        assert!(mux.ingest(&record, None).is_err());
        let mux = Multiplexer::with_features(
            None,
            None,
            Features {
                frames_v4: true,
                ..Features::default()
            },
        );
        assert!(mux.ingest(&record, None).is_err()); // malformed body
        let mux = Multiplexer::with_features(
            None,
            None,
            Features {
                frames_v4: true,
                ..Features::default()
            },
        );
        let body = streamcore_media::video_control::VideoNack {
            stream_id: 1,
            generation: 1,
            sequence: 1,
            fragments: vec![0],
        }
        .encode()
        .unwrap();
        let valid = Frame {
            kind: 0x13,
            payload: &body,
        }
        .encode()
        .unwrap();
        record = data(0, 1, valid.len() as u32, 0, &valid);
        mux.ingest(&record, None).unwrap();
    }
}
