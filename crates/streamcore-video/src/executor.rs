//! The per-stream serial executor (3.5): a FIFO mailbox drained by one thread at
//! a time on the stream's [`SerialQueue`], the event ports native code posts
//! through, surface leases and the close slot. The rules are in
//! [`crate::threading`].

use crate::error::MediaError;
use crate::stream::{Command, Internal, StreamState};
use crate::traits::*;
use crate::types::*;
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::task::Waker;
use std::time::{Duration, Instant};
use streamcore_media::media::{MAX_FRAME_LENGTH, VIDEO_HEADER_LENGTH};
use streamcore_media::stream::VideoWorkQueue;

/// Poison-tolerant lock: every lock here is a short leaf, and a panic elsewhere
/// must not wedge teardown.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Thread markers (rule 6).

thread_local! {
    /// The executor draining on this thread (0: none).
    static CURRENT: Cell<u64> = const { Cell::new(0) };
}

static NEXT_EXECUTOR: AtomicU64 = AtomicU64::new(1);

/// The name of the transport runtime's worker threads.
pub const NETWORK_THREAD_NAME: &str = "streamcore-network";

pub(crate) struct DrainMarker {
    previous: u64,
}

impl DrainMarker {
    pub(crate) fn enter(id: u64) -> Self {
        Self {
            previous: CURRENT.replace(id),
        }
    }
}

impl Drop for DrainMarker {
    fn drop(&mut self) {
        CURRENT.set(self.previous);
    }
}

/// Whether this thread is inside a drain of the executor `id`.
pub(crate) fn is_current(id: u64) -> bool {
    CURRENT.get() == id
}

/// Whether this thread is inside any executor's drain.
pub fn on_executor() -> bool {
    CURRENT.get() != 0
}

/// Whether a StreamCore blocking entry point (`close_and_wait`) may wait here:
/// not on any executor and not on a `streamcore-network` worker.
pub fn blocking_allowed() -> bool {
    !on_executor() && std::thread::current().name() != Some(NETWORK_THREAD_NAME)
}

// ---------------------------------------------------------------------------
// Clock.

/// Host-monotonic nanoseconds for the executor's timers. Injected in tests.
pub trait Clock: Send + Sync {
    fn now_ns(&self) -> u64;
}

