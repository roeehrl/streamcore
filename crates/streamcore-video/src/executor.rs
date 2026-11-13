//! The per-stream serial executor (3.5): a FIFO mailbox drained by one thread at
//! a time on the stream's [`SerialQueue`], the event ports native code posts
//! through, surface leases and the close slot. The rules are in
//! [`crate::threading`].

use crate::error::MediaError;
use crate::native::{self, Context, NativeStream, NativeSubmit, ReleaseFn};
use crate::stream::{Command, Internal, StreamState};
use crate::traits::*;
use crate::types::*;
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::task::Waker;
use std::time::{Duration, Instant};
use streamcore_media::media::{MAX_FRAME_LENGTH, VIDEO_HEADER_LENGTH};
use streamcore_media::stream::{VideoWorkPermit, VideoWorkQueue};

/// Poison-tolerant lock: every lock here is a short leaf, and a panic elsewhere
/// must not wedge teardown.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Thread markers (rule 6).

thread_local! {
    /// The executor draining on this thread (0: none).
    static CURRENT: Cell<u64> = const { Cell::new(0) };
}

static NEXT_EXECUTOR: AtomicU64 = AtomicU64::new(1);

/// A new executor id for the drain markers (host and client streams share the
/// id space).
pub(crate) fn next_executor_id() -> u64 {
    NEXT_EXECUTOR.fetch_add(1, Ordering::Relaxed)
}

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
    /// The capture backend, and its attached C `release` if any.
    Capture(Weak<dyn CaptureBackend>, Option<(Context, ReleaseFn)>),
    Converter(Weak<dyn Converter>),
    /// A composite (3.9): releasing it may end a `PoolExhausted` wait.
    Compositor(Weak<dyn Compositor>, Weak<Shared>),
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
            Releaser::Capture(backend, native) => {
                // The upgrade keeps the backend (the native context) alive
                // for the call.
                if let Some(backend) = backend.upgrade() {
                    match native {
                        Some(native) => native::release(*native, self.id),
                        None => backend.release(self.id),
                    }
                }
            }
            Releaser::Converter(converter) => {
                if let Some(converter) = converter.upgrade() {
                    converter.release(self.id);
                }
            }
            Releaser::Compositor(compositor, shared) => {
                if let Some(compositor) = compositor.upgrade() {
                    compositor.release(self.id);
                }
                if let Some(shared) = shared.upgrade() {
                    shared.composite_released();
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
    Layout {
        capture: CaptureId,
        layout: DisplayLayout,
    },
    InputReady {
        encoder: u64,
    },
    SessionFailed {
        encoder: u64,
        error: MediaError,
        recoverable: bool,
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
    /// The sink's report (written, failed, shed, consumed).
    Sink(crate::sink::SinkEvent),
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
    pub(crate) fn complete(&self, outcome: CloseOutcome) {
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
    /// The stream's `VideoSink` (3.4): `revoke` and its state are leaf calls.
    pub(crate) sink: crate::sink::VideoSink,
    pub(crate) work: VideoWorkQueue,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) published: Mutex<Published>,
    pub(crate) close_slot: CloseSlot,
    pub(crate) next_command: AtomicU64,
    /// Frames replaced in the mailbox before delivery (skip cause b).
    pub(crate) mailbox_replaced: AtomicU64,
    /// Frames the capture backend dropped unseen (skip cause a).
    pub(crate) capture_skipped: AtomicU64,
    /// A permit was released on this executor's own drain: the drain runs
    /// `submit_due` before it ends instead of posting `PermitReleased` to
    /// itself (which cost a wake and a second drain per frame).
    pub(crate) permit_released: AtomicBool,
    /// Canvas: a compose found the pool exhausted; the next composite release
    /// posts `PoolReleased` (3.9).
    pub(crate) pool_waiting: AtomicBool,
    /// Canvas: composite surfaces Rust holds (the latest and those in flight).
    pub(crate) composites_out: AtomicUsize,
    /// The C ABI's per-frame callbacks ([`native`]); cleared at `Closed`.
    native: Mutex<NativeStream>,
    /// The open capture's event port, for the C ABI's frames.
    capture_port: Mutex<Weak<CapturePort>>,
}

impl Shared {
    pub(crate) fn new(
        queue: Arc<dyn SerialQueue>,
        sink: crate::sink::VideoSink,
        clock: Arc<dyn Clock>,
        build: impl FnOnce(&Weak<Shared>) -> StreamState,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak: &Weak<Shared>| {
            let hook_target = weak.clone();
            let work = VideoWorkQueue::with_release_hook(Arc::new(move || {
                if let Some(shared) = hook_target.upgrade() {
                    if is_current(shared.id) {
                        shared.permit_released.store(true, Ordering::Release);
                    } else {
                        shared.post(Event::PermitReleased);
                    }
                }
            }));
            Shared {
                id: next_executor_id(),
                mailbox: Mutex::default(),
                wake_pending: AtomicBool::new(false),
                draining: AtomicBool::new(false),
                queue: Mutex::new(Some(queue)),
                state: Mutex::new(Some(Box::new(build(weak)))),
                revoked: AtomicBool::new(false),
                closing: AtomicBool::new(false),
                sink,
                work,
                clock,
                published: Mutex::new(Published {
                    phase: StreamPhase::Idle,
                    stats: StatsSnapshot::default(),
                }),
                close_slot: CloseSlot::default(),
                next_command: AtomicU64::new(1),
                mailbox_replaced: AtomicU64::new(0),
                capture_skipped: AtomicU64::new(0),
                permit_released: AtomicBool::new(false),
                pool_waiting: AtomicBool::new(false),
                composites_out: AtomicUsize::new(0),
                native: Mutex::default(),
                capture_port: Mutex::new(Weak::new()),
            }
        })
    }

    pub(crate) fn revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    /// Revoke the sink's authority. A leaf call: an atomic, then the sink's own
    /// `revoke` (its lock, then the transport's).
    pub(crate) fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        self.sink.revoke();
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
        // The clone keeps the queue (the C wake's context) alive for the call;
        // `finalize` clears both under the queue lock.
        let (queue, wake) = {
            let queue = lock(&self.queue);
            (queue.clone(), lock(&self.native).wake)
        };
        if let Some(queue) = queue {
            match wake {
                Some(wake) => native::wake(wake),
                None => queue.wake(),
            }
        }
    }

    /// The attached C callbacks.
    pub(crate) fn native(&self) -> NativeStream {
        *lock(&self.native)
    }

    /// Attach the C ABI's per-frame callbacks; false once the stream is closed.
    pub(crate) fn attach(&self, callbacks: NativeStream) -> bool {
        // The queue lock orders this against `finalize`, which clears both.
        let queue = lock(&self.queue);
        if queue.is_none() {
            return false;
        }
        *lock(&self.native) = callbacks;
        true
    }

    /// A per-frame C call panicked: fail the stream (`stream.output`).
    pub(crate) fn fail_from_native(&self) {
        self.post(Event::Internal(Internal::Fail(crate::error::codes::OUTPUT)));
    }

    /// A composite lease was dropped (a leaf: atomics, then a post).
    fn composite_released(&self) {
        let _ = self
            .composites_out
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        if self.pool_waiting.swap(false, Ordering::AcqRel) {
            self.post(Event::Internal(Internal::PoolReleased));
        }
    }

    pub(crate) fn set_capture_port(&self, port: &Arc<CapturePort>) {
        *lock(&self.capture_port) = Arc::downgrade(port);
    }

    pub(crate) fn capture_port(&self) -> Option<Arc<CapturePort>> {
        lock(&self.capture_port).upgrade()
    }

    fn end_drain(&self) {
        self.draining.store(false, Ordering::Release);
        // A permit released after the drain's last check (in `after_drain` or
        // `finalize`, or an inline frame drain's refused submit) is posted, so a
        // retained frame still gets submitted.
        if self.permit_released.swap(false, Ordering::AcqRel) {
            self.post(Event::PermitReleased);
        }
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
                let drain = &mut state;
                let drained = catch_unwind(AssertUnwindSafe(move || {
                    for event in batch {
                        drain.handle(self, event);
                    }
                    while self.permit_released.swap(false, Ordering::AcqRel) {
                        drain.handle(self, Event::PermitReleased);
                    }
                    drain.after_drain(self);
                }));
                if drained.is_err() && !self.recover(&mut state) {
                    self.abandon(state);
                } else if state.is_closed() {
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
                let drain = &mut state;
                let drained = catch_unwind(AssertUnwindSafe(move || {
                    drain.handle_frame_inline(self, event);
                    drain.publish(self);
                }));
                if drained.is_err() && !self.recover(&mut state) {
                    self.abandon(state);
                } else if state.is_closed() {
                    self.finalize(state);
                } else {
                    *lock(&self.state) = Some(state);
                }
                drop(marker);
            }
            None => self.post(event),
        }
        self.end_drain();
    }

    /// A drain panicked (a backend callback, or Rust): fail the stream
    /// (`stream.output`) and close it, so the capture stops and the close
    /// completes as for any failure. False when that panics too.
    fn recover(self: &Arc<Self>, state: &mut StreamState) -> bool {
        catch_unwind(AssertUnwindSafe(|| {
            self.revoke();
            state.fail_after_panic(self);
            state.after_drain(self);
        }))
        .is_ok()
    }

    /// The stream cannot run any more: refuse every post, drop every native
    /// reference and complete the close with `StopUnknown` (native state is
    /// unknown). The capture is not stopped: nothing can be called safely.
    fn abandon(&self, state: Box<StreamState>) {
        self.revoked.store(true, Ordering::Release);
        let leftovers = {
            let mut mailbox = lock(&self.mailbox);
            mailbox.closed = true;
            std::mem::take(&mut mailbox.events)
        };
        let queue = {
            let mut queue = lock(&self.queue);
            *lock(&self.native) = NativeStream::default();
            queue.take()
        };
        *lock(&self.capture_port) = Weak::new();
        self.sink.revoke();
        {
            let mut published = lock(&self.published);
            published.phase = StreamPhase::Closed;
        }
        // Dropping the state releases its leases and permits; a panic there is
        // contained as well.
        let _ = catch_unwind(AssertUnwindSafe(move || drop((state, leftovers, queue))));
        self.close_slot.complete(CloseOutcome::StopUnknown);
    }

    fn finalize(&self, state: Box<StreamState>) {
        let leftovers = {
            let mut mailbox = lock(&self.mailbox);
            mailbox.closed = true;
            std::mem::take(&mut mailbox.events)
        };
        drop(leftovers);
        let queue = {
            let mut queue = lock(&self.queue);
            *lock(&self.native) = NativeStream::default();
            queue.take()
        };
        // The C ABI's frames are refused from now on (the caller releases), and
        // the sink enqueues nothing more.
        *lock(&self.capture_port) = Weak::new();
        self.sink.revoke();
        let mut state = state;
        let keepalive = state.keepalive.take();
        let outcome = state.close_outcome().unwrap_or(CloseOutcome::Closed);
        state.publish(self);
        drop(state);
        drop(queue);
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
/// then pass both to [`crate::StreamController::new`] (or
/// [`crate::ClientStream::new`]), which binds it.
#[derive(uniffi::Object)]
pub struct StreamPort {
    /// Set once by `bind`; read without a lock on every drain.
    executor: OnceLock<PortTarget>,
    bound: AtomicBool,
    key: u64,
}

#[uniffi::export]
impl StreamPort {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            executor: OnceLock::new(),
            bound: AtomicBool::new(false),
            key: native::register_port(weak.clone()),
        })
    }

    /// The key that names this port (and its stream) in the per-frame C ABI
    /// ([`crate::native`]): `streamcore_video_attach_stream`,
    /// `streamcore_video_frame` and `streamcore_video_run`. Never 0.
    pub fn native_key(&self) -> u64 {
        self.key
    }

    /// Drain the stream's mailbox. Call only on the stream's queue.
    pub fn run(&self) {
        self.drain(None);
    }

    /// A `wake_after` deadline passed. Call only on the stream's queue.
    pub fn run_timer(&self, timer: TimerToken) {
        self.drain(Some(timer));
    }
}

