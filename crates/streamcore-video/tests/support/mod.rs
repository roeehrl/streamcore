//! Mock backends for the executor tests. Every fake records what the executor
//! called, counts leases and permits, and notes any outbound call made after
//! the observer saw `Closed`.
#![allow(dead_code)]

use std::collections::{BinaryHeap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};
use streamcore_media::media::{ExtendedVideoPacket, FrameVersion, VideoExtensions, VideoMetadata};
use streamcore_media::stream::VideoWorkPermit;
use streamcore_video::native;
use streamcore_video::*;

pub const CAPTURE: CaptureId = 7;
pub const SECOND: u64 = 1_000_000_000;
pub const MS: u64 = 1_000_000;

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Shared log.

#[derive(Default)]
pub struct Log {
    /// Set when the observer receives `Closed`.
    pub closed: AtomicBool,
    /// Outbound calls (not `release`) seen after `Closed`.
    pub after_close: AtomicUsize,
    /// Outbound calls made outside a drain.
    pub off_executor: AtomicUsize,
    /// Frames inside encoders right now (submitted, output not yet posted),
    /// across every encoder: bounded by the 3 permits.
    pub native_permits: AtomicUsize,
    pub peak_native_permits: AtomicUsize,
    /// A call into a backend while another backend call of this stream is
    /// running (the executor must be serial).
    pub in_call: AtomicUsize,
    pub overlapping_calls: AtomicUsize,
}

impl Log {
    pub fn outbound(&self) -> CallGuard<'_> {
        if self.closed.load(Ordering::SeqCst) {
            self.after_close.fetch_add(1, Ordering::SeqCst);
        }
        if !on_executor() {
            self.off_executor.fetch_add(1, Ordering::SeqCst);
        }
        CallGuard { log: self }
    }
}

pub struct CallGuard<'a> {
    log: &'a Log,
}

impl Drop for CallGuard<'_> {
    fn drop(&mut self) {}
}

// ---------------------------------------------------------------------------
// Clock and queues.

#[derive(Default)]
pub struct ManualClock(pub AtomicU64);