/// `std::time::Instant` from a process-wide anchor.
pub struct MonotonicClock {
    anchor: Instant,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self {
            anchor: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now_ns(&self) -> u64 {
        u64::try_from(self.anchor.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

// ---------------------------------------------------------------------------
// Leases.

pub(crate) enum Releaser {
    Capture(Weak<dyn CaptureBackend>),
    Converter(Weak<dyn Converter>),
}

/// Rust's hold on a native surface. Dropping it calls the producer's
/// `release(id)` exactly once, on whichever thread drops it: the executor while
/// it exists, else the posting thread (3.2).
pub(crate) struct SurfaceLease {
    id: SurfaceId,
    releaser: Releaser,
}

impl SurfaceLease {
    pub(crate) fn new(id: SurfaceId, releaser: Releaser) -> Self {
        Self { id, releaser }
    }
}

impl Drop for SurfaceLease {
    fn drop(&mut self) {
        if self.id == 0 {
            return;
        }
        match &self.releaser {
            Releaser::Capture(backend) => {
                if let Some(backend) = backend.upgrade() {
                    backend.release(self.id);
                }
            }
            Releaser::Converter(converter) => {
                if let Some(converter) = converter.upgrade() {
                    converter.release(self.id);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Events.

pub(crate) enum Event {
    Frame {
        capture: CaptureId,
        member: Option<u8>,
        lease: SurfaceLease,
        info: SurfaceInfo,
        status: FrameStatus,
    },
    OpDone {
        op: OpToken,
        error: Option<MediaError>,
    },
    SourceGone {
        capture: CaptureId,
        reason: SourceGone,
    },
    Encoded {
        encoder: u64,
        frame: EncodedFrame,
    },
    Dropped {
        encoder: u64,
        submit_seq: u64,
        reason: DropReason,
    },
    Receipt {
        encoder: u64,
        sequence: u64,
        keyframe: bool,
        config: bool,
        accepted: bool,
    },
    PermitReleased,
    Timer(TimerToken),
    Command(Command),
    Internal(Internal),
    Close {
        keepalive: Option<Arc<Shared>>,
    },
}

#[derive(Default)]
struct Mailbox {
    events: VecDeque<Event>,
    /// The executor reached `Closed`: posts are dropped on the posting thread.
    closed: bool,
}

// ---------------------------------------------------------------------------
// Close slot.

#[derive(Default)]
struct CloseState {
    outcome: Option<CloseOutcome>,
    wakers: Vec<Waker>,
}

#[derive(Default)]
pub(crate) struct CloseSlot {
    state: Mutex<CloseState>,
    done: Condvar,
}

impl CloseSlot {
    fn complete(&self, outcome: CloseOutcome) {
        let wakers = {
            let mut state = lock(&self.state);
            if state.outcome.is_some() {
                return;
            }
            state.outcome = Some(outcome);
            std::mem::take(&mut state.wakers)
        };
        self.done.notify_all();
        for waker in wakers {
            waker.wake();
        }
    }

    pub(crate) fn outcome(&self) -> Option<CloseOutcome> {
        lock(&self.state).outcome
    }

    pub(crate) fn wait(&self, timeout: Duration) -> Option<CloseOutcome> {
        let state = lock(&self.state);
        let (state, _) = self
            .done
            .wait_timeout_while(state, timeout, |state| state.outcome.is_none())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.outcome
    }

    pub(crate) fn poll(&self, waker: &Waker) -> Option<CloseOutcome> {
        let mut state = lock(&self.state);
        if state.outcome.is_none() && !state.wakers.iter().any(|w| w.will_wake(waker)) {
            state.wakers.push(waker.clone());
        }
        state.outcome
    }
}

// ---------------------------------------------------------------------------
// The executor.

pub(crate) struct Published {
    pub(crate) phase: StreamPhase,
    pub(crate) stats: StatsSnapshot,
}

pub(crate) struct Shared {
    pub(crate) id: u64,
    mailbox: Mutex<Mailbox>,
    wake_pending: AtomicBool,
    draining: AtomicBool,
    queue: Mutex<Option<Arc<dyn SerialQueue>>>,
    /// Taken out for the length of a drain, so no guard is held across a
    /// native call.
    state: Mutex<Option<Box<StreamState>>>,
    pub(crate) revoked: AtomicBool,
    pub(crate) closing: AtomicBool,
    /// For `revoke` from any thread (a leaf call into the adapter).
    out: Mutex<Option<Arc<dyn VideoOut>>>,
    pub(crate) work: VideoWorkQueue,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) published: Mutex<Published>,
    pub(crate) close_slot: CloseSlot,
    pub(crate) next_command: AtomicU64,
    /// Frames replaced in the mailbox before delivery (skip cause b).
    pub(crate) mailbox_replaced: AtomicU64,
}

impl Shared {
    pub(crate) fn new(
        queue: Arc<dyn SerialQueue>,
        out: Arc<dyn VideoOut>,
        clock: Arc<dyn Clock>,
        build: impl FnOnce(&Weak<Shared>) -> StreamState,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak: &Weak<Shared>| {
            let hook_target = weak.clone();
            let work = VideoWorkQueue::with_release_hook(Arc::new(move || {
                if let Some(shared) = hook_target.upgrade() {
                    shared.post(Event::PermitReleased);
                }
            }));
            Shared {
                id: NEXT_EXECUTOR.fetch_add(1, Ordering::Relaxed),
                mailbox: Mutex::default(),
                wake_pending: AtomicBool::new(false),
                draining: AtomicBool::new(false),
                queue: Mutex::new(Some(queue)),
                state: Mutex::new(Some(Box::new(build(weak)))),
                revoked: AtomicBool::new(false),
                closing: AtomicBool::new(false),
                out: Mutex::new(Some(out)),
                work,
                clock,
                published: Mutex::new(Published {
                    phase: StreamPhase::Idle,
                    stats: StatsSnapshot::default(),
                }),
                close_slot: CloseSlot::default(),
                next_command: AtomicU64::new(1),
                mailbox_replaced: AtomicU64::new(0),
            }
        })
    }

    pub(crate) fn revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    /// Revoke the sink's authority. A leaf call: an atomic, then the adapter's
    /// own `revoke` with no Rust lock held.
    pub(crate) fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        let out = lock(&self.out).clone();
        if let Some(out) = out {
            out.revoke();
        }
    }

    /// Enqueue and return in O(1). A newer frame for the same capture and member
    /// replaces an undelivered one. Wakes the queue after the mailbox lock is
    /// dropped. After `Closed`, the event (and any lease or permit it carries) is
    /// dropped on this thread.
    pub(crate) fn post(&self, event: Event) {
        let mut rejected = None;
        let mut displaced = None;
        {
            let mut mailbox = lock(&self.mailbox);
            if mailbox.closed {
                rejected = Some(event);
            } else if let Event::Frame {
                capture, member, ..
            } = &event
                && let Some(slot) = mailbox.events.iter_mut().find(|queued| {
                    matches!(queued, Event::Frame { capture: c, member: m, .. }
                        if c == capture && m == member)
                })
            {
                displaced = Some(std::mem::replace(slot, event));
            } else {
                mailbox.events.push_back(event);
            }
        }
        if displaced.is_some() {
            self.mailbox_replaced.fetch_add(1, Ordering::Relaxed);
        }
        // Leases release here, outside the mailbox lock.
        drop(displaced);
        if rejected.is_some() {
            drop(rejected);
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
            // A wake may have been swallowed while this drain ran.
            self.wake_pending.store(false, Ordering::Release);
            self.request_wake();
        }
    }

    /// A wake or a timer: drain what the mailbox holds now. Anything posted while
    /// this runs waits for the next wake.
    pub(crate) fn run(self: &Arc<Self>, timer: Option<TimerToken>) {
        if let Some(timer) = timer {
            let mut mailbox = lock(&self.mailbox);
            if mailbox.closed {
                return;
            }
            mailbox.events.push_back(Event::Timer(timer));
        }
        if self
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // Another drain is active; its end re-checks the mailbox.
            return;
        }
        self.wake_pending.store(false, Ordering::Release);
        let batch = std::mem::take(&mut lock(&self.mailbox).events);
        let state = lock(&self.state).take();
        match state {
            None => drop(batch),
            Some(mut state) => {
                let marker = DrainMarker::enter(self.id);
                for event in batch {
                    state.handle(self, event);
                }
                state.after_drain(self);
                if state.is_closed() {
                    self.finalize(state);
                } else {
                    *lock(&self.state) = Some(state);
                }
                drop(marker);
            }
        }
        self.end_drain();
    }

    /// A capture callback. When no drain is active and nothing is queued ahead
    /// of it, only admission, convert and submit for this frame run inline, so a
    /// capture callback reaches submit with no hop. Otherwise it is posted.
    pub(crate) fn frame(self: &Arc<Self>, event: Event) {
        if self
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.post(event);
            return;
        }
        let empty = lock(&self.mailbox).events.is_empty();
        let state = if empty {
            lock(&self.state).take()
        } else {
            None
        };
        match state {
            Some(mut state) => {
                let marker = DrainMarker::enter(self.id);
                state.handle_frame_inline(self, event);
                state.publish(self);
                *lock(&self.state) = Some(state);
                drop(marker);
            }
            None => self.post(event),
        }
        self.end_drain();
    }

    fn finalize(&self, state: Box<StreamState>) {
        let leftovers = {
            let mut mailbox = lock(&self.mailbox);
            mailbox.closed = true;
            std::mem::take(&mut mailbox.events)
        };
        drop(leftovers);
        let queue = lock(&self.queue).take();
        let out = lock(&self.out).take();
        let mut state = state;
        let keepalive = state.keepalive.take();
        let outcome = state.close_outcome().unwrap_or(CloseOutcome::Closed);
        state.publish(self);
        drop(state);
        drop((queue, out));
        self.close_slot.complete(outcome);
        drop(keepalive);
    }

    pub(crate) fn queue(&self) -> Option<Arc<dyn SerialQueue>> {
        lock(&self.queue).clone()
    }
}

// ---------------------------------------------------------------------------
// Ports.

/// What a native [`SerialQueue`] calls: `run()` for a wake, `run_timer(token)`
/// when a `wake_after` deadline passes. Create it first, give it to the queue,
/// then pass both to [`crate::StreamController::new`], which binds it.
#[derive(uniffi::Object)]
pub struct StreamPort {
    executor: Mutex<Weak<Shared>>,
    bound: AtomicBool,
}

#[uniffi::export]
impl StreamPort {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            executor: Mutex::new(Weak::new()),
            bound: AtomicBool::new(false),
        })
    }

    /// Drain the stream's mailbox. Call only on the stream's queue.
    pub fn run(&self) {
        let executor = lock(&self.executor).upgrade();
        if let Some(executor) = executor {
            executor.run(None);
        }
    }

    /// A `wake_after` deadline passed. Call only on the stream's queue.
    pub fn run_timer(&self, timer: TimerToken) {
        let executor = lock(&self.executor).upgrade();
        if let Some(executor) = executor {
            executor.run(Some(timer));
        }
    }
}

impl StreamPort {
    pub(crate) fn bind(&self, shared: &Arc<Shared>) -> Result<(), MediaError> {
        if self.bound.swap(true, Ordering::AcqRel) {
            return Err(MediaError::InvalidSettings);
        }
        *lock(&self.executor) = Arc::downgrade(shared);
        Ok(())
    }
}

/// The capture backend's event port. Holds the executor weakly.
pub(crate) struct CapturePort {
    pub(crate) executor: Weak<Shared>,
    pub(crate) backend: Weak<dyn CaptureBackend>,
}

impl CaptureEvents for CapturePort {
    fn frame(&self, id: CaptureId, member: Option<u8>, surface: SurfaceInfo, status: FrameStatus) {
        let lease = SurfaceLease::new(surface.id, Releaser::Capture(self.backend.clone()));
        match self.executor.upgrade() {
            Some(executor) => executor.frame(Event::Frame {
                capture: id,
                member,
                lease,
                info: surface,
                status,
            }),
            None => drop(lease),
        }
    }