/// The executor a port is bound to: a host stream's or a client stream's.
enum PortTarget {
    Stream(Weak<Shared>),
    Client(Weak<crate::client::ClientShared>),
}

impl Drop for StreamPort {
    fn drop(&mut self) {
        native::unregister_port(self.key);
    }
}

impl StreamPort {
    /// The host stream's executor (the C ABI names only host streams).
    pub(crate) fn executor(&self) -> Option<Arc<Shared>> {
        match self.executor.get() {
            Some(PortTarget::Stream(executor)) => executor.upgrade(),
            _ => None,
        }
    }

    fn drain(&self, timer: Option<TimerToken>) {
        match self.executor.get() {
            Some(PortTarget::Stream(executor)) => {
                if let Some(executor) = executor.upgrade() {
                    executor.run(timer);
                }
            }
            Some(PortTarget::Client(executor)) => {
                if let Some(executor) = executor.upgrade() {
                    executor.run(timer);
                }
            }
            None => {}
        }
    }

    pub(crate) fn bind(&self, shared: &Arc<Shared>) -> Result<(), MediaError> {
        self.bind_target(PortTarget::Stream(Arc::downgrade(shared)))
    }

    pub(crate) fn bind_client(
        &self,
        shared: &Arc<crate::client::ClientShared>,
    ) -> Result<(), MediaError> {
        self.bind_target(PortTarget::Client(Arc::downgrade(shared)))
    }