impl ManualClock {
    pub fn advance(&self, ns: u64) {
        self.0.fetch_add(ns, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ns(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A deterministic queue: the test decides when drains and timers run.
pub struct ManualQueue {
    pub port: Arc<StreamPort>,
    pub clock: Arc<ManualClock>,
    pub wakes: AtomicUsize,
    pub timers: Mutex<Vec<(u64, TimerToken)>>,
    /// Run the drain inside `wake` (a queue that breaks "never inline").
    pub inline_wake: AtomicBool,
}

impl ManualQueue {
    pub fn new(port: Arc<StreamPort>, clock: Arc<ManualClock>) -> Arc<Self> {
        Arc::new(Self {
            port,
            clock,
            wakes: AtomicUsize::new(0),
            timers: Mutex::new(Vec::new()),
            inline_wake: AtomicBool::new(false),
        })
    }

    /// Run drains until no wake is pending.
    pub fn pump(&self) {
        for _ in 0..1_000 {
            if self.wakes.swap(0, Ordering::SeqCst) == 0 {
                return;
            }
            self.port.run();
        }
        panic!("the executor keeps waking itself");
    }

    /// Advance the clock in steps, firing due timers and pumping.
    pub fn advance(&self, ns: u64) {
        let step = 10 * MS;
        let mut left = ns;
        self.pump();
        while left > 0 {
            let d = left.min(step);
            self.clock.advance(d);
            left -= d;
            let now = self.clock.now_ns();
            let due: Vec<_> = {
                let mut timers = lock(&self.timers);
                let due = timers
                    .iter()
                    .filter(|(at, _)| *at <= now)
                    .copied()
                    .collect();
                timers.retain(|(at, _)| *at > now);
                due
            };
            for (_, token) in due {
                self.port.run_timer(token);
            }
            self.pump();
        }
    }
}

impl SerialQueue for ManualQueue {
    fn wake(&self) {
        if self.inline_wake.load(Ordering::SeqCst) {
            self.port.run();
        } else {
            self.wakes.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn wake_after(&self, delay_ns: u64, timer: TimerToken) {
        lock(&self.timers).push((self.clock.now_ns() + delay_ns, timer));
    }
}

enum Job {
    Wake,
    Timer(Instant, TimerToken),
    Run(Box<dyn FnOnce() + Send>),
    Quit,
}

#[derive(PartialEq, Eq)]
struct Due(Instant, TimerToken);
impl PartialOrd for Due {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Due {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.0.cmp(&self.0).then(other.1.cmp(&self.1))
    }
}

/// A real serial queue: one worker thread, like a serial `DispatchQueue`.
pub struct ThreadQueue {
    tx: Mutex<mpsc::Sender<Job>>,
    pub thread: std::thread::ThreadId,
}

impl ThreadQueue {
    pub fn new(port: Arc<StreamPort>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<Job>();
        let (id_tx, id_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("test-stream-queue".into())
            .spawn(move || {
                id_tx.send(std::thread::current().id()).ok();
                let mut timers = BinaryHeap::<Due>::new();
                loop {
                    let job = match timers.peek() {
                        Some(Due(at, _)) => {
                            let wait = at.saturating_duration_since(Instant::now());
                            match rx.recv_timeout(wait) {
                                Ok(job) => Some(job),
                                Err(mpsc::RecvTimeoutError::Timeout) => None,
                                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                            }
                        }
                        None => match rx.recv() {
                            Ok(job) => Some(job),
                            Err(_) => return,
                        },
                    };
                    match job {
                        Some(Job::Wake) => port.run(),
                        Some(Job::Timer(at, token)) => timers.push(Due(at, token)),
                        Some(Job::Run(f)) => f(),
                        Some(Job::Quit) => return,
                        None => {}
                    }
                    while timers
                        .peek()
                        .is_some_and(|Due(at, _)| *at <= Instant::now())
                    {
                        if let Some(Due(_, token)) = timers.pop() {
                            port.run_timer(token);
                        }
                    }
                }
            })
            .expect("queue thread");
        let thread = id_rx.recv().expect("queue thread id");
        Arc::new(Self {
            tx: Mutex::new(tx),
            thread,
        })
    }

    /// Run `f` on the queue (how a capture backend delivers its callbacks).
    pub fn run_on(&self, f: impl FnOnce() + Send + 'static) {
        lock(&self.tx).send(Job::Run(Box::new(f))).ok();
    }

    /// Wait until every job queued so far has run.
    pub fn flush(&self) {
        let (tx, rx) = mpsc::channel();
        self.run_on(move || {
            tx.send(()).ok();
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("queue flush");
    }

    pub fn quit(&self) {
        lock(&self.tx).send(Job::Quit).ok();
    }
}

impl SerialQueue for ThreadQueue {
    fn wake(&self) {
        lock(&self.tx).send(Job::Wake).ok();
    }
    fn wake_after(&self, delay_ns: u64, timer: TimerToken) {
        let at = Instant::now() + Duration::from_nanos(delay_ns);
        lock(&self.tx).send(Job::Timer(at, timer)).ok();
    }
}

// ---------------------------------------------------------------------------
// Capture.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Start,
    Update,
    Stop,
}

pub struct FakeCapture {
    pub log: Arc<Log>,
    pub events: Mutex<Option<Arc<dyn CaptureEvents>>>,
    pub ops: Mutex<Vec<(Op, OpToken)>>,
    /// Answer `op_done` from inside the call (still posted by the executor).
    pub auto_answer: AtomicBool,
    pub live: Mutex<HashSet<SurfaceId>>,
    pub released: AtomicUsize,
    pub double_release: AtomicUsize,
    next_surface: AtomicU64,
    pub opens: AtomicUsize,
    /// Emit one frame from inside `start` (a reentrant backend).
    pub frame_on_start: AtomicBool,
    pub opened: Mutex<Option<CaptureSpec>>,
    /// `CaptureCaps::region_in_surface`.
    pub region_in_surface: AtomicBool,
    /// The thread of every `caps`, `open`, `start`, `update` and `stop` call.
    pub op_threads: Mutex<Vec<std::thread::ThreadId>>,
}

impl FakeCapture {
    pub fn new(log: Arc<Log>) -> Arc<Self> {
        Arc::new(Self {
            log,
            events: Mutex::new(None),
            ops: Mutex::new(Vec::new()),
            auto_answer: AtomicBool::new(false),
            live: Mutex::new(HashSet::new()),
            released: AtomicUsize::new(0),
            double_release: AtomicUsize::new(0),
            next_surface: AtomicU64::new(1),
            opens: AtomicUsize::new(0),
            frame_on_start: AtomicBool::new(false),
            opened: Mutex::new(None),
            region_in_surface: AtomicBool::new(true),
            op_threads: Mutex::new(Vec::new()),
        })
    }

    pub fn surface(&self, color: ColorTags, capture_ns: u64) -> SurfaceInfo {
        self.surface_as(PixelFormat::Nv12, color, capture_ns)
    }

    pub fn surface_as(
        &self,
        format: PixelFormat,
        color: ColorTags,
        capture_ns: u64,
    ) -> SurfaceInfo {
        let id = self.next_surface.fetch_add(1, Ordering::SeqCst);
        lock(&self.live).insert(id);
        SurfaceInfo {
            id,
            kind: SurfaceKind::IoSurface,
            format,
            width: 64,
            height: 64,
            color,
            capture_ns,
            source_present_ns: None,
            dirty: None,
            rotation: Rotation::None,
            fence: GpuFence::None,
        }
    }

    /// Deliver a frame, as the capture callback does.
    pub fn emit(&self, color: ColorTags, capture_ns: u64) -> SurfaceId {
        self.emit_as(PixelFormat::Nv12, color, capture_ns)
    }

    pub fn emit_as(&self, format: PixelFormat, color: ColorTags, capture_ns: u64) -> SurfaceId {
        let surface = self.surface_as(format, color, capture_ns);
        let id = surface.id;
        let events = lock(&self.events).clone();
        if let Some(events) = events {
            events.frame(CAPTURE, None, surface, FrameStatus::Complete);
        } else {
            self.release(id);
        }
        id
    }

    pub fn last_op(&self, op: Op) -> Option<OpToken> {
        lock(&self.ops)
            .iter()
            .rev()
            .find(|(o, _)| *o == op)
            .map(|(_, t)| *t)
    }

    pub fn count(&self, op: Op) -> usize {
        lock(&self.ops).iter().filter(|(o, _)| *o == op).count()
    }

    pub fn answer(&self, op: Op, error: Option<MediaError>) {
        let token = self.last_op(op).expect("no such op");
        let events = lock(&self.events).clone().expect("not open");
        events.op_done(token, error);
    }

    pub fn leases_outstanding(&self) -> usize {
        lock(&self.live).len()
    }

    /// Deliver a prepared surface, as the capture callback does.
    pub fn emit_info(&self, surface: SurfaceInfo) {
        let id = surface.id;
        let events = lock(&self.events).clone();
        if let Some(events) = events {
            events.frame(CAPTURE, None, surface, FrameStatus::Complete);
        } else {
            self.release(id);
        }
    }

    fn note_thread(&self) {
        lock(&self.op_threads).push(std::thread::current().id());
    }

    fn record(&self, op: Op, token: OpToken) {
        let _call = self.log.outbound();
        self.note_thread();
        lock(&self.ops).push((op, token));
        if self.auto_answer.load(Ordering::SeqCst) {
            let events = lock(&self.events).clone();
            if let Some(events) = events {
                events.op_done(token, None);
            }
        }
    }
}

impl CaptureBackend for FakeCapture {
    fn caps(&self) -> CaptureCaps {
        self.note_thread();
        CaptureCaps {
            target_kinds: vec![CaptureTargetKind::Display],
            window_fit: false,
            live_region_move: true,
            cursor_modes: vec![CursorMode::Composited],
            surface_kinds: vec![SurfaceKind::IoSurface],
            pool_depth: 5,
            virtual_displays: false,
            region_in_surface: self.region_in_surface.load(Ordering::SeqCst),
        }
    }
    fn enumerate(&self) -> Result<SourceCatalog, MediaError> {
        Ok(SourceCatalog {
            displays: vec![],
            windows: vec![],
        })
    }
    fn observe_layout(&self, _display: u32) -> Option<DisplayLayout> {
        None
    }
    fn open(
        &self,
        spec: CaptureSpec,
        _queue: Arc<dyn SerialQueue>,
        events: Arc<dyn CaptureEvents>,
    ) -> Result<CaptureId, MediaError> {
        let _call = self.log.outbound();
        self.note_thread();
        self.opens.fetch_add(1, Ordering::SeqCst);
        *lock(&self.opened) = Some(spec);
        *lock(&self.events) = Some(events);
        Ok(CAPTURE)
    }
    fn start(&self, _id: CaptureId, op: OpToken) {
        self.record(Op::Start, op);
        if self.frame_on_start.load(Ordering::SeqCst) {
            self.emit(srgb(), 1);
        }
    }
    fn update(&self, _id: CaptureId, _update: CaptureUpdate, op: OpToken) {
        self.record(Op::Update, op);
    }
    fn stop(&self, _id: CaptureId, op: OpToken) {
        self.record(Op::Stop, op);
    }
    fn release(&self, surface: SurfaceId) {
        if lock(&self.live).remove(&surface) {
            self.released.fetch_add(1, Ordering::SeqCst);
        } else {
            self.double_release.fetch_add(1, Ordering::SeqCst);
        }
    }
}

// ---------------------------------------------------------------------------
// Encoder.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidateMode {
    /// Return at once; pending outputs stay with the test.
    Quick,
    /// The platform call never returns: the adapter waits with `run_bounded`
    /// past the stream's bound and reports `Completed` (the executor's elapsed
    /// backstop must catch it).
    Stuck,
    /// The platform call never returns: the adapter waits with `run_bounded`
    /// for this bound, below the stream's, and reports `TimedOut` (as the
    /// shipped adapters do, at 450 ms).
    TimesOut(Duration),
}

pub struct Pending {
    pub seq: u64,
    pub opts: SubmitOpts,
    permit: Option<Arc<VideoWorkPermit>>,
}

pub struct FakeEncoder {
    pub index: usize,
    pub spec: EncoderSpec,
    pub events: Arc<dyn EncoderEvents>,
    pub log: Arc<Log>,
    pub pending: Mutex<Vec<Pending>>,
    pub submits: Mutex<Vec<SubmitOpts>>,
    pub invalidated: AtomicUsize,
    /// Emit each output from inside `submit` (a reentrant encoder).
    pub sync_output: AtomicBool,
    pub busy_next: AtomicBool,
    pub fail_next: Mutex<Option<MediaError>>,
    pub invalidate_mode: Mutex<InvalidateMode>,
    pub unstick: Arc<(Mutex<bool>, Condvar)>,
    pub codec: Codec,
    pub rates: Mutex<Vec<RateUpdate>>,
    next_token: AtomicU64,
    /// Called inside `submit` (to post commands mid-drain).
    pub on_submit: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Behave as an LTR encoder (VideoToolbox's model): every output marks
    /// LTR token = its sequence; a refresh submit outputs a delta predicted
    /// from the newest acknowledged token.
    pub ltr: AtomicBool,
    /// Answer refresh submits with a plain delta (an encoder that cannot).
    pub ignore_refresh: AtomicBool,
    /// The newest `ltr_acknowledged` seen.
    pub acked: Mutex<Option<u32>>,
    /// Size each output like a rate-controlled encoder: the current bitrate
    /// (the spec's, then the last `set_rate`) over the time since the previous
    /// frame, a keyframe four times that.
    pub honour_rate: AtomicBool,
    /// Moves the reported H3 and H4 (normally 10 and 20 ns after the
    /// submitted time) by this many ns: a negative shift puts them before H0.
    pub timing_shift_ns: AtomicI64,
    last_pts: AtomicU64,
}

impl FakeEncoder {
    pub fn pending_seqs(&self) -> Vec<u64> {
        lock(&self.pending).iter().map(|p| p.seq).collect()
    }

    fn take(&self, seq: u64) -> Option<Pending> {
        let mut pending = lock(&self.pending);
        let index = pending.iter().position(|p| p.seq == seq)?;
        self.log.native_permits.fetch_sub(1, Ordering::SeqCst);
        Some(pending.remove(index))
    }

    fn drop_permit(&self, mut pending: Pending) {
        pending.permit.take();
    }

    /// Produce the output for `seq` from an "output thread": bytes first, then
    /// the event, then drop the permit.
    pub fn complete(&self, seq: u64) -> bool {
        self.complete_as(seq, BitstreamForm::Avcc4)
    }

    /// The encoder can take input again (after `Busy`).
    pub fn ready(&self) {
        self.events.input_ready();
    }

    pub fn complete_as(&self, seq: u64, form: BitstreamForm) -> bool {
        self.complete_with(seq, form, false, None)
    }

    /// A discardable (temporal layer) output, as a layered encoder makes.
    pub fn complete_discardable(&self, seq: u64) -> bool {
        self.complete_with(seq, BitstreamForm::Avcc4, true, Some(1))
    }

    pub fn complete_with(
        &self,
        seq: u64,
        form: BitstreamForm,
        discardable: bool,
        temporal_id: Option<u8>,
    ) -> bool {
        let Some(pending) = self.take(seq) else {
            return false;
        };
        let keyframe = pending.opts.force_keyframe;
        let ltr = if self.ltr.load(Ordering::SeqCst) {
            let acked = *lock(&self.acked);
            match acked {
                Some(token)
                    if pending.opts.ltr_refresh
                        && !keyframe
                        && !self.ignore_refresh.load(Ordering::SeqCst) =>
                {
                    Some(LtrInfo {
                        token,
                        flags: LtrInfo::REFRESH,
                    })
                }
                _ => Some(LtrInfo {
                    token: u32::try_from(seq).unwrap_or(u32::MAX),
                    flags: LtrInfo::MARK,
                }),
            }
        } else {
            None
        };
        let payload_token = self.next_token.fetch_add(1, Ordering::SeqCst);
        let mut payload = seq.to_le_bytes().to_vec();
        payload.push(u8::from(keyframe));
        if self.honour_rate.load(Ordering::SeqCst) {
            let bitrate = lock(&self.rates)
                .last()
                .map_or(self.spec.bitrate_bps, |rate| rate.bitrate_bps);
            let previous = self.last_pts.swap(pending.opts.pts_ns, Ordering::SeqCst);
            let dt = if previous == 0 {
                1_000_000_000 / u64::from(self.spec.fps.max(1))
            } else {
                pending.opts.pts_ns.saturating_sub(previous)
            }
            .clamp(8_000_000, 100_000_000);
            let mut bytes = (u128::from(bitrate) * u128::from(dt) / 8_000_000_000) as usize;
            if keyframe {
                bytes *= 4;
            }
            payload.resize(bytes.max(payload.len()), 0);
        }
        self.spec.sink.submit_encoded(payload_token, payload);
        let config_token = keyframe.then(|| {
            let token = self.next_token.fetch_add(1, Ordering::SeqCst);
            // Byte-identical across sessions: a new session must still report
            // config_changed.
            self.spec
                .sink
                .submit_encoded(token, b"avcC-config".to_vec());
            token
        });
        self.events.encoded(EncodedFrame {
            submit_seq: seq,
            pts_ns: pending.opts.pts_ns,
            keyframe,
            config_token,
            payload_token,
            form,
            discardable,
            temporal_id,
            ltr,
            timings: {
                let shift = self.timing_shift_ns.load(Ordering::SeqCst);
                let at = |ns: i64| Some(pending.opts.capture_ns.saturating_add_signed(ns + shift));
                EncodeTimings {
                    input_ns: at(10),
                    output_ns: at(20),
                }
            },
        });
        self.drop_permit(pending);
        true
    }

    pub fn drop_output(&self, seq: u64, reason: DropReason) -> bool {
        let Some(pending) = self.take(seq) else {
            return false;
        };
        self.events.dropped(seq, reason);
        self.drop_permit(pending);
        true
    }

    /// Report a drop for a sequence this encoder no longer tracks.
    pub fn report_drop(&self, seq: u64, reason: DropReason) {
        self.events.dropped(seq, reason);
    }

    pub fn complete_all(&self) {
        for seq in self.pending_seqs() {
            self.complete(seq);
        }
    }
}

impl Encoder for FakeEncoder {
    fn info(&self) -> EncoderInfo {
        EncoderInfo {
            codec: self.codec,
            name: "fake".into(),
            hardware: true,
            low_latency: true,
        }
    }
    fn submit(
        &self,
        _frame: SurfaceId,
        opts: SubmitOpts,
        permit: Arc<VideoWorkPermit>,
    ) -> SubmitResult {
        self.accept(opts, Some(permit))
    }
    fn set_rate(&self, update: RateUpdate) -> Result<(), MediaError> {
        let _call = self.log.outbound();
        lock(&self.rates).push(update);
        Ok(())
    }
    fn invalidate(&self) -> InvalidateOutcome {
        self.invalidate_now()
    }
}

impl FakeEncoder {
    /// `submit`; through the C ABI the stream keeps the permit (`None`).
    fn accept(&self, opts: SubmitOpts, permit: Option<Arc<VideoWorkPermit>>) -> SubmitResult {
        let _call = self.log.outbound();
        if let Some(hook) = lock(&self.on_submit).as_ref() {
            hook();
        }
        if self.busy_next.swap(false, Ordering::SeqCst) {
            return SubmitResult::Busy;
        }
        if let Some(error) = lock(&self.fail_next).take() {
            return SubmitResult::Failed { error };
        }
        lock(&self.submits).push(opts);
        if let Some(token) = opts.ltr_acknowledged {
            *lock(&self.acked) = Some(token);
        }
        let held = self.log.native_permits.fetch_add(1, Ordering::SeqCst) + 1;
        self.log
            .peak_native_permits
            .fetch_max(held, Ordering::SeqCst);
        lock(&self.pending).push(Pending {
            seq: opts.submit_seq,
            opts,
            permit,
        });
        if self.sync_output.load(Ordering::SeqCst) {
            self.complete(opts.submit_seq);
        }
        SubmitResult::Submitted
    }

    fn invalidate_now(&self) -> InvalidateOutcome {
        let _call = self.log.outbound();
        self.invalidated.fetch_add(1, Ordering::SeqCst);
        let (bound, report) = match *lock(&self.invalidate_mode) {
            InvalidateMode::Quick => return InvalidateOutcome::Completed,
            InvalidateMode::Stuck => (Duration::from_millis(500), false),
            InvalidateMode::TimesOut(bound) => (bound, true),
        };
        let unstick = self.unstick.clone();
        let finished = run_bounded(bound, move || {
            let (stuck, cv) = &*unstick;
            let mut released = lock(stuck);
            while !*released {
                released = cv.wait(released).unwrap_or_else(|e| e.into_inner());
            }
        });
        if finished || !report {
            InvalidateOutcome::Completed
        } else {
            InvalidateOutcome::TimedOut
        }
    }

    /// The session dies with nothing submitted (`EncoderEvents::session_failed`).
    pub fn fail_session(&self, error: MediaError, recoverable: bool) {
        self.events.session_failed(error, recoverable);
    }

    /// Let a `Stuck` or `TimesOut` platform call return.
    pub fn unstick(&self) {
        let (stuck, cv) = &*self.unstick;
        *lock(stuck) = true;
        cv.notify_all();
    }
}

pub struct FakeFactory {
    pub log: Arc<Log>,
    pub encoders: Mutex<Vec<Arc<FakeEncoder>>>,
    pub creates: AtomicUsize,
    /// Codecs whose creation fails as Unsupported.
    pub unsupported: Mutex<Vec<Codec>>,
    pub sync_output: AtomicBool,
    pub invalidate_mode: Mutex<InvalidateMode>,
    /// `create` blocks this long (a slow MFT activation).
    pub create_delay: Mutex<Duration>,
    /// `create` runs on the executor's helper thread, not on the executor.
    pub create_on_executor: AtomicUsize,
    /// Attach each encoder's C submit (the per-frame C ABI).
    pub native: AtomicBool,
    /// New encoders size outputs by their rate (`FakeEncoder::honour_rate`).
    pub honour_rate: AtomicBool,
}

impl FakeFactory {
    pub fn new(log: Arc<Log>) -> Arc<Self> {
        Arc::new(Self {
            log,
            encoders: Mutex::new(Vec::new()),
            creates: AtomicUsize::new(0),
            unsupported: Mutex::new(Vec::new()),
            sync_output: AtomicBool::new(false),
            invalidate_mode: Mutex::new(InvalidateMode::Quick),
            create_delay: Mutex::new(Duration::ZERO),
            create_on_executor: AtomicUsize::new(0),
            native: AtomicBool::new(false),
            honour_rate: AtomicBool::new(false),
        })
    }
    pub fn encoder(&self, index: usize) -> Arc<FakeEncoder> {
        lock(&self.encoders)[index].clone()
    }
    pub fn last(&self) -> Arc<FakeEncoder> {
        lock(&self.encoders).last().cloned().expect("no encoder")
    }
    pub fn count(&self) -> usize {
        lock(&self.encoders).len()
    }
}

impl EncoderFactory for FakeFactory {
    fn probe(&self, _adapter: Option<u64>) -> Vec<EncoderCaps> {
        vec![]
    }
    fn create(
        &self,
        spec: EncoderSpec,
        events: Arc<dyn EncoderEvents>,
    ) -> Result<Arc<dyn Encoder>, MediaError> {
        if self.log.closed.load(Ordering::SeqCst) {
            self.log.after_close.fetch_add(1, Ordering::SeqCst);
        }
        if on_executor() {
            self.create_on_executor.fetch_add(1, Ordering::SeqCst);
        }
        let delay = *lock(&self.create_delay);
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        self.creates.fetch_add(1, Ordering::SeqCst);
        if lock(&self.unsupported).contains(&spec.codec) {
            return Err(MediaError::Unsupported);
        }
        let mut encoders = lock(&self.encoders);
        let ltr = spec.ltr;
        let encoder = Arc::new(FakeEncoder {
            index: encoders.len(),
            codec: spec.codec,
            spec,
            events,
            log: self.log.clone(),
            pending: Mutex::new(Vec::new()),
            submits: Mutex::new(Vec::new()),
            invalidated: AtomicUsize::new(0),
            sync_output: AtomicBool::new(self.sync_output.load(Ordering::SeqCst)),
            busy_next: AtomicBool::new(false),
            fail_next: Mutex::new(None),
            invalidate_mode: Mutex::new(*lock(&self.invalidate_mode)),
            unstick: Arc::new((Mutex::new(false), Condvar::new())),
            rates: Mutex::new(Vec::new()),
            next_token: AtomicU64::new(1),
            on_submit: Mutex::new(None),
            ltr: AtomicBool::new(ltr),
            ignore_refresh: AtomicBool::new(false),
            acked: Mutex::new(None),
            honour_rate: AtomicBool::new(self.honour_rate.load(Ordering::SeqCst)),
            timing_shift_ns: AtomicI64::new(0),
            last_pts: AtomicU64::new(0),
        });
        encoders.push(encoder.clone());
        if self.native.load(Ordering::SeqCst) {
            assert!(native::streamcore_video_attach_encoder(
                encoder.spec.sink.native_key(),
                Arc::as_ptr(&encoder).cast_mut().cast(),
                Some(glue::submit),
            ));
        }
        Ok(encoder)
    }
}

// ---------------------------------------------------------------------------
// Converter.

/// A GPU converter: records each job and leases its output surfaces.
pub struct FakeConverter {
    pub log: Arc<Log>,
    pub jobs: Mutex<Vec<(SurfaceId, ConvertJob)>>,
    pub live: Mutex<HashSet<SurfaceId>>,
    pub double_release: AtomicUsize,
    next: AtomicU64,
}

impl FakeConverter {
    pub fn new(log: Arc<Log>) -> Arc<Self> {
        Arc::new(Self {
            log,
            jobs: Mutex::new(Vec::new()),
            live: Mutex::new(HashSet::new()),
            double_release: AtomicUsize::new(0),
            next: AtomicU64::new(1_000_000),
        })
    }
}

impl Converter for FakeConverter {
    fn convert(&self, input: SurfaceId, job: ConvertJob) -> Result<SurfaceInfo, MediaError> {
        let _call = self.log.outbound();
        lock(&self.jobs).push((input, job));
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        lock(&self.live).insert(id);
        Ok(SurfaceInfo {
            id,
            kind: SurfaceKind::IoSurface,
            format: job.format,
            width: job.width,
            height: job.height,
            color: job.color,
            capture_ns: 0,
            source_present_ns: None,
            dirty: None,
            rotation: Rotation::None,
            fence: GpuFence::None,
        })
    }
    fn release(&self, surface: SurfaceId) {
        if !lock(&self.live).remove(&surface) {
            self.double_release.fetch_add(1, Ordering::SeqCst);
        }
    }
}

// ---------------------------------------------------------------------------
// Sink and observer.

/// What the sink sent for one packet, decoded from its wire message.
#[derive(Clone, Copy, Debug)]
pub struct PacketMeta {
    pub header: VideoMetadata,
    pub keyframe: bool,
    pub config: bool,
    /// Not a keyframe, not a configuration.
    pub droppable: bool,
    /// The frames.v4 extension block (empty under v3).
    pub extensions: VideoExtensions,
}

#[derive(Clone, Debug)]
pub struct Packet {
    pub meta: PacketMeta,
    /// The packet's data (payload or avcC).
    pub bytes: Vec<u8>,
    /// The whole enveloped message as enqueued.
    pub message: Vec<u8>,
    /// Enqueued on the stream's executor (inside `push`), not by a write
    /// completion.
    pub on_executor: bool,
}

#[derive(Default)]
pub struct OutState {
    pub packets: Vec<Packet>,
    /// Payload messages' tickets, held for the test; configuration tickets
    /// are completed at once.
    pub receipts: Vec<(VideoMetadata, SendTicket)>,
    /// Other lane-0 messages (frameTiming).
    pub control: Vec<Vec<u8>>,
    pub rates: Vec<(u64, u64)>,
}

/// The sink's transport (lane 1 of a channel) as a fake: it decodes every
/// message, completes configuration at once and holds payload tickets until
/// the test finishes them (written, then consumed unless `hold_consumed`).
pub struct FakeOut {
    pub log: Arc<Log>,
    pub state: Mutex<OutState>,
    /// The transport-side backpressure signal.
    pub pressured: AtomicBool,
    /// Complete payload tickets at once (written and consumed).
    pub sync_receipts: AtomicBool,
    /// `finish(_, true)` writes but does not consume; `consume_all` does.
    pub hold_consumed: AtomicBool,
    pub held_consumed: Mutex<Vec<SendTicket>>,
    /// Refuse every enqueue.
    pub refuse: AtomicBool,
    pub frames_v4: AtomicBool,
    pub bucket: AtomicBool,
    pub now_us: AtomicU64,
}

impl FakeOut {
    pub fn new(log: Arc<Log>) -> Arc<Self> {
        Arc::new(Self {
            log,
            state: Mutex::new(OutState::default()),
            pressured: AtomicBool::new(false),
            sync_receipts: AtomicBool::new(false),
            hold_consumed: AtomicBool::new(false),
            held_consumed: Mutex::new(Vec::new()),
            refuse: AtomicBool::new(false),
            frames_v4: AtomicBool::new(false),
            bucket: AtomicBool::new(false),
            now_us: AtomicU64::new(1_000_000),
        })
    }
    pub fn samples(&self) -> Vec<VideoMetadata> {
        lock(&self.state)
            .packets
            .iter()
            .filter(|p| !p.meta.config)
            .map(|p| p.meta.header)
            .collect()
    }
    pub fn packets(&self) -> Vec<Packet> {
        lock(&self.state).packets.clone()
    }
    /// Payload tickets still held.
    pub fn held(&self) -> Vec<VideoMetadata> {
        lock(&self.state).receipts.iter().map(|(h, _)| *h).collect()
    }
    fn complete(&self, ticket: SendTicket, ok: bool) {
        if !ok {
            drop(ticket);
            return;
        }
        ticket.written(Some(self.now_us.load(Ordering::SeqCst)));
        if self.hold_consumed.load(Ordering::SeqCst) {
            lock(&self.held_consumed).push(ticket);
        } else {
            ticket.consumed();
        }
    }
    /// The peer consumes every written message held so far.
    pub fn consume_all(&self) {
        let held = std::mem::take(&mut *lock(&self.held_consumed));
        for ticket in held {
            ticket.consumed();
        }
    }
    pub fn finish(&self, header: VideoMetadata, ok: bool) {
        let ticket = {
            let mut state = lock(&self.state);
            let index = state
                .receipts
                .iter()
                .position(|(h, _)| {
                    h.generation == header.generation && h.sequence == header.sequence
                })
                .expect("no such receipt");
            state.receipts.remove(index).1
        };
        self.complete(ticket, ok);
    }
    pub fn finish_any(&self, index: usize, ok: bool) -> bool {
        let ticket = {
            let mut state = lock(&self.state);
            if state.receipts.is_empty() {
                return false;
            }
            let index = index % state.receipts.len();
            state.receipts.remove(index).1
        };
        self.complete(ticket, ok);
        true
    }
}

impl SinkTransport for FakeOut {
    fn frames_v4(&self) -> bool {
        self.frames_v4.load(Ordering::SeqCst)
    }
    fn bulk_bucket_enabled(&self) -> bool {
        self.bucket.load(Ordering::SeqCst)
    }
    fn set_bulk_rate_inputs(&self, capacity_bps: u64, video_bps: u64) {
        lock(&self.state).rates.push((capacity_bps, video_bps));
    }
    fn now_us(&self) -> u64 {
        self.now_us.load(Ordering::SeqCst)
    }
    fn backpressured(&self) -> bool {
        self.pressured.load(Ordering::SeqCst)
    }
    fn enqueue(&self, message: Vec<u8>, ticket: SendTicket) -> Result<(), SendTicket> {
        // A leaf call: any thread (the sink enqueues from write completions),
        // never after Closed.
        if self.log.closed.load(Ordering::SeqCst) {
            self.log.after_close.fetch_add(1, Ordering::SeqCst);
        }
        if self.refuse.load(Ordering::SeqCst) {
            return Err(ticket);
        }
        assert!(message.len() > 5);
        let kind = message[0];
        let length = u32::from_be_bytes(message[1..5].try_into().unwrap()) as usize;
        assert_eq!(length, message.len() - 5, "envelope length");
        if kind != 0x10 {
            lock(&self.state).control.push(message);
            ticket.written(Some(self.now_us.load(Ordering::SeqCst)));
            ticket.consumed();
            return Ok(());
        }
        let version = if self.frames_v4() {
            FrameVersion::V4
        } else {
            FrameVersion::V3
        };
        let decoded =
            ExtendedVideoPacket::decode(&message[5..], version).expect("a valid video message");
        let header = decoded.metadata;
        let config = header.flags == 2;
        let packet = Packet {
            meta: PacketMeta {
                header,
                keyframe: header.flags == 1,
                config,
                droppable: header.flags == 0,
                extensions: decoded.extensions,
            },
            bytes: decoded.data.to_vec(),
            message: message.clone(),
            on_executor: on_executor(),
        };
        let sync = config || self.sync_receipts.load(Ordering::SeqCst);
        {
            let mut state = lock(&self.state);
            state.packets.push(packet);
            if !sync {
                state.receipts.push((header, ticket));
                return Ok(());
            }
        }
        self.complete(ticket, true);
        Ok(())
    }
}

pub type Reentry = Box<dyn Fn(&ObserverJob) + Send + Sync>;

pub struct FakeObserver {
    pub log: Arc<Log>,
    pub jobs: Mutex<Vec<ObserverJob>>,
    pub reentry: Mutex<Option<Reentry>>,
    pub closed_tx: Mutex<Option<mpsc::Sender<CloseOutcome>>>,
}

impl FakeObserver {
    pub fn new(log: Arc<Log>) -> Arc<Self> {
        Arc::new(Self {
            log,
            jobs: Mutex::new(Vec::new()),
            reentry: Mutex::new(None),
            closed_tx: Mutex::new(None),
        })
    }
    pub fn jobs(&self) -> Vec<ObserverJob> {
        lock(&self.jobs).clone()
    }
    pub fn results(&self, op: OpToken) -> Vec<OpOutcome> {
        self.jobs()
            .into_iter()
            .filter_map(|j| match j {
                ObserverJob::OpResult { op: o, outcome } if o == op => Some(outcome),
                _ => None,
            })
            .collect()
    }
    pub fn failures(&self) -> Vec<String> {
        self.jobs()
            .into_iter()
            .filter_map(|j| match j {
                ObserverJob::Failed { code } => Some(code),
                _ => None,
            })
            .collect()
    }
    pub fn closed(&self) -> Option<CloseOutcome> {
        self.jobs().into_iter().find_map(|j| match j {
            ObserverJob::Closed { outcome } => Some(outcome),
            _ => None,
        })
    }
}

impl ObserverQueue for FakeObserver {
    fn post(&self, job: ObserverJob) {
        let _call = self.log.outbound();
        if let ObserverJob::Closed { outcome } = &job {
            self.log.closed.store(true, Ordering::SeqCst);
            if let Some(tx) = lock(&self.closed_tx).as_ref() {
                tx.send(*outcome).ok();
            }
        }
        lock(&self.jobs).push(job.clone());
        if let Some(reentry) = lock(&self.reentry).as_ref() {
            reentry(&job);
        }
    }
}

// ---------------------------------------------------------------------------
// Plans and harnesses.

pub fn srgb() -> ColorTags {
    ColorTags {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Srgb,
        matrix: YCbCrMatrix::Bt709,
        full_range: false,
    }
}

pub fn p3() -> ColorTags {
    ColorTags {
        primaries: ColorPrimaries::DisplayP3,
        ..srgb()
    }
}

pub fn spec() -> CaptureSpec {
    CaptureSpec {
        target: CaptureTarget::Display { display_id: 1 },
        mode: StreamMode::Control,
        pixel_width: 64,
        pixel_height: 64,
        min_interval_ns: SECOND / 60,
        format: PixelFormat::Nv12,
        color: ColorRequest::Source,
        cursor: CursorMode::Composited,
        queue_depth: 5,
        adapter_luid: None,
    }
}

pub fn plan() -> StreamPlan {
    StreamPlan {
        mode: StreamMode::Control,
        codec: Codec::H264,
        encode_width: 64,
        encode_height: 64,
        fps: 60,
        bitrate_bps: 2_000_000,
        convert: ConvertPath::Identity,
        encoder_input: PixelFormat::Nv12,
        encoder: "fake".into(),
        color_out: srgb(),
        color_mapping: ColorMapping {
            allow_p3: true,
            srgb_transfer_as_bt709: false,
        },
        extensions: vec![],
        permit_target: 3,
        recovery: vec![RecoveryStep::Idr],
        on_source_gone: OnSourceGone::Fail,
        medium_cap_bps: None,
        adapter_luid: None,
        refresh_interval_ns: None,
        pacing: PacingPolicy::default(),
    }
}

pub struct Fakes {
    pub log: Arc<Log>,
    pub converter: Arc<FakeConverter>,
    pub capture: Arc<FakeCapture>,
    pub factory: Arc<FakeFactory>,
    pub out: Arc<FakeOut>,
    pub observer: Arc<FakeObserver>,
}

impl Fakes {
    pub fn new() -> Self {
        let log = Arc::new(Log::default());
        Self {
            capture: FakeCapture::new(log.clone()),
            converter: FakeConverter::new(log.clone()),
            factory: FakeFactory::new(log.clone()),
            out: FakeOut::new(log.clone()),
            observer: FakeObserver::new(log.clone()),
            log,
        }
    }

    pub fn parts(
        &self,
        port: Arc<StreamPort>,
        queue: Arc<dyn SerialQueue>,
        plan: StreamPlan,
    ) -> StreamParts {
        StreamParts {
            stream_id: 1,
            spec: spec(),
            plan,
            port,
            queue,
            capture: self.capture.clone(),
            converter: None,
            encoders: self.factory.clone(),
            output: VideoOutput::with_transport(self.out.clone(), self.policy(), None),
            observer: self.observer.clone(),
            timings: None,
        }
    }

    /// The executor tests run the sink without its bounds (P1's enqueue-all,
    /// still through the sink) so they see every packet at once; the sink's
    /// own tests (`sink.rs`) turn them on.
    pub fn policy(&self) -> SinkPolicy {
        SinkPolicy {
            bounds: false,
            ..SinkPolicy::default()
        }
    }

    /// Invariants every test ends with, after the stream closed.
    pub fn assert_clean(&self) {
        assert_eq!(
            self.capture.double_release.load(Ordering::SeqCst),
            0,
            "a surface released twice"
        );
        assert_eq!(
            self.capture.leases_outstanding(),
            0,
            "a surface never released"
        );
        assert!(
            lock(&self.converter.live).is_empty(),
            "a converted surface never released"
        );
        assert_eq!(self.converter.double_release.load(Ordering::SeqCst), 0);
        assert_eq!(
            self.factory.create_on_executor.load(Ordering::SeqCst),
            0,
            "create ran on the executor"
        );
        assert_eq!(
            self.log.after_close.load(Ordering::SeqCst),
            0,
            "an outbound call after Closed"
        );
        assert_eq!(
            self.log.off_executor.load(Ordering::SeqCst),
            0,
            "an outbound call outside a drain"
        );
        assert!(
            self.log.peak_native_permits.load(Ordering::SeqCst) <= 3,
            "more than 3 frames in flight"
        );
    }

    /// Wire sequences increase within each generation, and every generation
    /// starts with config followed by a keyframe.
    pub fn assert_wire_order(&self) {
        let packets = self.out.packets();
        let mut generation = 0;
        let mut last_seq = 0;
        let mut expect_key = false;
        for p in &packets {
            let h = p.meta.header;
            assert!(h.generation >= generation, "generation went back");
            if h.generation > generation {
                assert!(p.meta.config, "a new generation must start with config");
                generation = h.generation;
                last_seq = 0;
                expect_key = true;
                continue;
            }
            if p.meta.config {
                expect_key = true;
                continue;
            }
            if expect_key {
                assert!(p.meta.keyframe, "config must precede a keyframe");
                expect_key = false;
            }
            assert!(
                h.sequence > last_seq,
                "sequence must increase within a generation"
            );
            last_seq = h.sequence;
        }
    }
}

/// A stream on a manual queue and clock.
pub struct Manual {
    pub fakes: Fakes,
    pub clock: Arc<ManualClock>,
    pub queue: Arc<ManualQueue>,
    pub controller: Arc<StreamController>,
}

impl std::ops::Deref for Manual {
    type Target = Fakes;
    fn deref(&self) -> &Fakes {
        &self.fakes
    }
}

impl Manual {
    pub fn new() -> Self {
        Self::with(|_, _| {})
    }

    pub fn with(adjust: impl FnOnce(&Fakes, &mut StreamParts)) -> Self {
        Self::with_clock(Arc::new(ManualClock(AtomicU64::new(SECOND))), adjust)
    }

    /// [`Manual::with`] on a given clock (shared with a simulated transport).
    pub fn with_clock(
        clock: Arc<ManualClock>,
        adjust: impl FnOnce(&Fakes, &mut StreamParts),
    ) -> Self {
        let fakes = Fakes::new();
        let port = StreamPort::new();
        let queue = ManualQueue::new(port.clone(), clock.clone());
        let mut parts = fakes.parts(port, queue.clone(), plan());
        adjust(&fakes, &mut parts);
        let controller = StreamController::with_clock(parts, clock.clone()).expect("valid stream");
        Self {
            fakes,
            clock,
            queue,
            controller,
        }
    }

    pub fn pump(&self) {
        self.queue.pump();
    }

    pub fn advance_ms(&self, ms: u64) {
        self.queue.advance(ms * MS);
    }

    /// Start and confirm the capture start; the stream is Running.
    pub fn start_running(&self) -> OpToken {
        let op = self.controller.start();
        self.pump();
        self.capture.answer(Op::Start, None);
        self.pump();
        assert_eq!(self.observer.results(op), vec![OpOutcome::Completed]);
        assert_eq!(self.controller.phase(), StreamPhase::Running);
        op
    }

    /// A stream whose queue, capture backend and encoders make
    /// the per-frame calls through the C ABI.
    pub fn native() -> Self {
        let s = Self::new();
        s.factory.native.store(true, Ordering::SeqCst);
        let callbacks = native::StreamCallbacks {
            queue: Arc::as_ptr(&s.queue).cast_mut().cast(),
            wake: Some(glue::wake),
            capture: Arc::as_ptr(&s.capture).cast_mut().cast(),
            release: Some(glue::release),
        };
        assert!(native::streamcore_video_attach_stream(
            s.queue.port.native_key(),
            callbacks
        ));
        s
    }

    /// Deliver a frame through the C ABI (shaped like the last full frame),
    /// then pump.
    pub fn frame_native(&self, capture_ns: u64) -> SurfaceId {
        let id = self.capture.surface(srgb(), capture_ns).id;
        assert!(native::streamcore_video_frame(
            self.queue.port.native_key(),
            CAPTURE,
            id,
            capture_ns,
            0
        ));
        self.pump();
        id
    }

    /// Deliver a frame "on the queue" (outside any drain), then pump.
    pub fn frame(&self, capture_ns: u64) -> SurfaceId {
        let id = self.capture.emit(srgb(), capture_ns);
        self.pump();
        id
    }

    pub fn encoder(&self) -> Arc<FakeEncoder> {
        self.factory.last()
    }

    /// Run `f` (which creates the encoder, e.g. the first frame), then return
    /// the encoder.
    pub fn encoder_after(&self, f: impl FnOnce(&Self)) -> Arc<FakeEncoder> {
        f(self);
        self.encoder()
    }

    /// Close: begin, answer the capture stop, check the outcome.
    pub fn close(&self) {
        self.controller.begin_close();
        self.pump();
        if self.controller.close_outcome().is_none() && self.capture.last_op(Op::Stop).is_some() {
            self.capture.answer(Op::Stop, None);
            self.pump();
        }
        assert_eq!(self.controller.close_outcome(), Some(CloseOutcome::Closed));
        assert_eq!(self.observer.closed(), Some(CloseOutcome::Closed));
    }
}

/// Run `f` on a helper thread and fail if it takes longer than `limit` (a
/// deadlock).
pub fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        tx.send(f()).ok();
    });
    rx.recv_timeout(limit)
        .expect("deadlock: the call did not return in time")
}

/// The fakes behind the per-frame C ABI: each context is the fake given to
/// the stream through `StreamParts` (or returned by `create`), which the
/// stream keeps alive while it may call.
#[allow(unsafe_code)]
pub mod glue {
    use super::*;
    use std::ffi::c_void;

    pub extern "C" fn wake(queue: *mut c_void) {
        // SAFETY: the context is the stream's `ManualQueue`, alive for the call.
        let queue = unsafe { &*queue.cast::<ManualQueue>() };
        queue.wake();
    }

    pub extern "C" fn release(capture: *mut c_void, surface: u64) {
        // SAFETY: the context is the stream's `FakeCapture`, alive for the call.
        let capture = unsafe { &*capture.cast::<FakeCapture>() };
        CaptureBackend::release(capture, surface);
    }

    pub extern "C" fn submit(
        encoder: *mut c_void,
        _surface: u64,
        submit_seq: u64,
        pts_ns: u64,
        capture_ns: u64,
        force_keyframe: bool,
        ltr_acknowledged: u32,
        ltr_refresh: bool,
    ) -> i32 {
        // SAFETY: the context is the `FakeEncoder` returned by `create`, which
        // the stream holds while it may submit.
        let encoder = unsafe { &*encoder.cast::<FakeEncoder>() };
        let opts = SubmitOpts {
            submit_seq,
            pts_ns,
            capture_ns,
            force_keyframe,
            ltr_refresh,
            ltr_acknowledged: (ltr_acknowledged != 0).then_some(ltr_acknowledged),
        };
        match encoder.accept(opts, None) {
            SubmitResult::Submitted => native::SUBMITTED,
            SubmitResult::Busy => native::SUBMIT_BUSY,
            SubmitResult::Failed { error } => {
                native::SUBMIT_FAILED + native::media_error_code(&error)
            }
        }
    }
}