    fn op_done(&self, op: OpToken, error: Option<MediaError>) {
        if let Some(executor) = self.executor.upgrade() {
            executor.post(Event::OpDone { op, error });
        }
    }

    fn source_gone(&self, id: CaptureId, reason: SourceGone) {
        if let Some(executor) = self.executor.upgrade() {
            executor.post(Event::SourceGone {
                capture: id,
                reason,
            });
        }
    }
}

/// One encoder's event port: outputs carry its generation.
pub(crate) struct EncoderPort {
    pub(crate) executor: Weak<Shared>,
    pub(crate) encoder: u64,
    pub(crate) sink: Arc<VideoSinkHandle>,
}

impl EncoderEvents for EncoderPort {
    fn encoded(&self, frame: EncodedFrame) {
        match self.executor.upgrade() {
            Some(executor) => executor.post(Event::Encoded {
                encoder: self.encoder,
                frame,
            }),
            None => {
                self.sink.take(frame.payload_token);
                if let Some(token) = frame.config_token {
                    self.sink.take(token);
                }
            }
        }
    }

    fn dropped(&self, submit_seq: u64, reason: DropReason) {
        if let Some(executor) = self.executor.upgrade() {
            executor.post(Event::Dropped {
                encoder: self.encoder,
                submit_seq,
                reason,
            });
        }
    }
}

/// Encoded bytes enter Rust here, once, from the encoder's output thread (a leaf
/// call). The executor takes them when it handles the matching `encoded`.
#[derive(uniffi::Object)]
pub struct VideoSinkHandle {
    payloads: Mutex<SinkPayloads>,
}

#[derive(Default)]
struct SinkPayloads {
    bytes: HashMap<u64, Vec<u8>>,
    closed: bool,
}

/// Payloads waiting for their `encoded` event: two per frame in flight, plus
/// slack for late outputs.
const SINK_CAPACITY: usize = 16;

#[uniffi::export]
impl VideoSinkHandle {
    /// Store `bytes` under `token` (unique per encoder). Returns false when the
    /// payload is empty or over the frame limit, the token is taken, too many are
    /// waiting, or the encoder was replaced; the frame is then lost.
    pub fn submit_encoded(&self, token: u64, bytes: Vec<u8>) -> bool {
        if bytes.is_empty() || bytes.len() > MAX_FRAME_LENGTH - VIDEO_HEADER_LENGTH {
            return false;
        }
        let mut payloads = lock(&self.payloads);
        if payloads.closed
            || payloads.bytes.len() >= SINK_CAPACITY
            || payloads.bytes.contains_key(&token)
        {
            return false;
        }
        payloads.bytes.insert(token, bytes);
        true
    }
}

impl VideoSinkHandle {
    pub(crate) fn new() -> Self {
        Self {
            payloads: Mutex::default(),
        }
    }