    fn bind_target(&self, target: PortTarget) -> Result<(), MediaError> {
        if self.bound.swap(true, Ordering::AcqRel) {
            return Err(MediaError::InvalidSettings);
        }
        let _ = self.executor.set(target);
        Ok(())
    }
}

/// The capture backend's event port. Holds the executor weakly.
pub(crate) struct CapturePort {
    pub(crate) executor: Weak<Shared>,
    pub(crate) backend: Weak<dyn CaptureBackend>,
    /// The shape of the last `Complete` single-source frame of each capture
    /// (one per canvas member), which the C ABI's `streamcore_video_frame`
    /// repeats.
    pub(crate) shape: Mutex<Vec<(CaptureId, SurfaceShape)>>,
}

/// The parts of a [`SurfaceInfo`] that the C ABI's frame does not carry.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct SurfaceShape {
    kind: SurfaceKind,
    format: PixelFormat,
    width: u32,
    height: u32,
    color: ColorTags,
    rotation: Rotation,
}

impl CapturePort {
    fn deliver(
        &self,
        id: CaptureId,
        member: Option<u8>,
        surface: SurfaceInfo,
        status: FrameStatus,
    ) {
        match self.executor.upgrade() {
            Some(executor) => {
                let release = executor.native().release;
                let lease =
                    SurfaceLease::new(surface.id, Releaser::Capture(self.backend.clone(), release));
                executor.frame(Event::Frame {
                    capture: id,
                    member,
                    lease,
                    info: surface,
                    status,
                });
            }
            None => drop(SurfaceLease::new(
                surface.id,
                Releaser::Capture(self.backend.clone(), None),
            )),
        }
    }

    /// The C ABI's frame (`streamcore_video_frame`): a `Complete` image shaped
    /// like the last `Complete` `frame` of capture `id`. False, without taking
    /// the surface, when this capture has no shape yet.
    pub(crate) fn frame_compact(
        &self,
        id: CaptureId,
        surface: SurfaceId,
        capture_ns: u64,
        source_present_ns: u64,
    ) -> bool {
        let shape = lock(&self.shape)
            .iter()
            .find(|(capture, _)| *capture == id)
            .map(|(_, shape)| *shape);
        // No full frame of this capture yet: not taken.
        let Some(shape) = shape else {
            return false;
        };
        let info = SurfaceInfo {
            id: surface,
            kind: shape.kind,
            format: shape.format,
            width: shape.width,
            height: shape.height,
            color: shape.color,
            capture_ns,
            source_present_ns: (source_present_ns != 0).then_some(source_present_ns),
            dirty: None,
            rotation: shape.rotation,
            fence: GpuFence::None,
        };
        self.deliver(id, None, info, FrameStatus::Complete);
        true
    }
}

impl CaptureEvents for CapturePort {
    fn frame(&self, id: CaptureId, member: Option<u8>, surface: SurfaceInfo, status: FrameStatus) {
        if status == FrameStatus::Complete && member.is_none() {
            let shape = SurfaceShape {
                kind: surface.kind,
                format: surface.format,
                width: surface.width,
                height: surface.height,
                color: surface.color,
                rotation: surface.rotation,
            };
            let mut shapes = lock(&self.shape);
            match shapes.iter_mut().find(|(capture, _)| *capture == id) {
                Some(entry) => entry.1 = shape,
                None => {
                    // A canvas has at most 8 member captures; a backend that
                    // reuses ids replaces its entry above.
                    if shapes.len() >= 16 {
                        shapes.remove(0);
                    }
                    shapes.push((id, shape));
                }
            }
        }
        self.deliver(id, member, surface, status);
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

    fn layout_changed(&self, id: CaptureId, layout: DisplayLayout) {
        if let Some(executor) = self.executor.upgrade() {
            executor.post(Event::Layout {
                capture: id,
                layout,
            });
        }
    }

    fn skipped(&self, _id: CaptureId, _member: Option<u8>, count: u32) {
        if let Some(executor) = self.executor.upgrade() {
            executor
                .capture_skipped
                .fetch_add(u64::from(count), Ordering::Relaxed);
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
        let seq = frame.submit_seq;
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
        // A frame submitted through the C ABI: its permit is dropped once posted.
        self.sink.unhold(seq);
    }

    fn dropped(&self, submit_seq: u64, reason: DropReason) {
        if let Some(executor) = self.executor.upgrade() {
            executor.post(Event::Dropped {
                encoder: self.encoder,
                submit_seq,
                reason,
            });
        }
        self.sink.unhold(submit_seq);
    }

    fn input_ready(&self) {
        if let Some(executor) = self.executor.upgrade() {
            executor.post(Event::InputReady {
                encoder: self.encoder,
            });
        }
    }

    fn session_failed(&self, error: MediaError, recoverable: bool) {
        if let Some(executor) = self.executor.upgrade() {
            executor.post(Event::SessionFailed {
                encoder: self.encoder,
                error,
                recoverable,
            });
        }
    }
}

/// Encoded bytes enter Rust here, once, from the encoder's output thread (a leaf
/// call). The executor takes them when it handles the matching `encoded`. A
/// native encoder hands bytes and metadata over in one call instead, through
/// the C ABI (`streamcore_video_encoded` with [`VideoSinkHandle::native_key`]).
#[derive(uniffi::Object)]
pub struct VideoSinkHandle {
    payloads: Mutex<SinkPayloads>,
    /// The encoder this sink belongs to, for the C ABI's combined call.
    executor: Weak<Shared>,
    encoder: u64,
    /// This sink's key in the C ABI.
    key: u64,
    /// Payload tokens of the C ABI: the upper half of the token space, so they
    /// never meet an adapter's own `submit_encoded` tokens.
    next_native_token: AtomicU64,
    /// The encoder's C submit, if attached.
    native_submit: Mutex<Option<NativeSubmit>>,
    /// Permits of frames submitted through the C submit, kept for the encoder
    /// until the frame's output or drop is posted.
    held: Mutex<Vec<(u64, Arc<VideoWorkPermit>)>>,
}

#[derive(Default)]
struct SinkPayloads {
    bytes: HashMap<u64, Vec<u8>>,
    closed: bool,
}

/// Payloads waiting for their `encoded` event: two per frame in flight, plus
/// slack for late outputs.
const SINK_CAPACITY: usize = 16;
const NATIVE_TOKENS: u64 = 1 << 63;

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

    /// The key that names this sink (and its encoder) in the per-frame C ABI
    /// ([`crate::native`]): `streamcore_video_attach_encoder` and
    /// `streamcore_video_encoded`. Never 0; valid while the sink lives.
    pub fn native_key(&self) -> u64 {
        self.key
    }
}

impl VideoSinkHandle {
    pub(crate) fn new(executor: Weak<Shared>, encoder: u64) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            payloads: Mutex::default(),
            executor,
            encoder,
            key: native::register_sink(weak.clone()),
            next_native_token: AtomicU64::new(NATIVE_TOKENS),
            native_submit: Mutex::new(None),
            held: Mutex::new(Vec::new()),
        })
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

    pub(crate) fn executor(&self) -> Option<Arc<Shared>> {
        self.executor.upgrade()
    }

    pub(crate) fn attach(&self, submit: NativeSubmit) {
        *lock(&self.native_submit) = Some(submit);
    }

    pub(crate) fn native_submit(&self) -> Option<NativeSubmit> {
        *lock(&self.native_submit)
    }

    /// Keep `permit` for the encoder until frame `seq` is posted.
    pub(crate) fn hold(&self, seq: u64, permit: Arc<VideoWorkPermit>) {
        lock(&self.held).push((seq, permit));
    }

    /// Drop the encoder's permit of frame `seq` (posted, or not submitted).
    pub(crate) fn unhold(&self, seq: u64) {
        let permit = {
            let mut held = lock(&self.held);
            held.iter()
                .position(|(held, _)| *held == seq)
                .map(|index| held.swap_remove(index))
        };
        drop(permit);
    }

    /// The C ABI's combined `submit_encoded` + `encoded` (or `dropped` with
    /// `NoOutput` when the bytes are refused). `payload` and `config` are
    /// already copied into Rust.
    pub(crate) fn encoded_native(
        &self,
        mut frame: EncodedFrame,
        payload: Vec<u8>,
        config: Option<Vec<u8>>,
    ) -> bool {
        let seq = frame.submit_seq;
        let Some(executor) = self.executor.upgrade() else {
            self.unhold(seq);
            return false;
        };
        let payload_token = self.next_native_token.fetch_add(2, Ordering::Relaxed);
        let stored = self.submit_encoded(payload_token, payload)
            && config.is_none_or(|config| {
                let stored = self.submit_encoded(payload_token + 1, config);
                if !stored {
                    self.take(payload_token);
                }
                stored
            });
        if stored {
            frame.payload_token = payload_token;
            frame.config_token = frame.config_token.map(|_| payload_token + 1);
            executor.post(Event::Encoded {
                encoder: self.encoder,
                frame,
            });
        } else {
            executor.post(Event::Dropped {
                encoder: self.encoder,
                submit_seq: seq,
                reason: DropReason::NoOutput,
            });
        }
        self.unhold(seq);
        stored
    }
}

impl Drop for VideoSinkHandle {
    fn drop(&mut self) {
        native::unregister_sink(self.key);
    }
}