    pub(crate) fn take(&self, token: u64) -> Option<Vec<u8>> {
        lock(&self.payloads).bytes.remove(&token)
    }

    pub(crate) fn close(&self) {
        let dropped = {
            let mut payloads = lock(&self.payloads);
            payloads.closed = true;
            std::mem::take(&mut payloads.bytes)
        };
        drop(dropped);
    }
}

/// The completion of one [`VideoOut::send`]. Complete it exactly once; dropping
/// it uncompleted counts as refused. Callable from any thread (it only posts).
#[derive(uniffi::Object)]
pub struct SinkReceipt {
    executor: Weak<Shared>,
    encoder: u64,
    sequence: u64,
    keyframe: bool,
    config: bool,
    done: AtomicBool,
}

#[uniffi::export]
impl SinkReceipt {
    pub fn complete(&self, accepted: bool) {
        if self.done.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(executor) = self.executor.upgrade() {
            executor.post(Event::Receipt {
                encoder: self.encoder,
                sequence: self.sequence,
                keyframe: self.keyframe,
                config: self.config,
                accepted,
            });
        }
    }
}

impl SinkReceipt {
    pub(crate) fn new(
        executor: Weak<Shared>,
        encoder: u64,
        sequence: u64,
        keyframe: bool,
        config: bool,
    ) -> Self {
        Self {
            executor,
            encoder,
            sequence,
            keyframe,
            config,
            done: AtomicBool::new(false),
        }
    }
}

impl Drop for SinkReceipt {
    fn drop(&mut self) {
        self.complete(false);
    }
}
