//! The stream state owned by one executor (3.5, 3.6): lifecycle, the latest
//! slot, admission, the encoder slot and its generation, recovery and the
//! `submitted_seq` / `delivered_keyframe` / `lost_seq` fences. Nothing outside a
//! drain reads or writes it.
//!
//! Recovery keeps the reference host's behaviour (2.3): a host-detected loss is
//! recovered after the recovery delay, at most once per recovery interval, never
//! while the sink is backpressured, until a keyframe submitted after the loss is
//! delivered; a client request fences the latest submission and recovers at
//! once.

use crate::bounded::run_bounded_value;
use crate::error::{MediaError, codes};
use crate::executor::*;
use crate::traits::*;
use crate::types::*;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use streamcore_media::stream::{VideoPacketizer, VideoWorkPermit, valid_video_settings};

pub(crate) enum Command {
    Start(OpToken),
    Update(OpToken, StreamUpdate),
    Stop(OpToken),
    RequestKeyframe,
    Revoked,
    Stats,
}

/// Jobs the executor schedules for its own next wake.
pub(crate) enum Internal {
    /// Re-create the encoder (colour, format or size change; device loss).
    Recreate,
    SubmitFailed(MediaError),
    ConvertFailed(MediaError),
    Fail(&'static str),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OpKind {
    Start,
    Update,
    Stop,
}

#[derive(Clone, Copy)]
struct PendingOp {
    token: OpToken,
    kind: OpKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TimerKind {
    Recovery,
    Op(OpToken),
    Command(OpToken),
}

struct EncoderSlot {
    generation: u64,
    encoder: Arc<dyn StreamEncoder>,
    /// The encoder's C submit (`streamcore_video_attach_encoder`), called
    /// instead of `submit` while `encoder` is held.
    native_submit: Option<crate::native::NativeSubmit>,
    info: EncoderInfo,
    /// The mapped output tags and source format this session was created for.
    color: ColorTags,
    source_format: PixelFormat,
    sink: Arc<VideoSinkHandle>,
    /// The first output of a session always reports `config_changed` (2.5).
    needs_config: bool,
    /// The first submit of a session is an IDR.
    first_submit: bool,
}

struct Retained {
    lease: Arc<SurfaceLease>,
    info: SurfaceInfo,
    mapped: ColorTags,
    pending: bool,
    /// A recovery re-encode's submission time.
    restamp: Option<u64>,
}

struct InFlight {
    permit: Arc<VideoWorkPermit>,
    /// The capture surface the encoder reads: `None` on the convert path, where
    /// the hold ends when `convert` returns (the converter's contract).
    _surface: Option<Arc<SurfaceLease>>,
    _converted: Option<SurfaceLease>,
    /// H0 and H1 of the submitted image.
    source_present_ns: Option<u64>,
    capture_ns: u64,
}

impl InFlight {
    /// Output handled or abandoned: release admission now.
    fn finish(self) {
        self.permit.release();
    }
}

pub(crate) struct Backends {
    pub(crate) capture: Arc<dyn CaptureBackend>,
    pub(crate) converter: Option<Arc<dyn Converter>>,
    pub(crate) encoders: Arc<dyn EncoderFactory>,
    pub(crate) out: Arc<dyn VideoOut>,
    pub(crate) observer: Arc<dyn ObserverQueue>,
}

pub(crate) struct StreamState {
    executor: Weak<Shared>,
    spec: CaptureSpec,
    plan: StreamPlan,
    timings: StreamTimings,
    /// Dropped at `Closed`, which ends the reference cycle with native code.
    backends: Option<Backends>,

    phase: StreamPhase,
    capture: Option<CaptureId>,
    op: Option<PendingOp>,
    command: Option<OpToken>,
    stop_waiters: Vec<OpToken>,
    stop_requested: bool,
    epoch: u64,
    next_op: u64,

    encoder: Option<EncoderSlot>,
    encoder_generation: u64,
    announced_codec: Option<Codec>,
    recreate_scheduled: bool,
    /// Busy or out of resources: wait for an output, a drop or a new frame.
    stalled: bool,
    device_losses: VecDeque<u64>,

    latest: Option<Retained>,
    /// (H1 of the latest frame, executor clock then): maps the executor clock to
    /// the host clock for recovery time stamps.
    time_anchor: Option<(u64, u64)>,
    last_pts_ns: u64,
    in_flight: BTreeMap<(u64, u64), InFlight>,
    packetizer: VideoPacketizer,
    last_config: Option<Vec<u8>>,

    submitted_seq: u64,
    delivered_keyframe: u64,
    lost_seq: u64,
    recovery_needed: bool,
    force_keyframe: bool,
    last_recovery_ns: Option<u64>,
    recovery_timer: Option<TimerToken>,

    timers: BTreeMap<TimerToken, (u64, TimerKind)>,
    next_timer: TimerToken,
    armed: Option<(u64, TimerToken)>,

    layout_epoch: u64,
    /// A `Region` target whose capture holds the whole display: the converter
    /// crops `ConvertJob::source` (read from `CaptureCaps` at start).
    crop_region: bool,
    closing: bool,
    closed: Option<CloseOutcome>,
    pub(crate) keepalive: Option<Arc<Shared>>,
    stats: StatsSnapshot,
}

const DEVICE_LOSS_WINDOW_NS: u64 = 10_000_000_000;
const MAX_DEVICE_LOSSES: usize = 3;
const MAX_STOP_WAITERS: usize = 16;

/// Rust is the single validator (2.5.8): nothing reaches native allocation
/// unchecked.
pub(crate) fn validate(spec: &CaptureSpec, plan: &StreamPlan) -> Result<(), MediaError> {
    let dims = |w: u32, h: u32| (2..=8192).contains(&w) && (2..=8192).contains(&h);
    let fps = i64::from(plan.fps);
    if !dims(plan.encode_width, plan.encode_height)
        || !dims(spec.pixel_width, spec.pixel_height)
        || !valid_video_settings(
            i64::from(plan.encode_width.max(plan.encode_height)),
            fps,
            0.0,
        )
        || plan.bitrate_bps == 0
        || !(1..=3).contains(&plan.permit_target)
        || spec.queue_depth == 0
        || spec.mode != plan.mode
        || plan.codec == Codec::Av1
    {
        return Err(MediaError::InvalidSettings);
    }
    if plan.convert == ConvertPath::Identity && spec.format != plan.encoder_input {
        return Err(MediaError::InvalidSettings);
    }
    match &spec.target {
        CaptureTarget::Region { rect, .. } if !valid_region(rect) => {
            return Err(MediaError::InvalidSettings);
        }
        // The canvas policy is a later P1 slice.
        CaptureTarget::Canvas { .. } => return Err(MediaError::Unsupported),
        CaptureTarget::App { app_id } if app_id.is_empty() => {
            return Err(MediaError::InvalidSettings);
        }
        _ => {}
    }
    Ok(())
}

/// A normalized, non-empty rectangle inside its source.
fn valid_region(rect: &NormalizedRect) -> bool {
    let unit = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
    unit(rect.x)
        && unit(rect.y)
        && unit(rect.width)
        && unit(rect.height)
        && rect.width > 0.0
        && rect.height > 0.0
        && rect.x + rect.width <= 1.0
        && rect.y + rect.height <= 1.0
}

/// A normalized region in pixels of a `width` × `height` surface, with an even
/// origin and size (4:2:0 chroma), at least 2 × 2 and inside the surface.
fn crop_rect(rect: &NormalizedRect, width: u32, height: u32) -> Rect {
    let even = |v: f64| (v.max(0.0) as u32) & !1;
    let x = even(rect.x * f64::from(width)).min(width.saturating_sub(2));
    let y = even(rect.y * f64::from(height)).min(height.saturating_sub(2));
    let w = even(rect.width * f64::from(width)).clamp(2, width - x);
    let h = even(rect.height * f64::from(height)).clamp(2, height - y);
    Rect {
        x,
        y,
        width: w & !1,
        height: h & !1,
    }
}

fn valid_surface(info: &SurfaceInfo) -> bool {
    info.id != 0 && (2..=8192).contains(&info.width) && (2..=8192).contains(&info.height)
}

impl StreamState {
    pub(crate) fn new(
        executor: Weak<Shared>,
        stream_id: u32,
        spec: CaptureSpec,
        plan: StreamPlan,
        timings: StreamTimings,
        backends: Backends,
    ) -> Self {
        Self {
            executor,
            spec,
            plan,
            timings,
            backends: Some(backends),
            phase: StreamPhase::Idle,
            capture: None,
            op: None,
            command: None,
            stop_waiters: Vec::new(),
            stop_requested: false,
            epoch: 0,
            next_op: 0,
            encoder: None,
            encoder_generation: 0,
            announced_codec: None,
            recreate_scheduled: false,
            stalled: false,
            device_losses: VecDeque::new(),
            latest: None,
            time_anchor: None,
            last_pts_ns: 0,
            in_flight: BTreeMap::new(),
            packetizer: VideoPacketizer::new(stream_id),
            last_config: None,
            submitted_seq: 0,
            delivered_keyframe: 0,
            lost_seq: 0,
            recovery_needed: false,
            force_keyframe: false,
            last_recovery_ns: None,
            recovery_timer: None,
            timers: BTreeMap::new(),
            next_timer: 0,
            armed: None,
            layout_epoch: 0,
            crop_region: false,
            closing: false,
            closed: None,
            keepalive: None,
            stats: StatsSnapshot::default(),
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.is_some()
    }

    pub(crate) fn close_outcome(&self) -> Option<CloseOutcome> {
        self.closed
    }

    // -----------------------------------------------------------------------
    // Dispatch.

    pub(crate) fn handle(&mut self, cx: &Arc<Shared>, event: Event) {
        if self.closed.is_some() {
            // Release-only: dropping the event releases its lease or permit.
            return;
        }
        match event {
            Event::Frame {
                capture,
                member,
                lease,
                info,
                status,
            } => self.on_frame(cx, capture, member, lease, info, status),
            Event::OpDone { op, error } => self.on_op_done(cx, op, error),
            Event::SourceGone { capture, reason } => self.on_source_gone(cx, capture, reason),
            Event::Layout { capture, layout } => {
                if self.capture == Some(capture) && !self.closing {
                    self.layout_epoch += 1;
                    self.notify(
                        cx,
                        ObserverJob::LayoutChanged {
                            epoch: self.layout_epoch,
                            layout,
                        },
                    );
                } else {
                    self.stats.stale_events += 1;
                }
            }
            Event::InputReady { encoder } => {
                if self.is_current_encoder(encoder) {
                    self.stalled = false;
                    self.submit_due(cx);
                }
            }
            Event::Encoded { encoder, frame } => self.on_encoded(cx, encoder, frame),
            Event::Dropped {
                encoder,
                submit_seq,
                reason,
            } => self.on_dropped(cx, encoder, submit_seq, reason),
            Event::Receipt {
                encoder,
                sequence,
                keyframe,
                config,
                accepted,
            } => self.on_receipt(cx, encoder, sequence, keyframe, config, accepted),
            Event::PermitReleased => self.submit_due(cx),
            Event::Timer(token) => {
                if self.armed.is_some_and(|(_, armed)| armed == token) {
                    self.armed = None;
                }
            }
            Event::Command(command) => self.on_command(cx, command),
            Event::Internal(job) => self.on_internal(cx, job),
            Event::Close { keepalive } => {
                if keepalive.is_some() && self.keepalive.is_none() {
                    self.keepalive = keepalive;
                }
                self.on_close(cx);
            }
        }
    }

    /// Inline drain, frames only: admission, convert and submit. Everything else
    /// this frame causes is posted for the next wake.
    pub(crate) fn handle_frame_inline(&mut self, cx: &Arc<Shared>, event: Event) {
        if let Event::Frame {
            capture,
            member,
            lease,
            info,
            status,
        } = event
        {
            self.on_frame(cx, capture, member, lease, info, status);
        }
    }

    /// Fire due timers, arm the next one, publish the snapshot.
    pub(crate) fn after_drain(&mut self, cx: &Arc<Shared>) {
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
                    self.on_timer(cx, token, kind);
                }
                if self.closed.is_some() {
                    break;
                }
            }
            self.arm(cx);
        }
        self.publish(cx);
    }

    pub(crate) fn publish(&mut self, cx: &Shared) {
        let mut stats = self.stats;
        stats.frames_replaced += cx
            .mailbox_replaced
            .load(std::sync::atomic::Ordering::Relaxed);
        stats.capture_skipped = cx
            .capture_skipped
            .load(std::sync::atomic::Ordering::Relaxed);
        let phase = if self.closed.is_some() {
            StreamPhase::Closed
        } else if self.closing {
            StreamPhase::Closing
        } else {
            self.phase
        };
        let mut published = lock(&cx.published);
        published.phase = phase;
        published.stats = stats;
    }

    // -----------------------------------------------------------------------
    // Outbound helpers. Every native call below runs on the executor during a
    // drain, with no Rust lock held (rule 2).

    fn notify(&self, cx: &Shared, job: ObserverJob) {
        debug_assert!(is_current(cx.id));
        if let Some(backends) = &self.backends {
            backends.observer.post(job);
        }
    }

    fn post_internal(&self, cx: &Shared, job: Internal) {
        cx.post(Event::Internal(job));
    }

    fn new_op(&mut self, kind: OpKind) -> OpToken {
        self.next_op += 1;
        let token = OpToken {
            id: self.next_op,
            epoch: self.epoch,
        };
        self.op = Some(PendingOp { token, kind });
        token
    }

    fn set_timer(&mut self, cx: &Shared, delay_ns: u64, kind: TimerKind) -> TimerToken {
        self.next_timer += 1;
        let token = self.next_timer;
        let deadline = cx.clock.now_ns().saturating_add(delay_ns);
        self.timers.insert(token, (deadline, kind));
        token
    }

    fn cancel_timers(&mut self, matches: impl Fn(&TimerKind) -> bool) {
        self.timers.retain(|_, (_, kind)| !matches(kind));
    }

    fn arm(&mut self, cx: &Shared) {
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

    fn reply(&self, cx: &Shared, op: OpToken, outcome: OpOutcome) {
        self.notify(cx, ObserverJob::OpResult { op, outcome });
    }

    fn rejected(code: &str) -> OpOutcome {
        OpOutcome::Rejected { code: code.into() }
    }

    fn finish_command(&mut self, cx: &Shared, outcome: OpOutcome) {
        if let Some(op) = self.command.take() {
            self.cancel_timers(|kind| *kind == TimerKind::Command(op));
            self.reply(cx, op, outcome);
        }
    }

    fn resolve_stops(&mut self, cx: &Shared, outcome: OpOutcome) {
        for op in std::mem::take(&mut self.stop_waiters) {
            self.reply(cx, op, outcome.clone());
        }
    }

    // -----------------------------------------------------------------------
    // Commands and lifecycle.

    fn on_command(&mut self, cx: &Arc<Shared>, command: Command) {
        match command {
            Command::Start(op) => self.on_start(cx, op),
            Command::Update(op, update) => self.on_update(cx, op, update),
            Command::Stop(op) => self.on_stop(cx, op),
            Command::RequestKeyframe => self.request_recovery(cx),
            Command::Revoked => {
                self.latest = None;
                self.recovery_needed = false;
                self.recovery_timer = None;
                self.cancel_timers(|kind| *kind == TimerKind::Recovery);
            }
            Command::Stats => {
                self.publish(cx);
                let stats = lock(&cx.published).stats;
                self.notify(cx, ObserverJob::Stats { snapshot: stats });
            }
        }
    }

    fn on_start(&mut self, cx: &Arc<Shared>, op: OpToken) {
        if self.closing || cx.revoked() {
            return self.reply(cx, op, Self::rejected(codes::CANCELLED));
        }
        if self.command.is_some() || self.phase != StreamPhase::Idle || self.capture.is_some() {
            return self.reply(cx, op, Self::rejected(codes::BUSY));
        }
        if let Err(error) = validate(&self.spec, &self.plan) {
            return self.reply(cx, op, Self::rejected(error.wire_code(codes::CONFIG)));
        }
        self.crop_region = false;
        if matches!(self.spec.target, CaptureTarget::Region { .. })
            && let Some(backends) = &self.backends
        {
            let capture = backends.capture.clone();
            debug_assert!(is_current(cx.id));
            if !capture.caps().region_in_surface {
                if self.plan.convert == ConvertPath::Identity {
                    // Nothing could crop the whole-display capture.
                    let error = MediaError::Unsupported;
                    return self.reply(cx, op, Self::rejected(error.wire_code(codes::CAPTURE)));
                }
                self.crop_region = true;
            }
        }
        self.epoch += 1;
        self.command = Some(op);
        self.set_timer(cx, self.timings.command_timeout_ns, TimerKind::Command(op));
        self.reset_stream_state();
        if let Err(error) = self.create_encoder(cx, true) {
            return self.finish_command(cx, Self::rejected(error.wire_code(codes::ENCODER)));
        }
        let Some(backends) = &self.backends else {
            return;
        };
        let capture = backends.capture.clone();
        let Some(queue) = cx.queue() else {
            return;
        };
        let port = Arc::new(CapturePort {
            executor: self.executor.clone(),
            backend: Arc::downgrade(&capture),
            shape: std::sync::Mutex::new(None),
        });
        cx.set_capture_port(&port);
        let events: Arc<dyn CaptureEvents> = port;
        debug_assert!(is_current(cx.id));
        let mut spec = self.spec.clone();
        spec.adapter_luid = self.plan.adapter_luid;
        match capture.open(spec, queue, events) {
            Ok(id) => {
                self.capture = Some(id);
                self.phase = StreamPhase::Starting;
                let token = self.new_op(OpKind::Start);
                self.set_timer(cx, self.timings.op_timeout_ns, TimerKind::Op(token));
                capture.start(id, token);
            }
            Err(error) => {
                self.teardown_encoder(cx);
                self.finish_command(cx, Self::rejected(error.wire_code(codes::CAPTURE)));
            }
        }
    }

    fn on_update(&mut self, cx: &Arc<Shared>, op: OpToken, update: StreamUpdate) {
        if self.closing || cx.revoked() {
            return self.reply(cx, op, Self::rejected(codes::CANCELLED));
        }
        if self.phase != StreamPhase::Running
            || self.command.is_some()
            || !self.stop_waiters.is_empty()
            || self.stop_requested
        {
            return self.reply(cx, op, Self::rejected(codes::BUSY));
        }
        let size = match (update.encode_width, update.encode_height) {
            (None, None) => None,
            (Some(w), Some(h)) if (2..=8192).contains(&w) && (2..=8192).contains(&h) => {
                Some((w, h))
            }
            _ => return self.reply(cx, op, Self::rejected(codes::CONFIG)),
        };
        if let Some(rate) = update.rate
            && (!(1..=120).contains(&rate.fps) || rate.bitrate_bps == 0)
        {
            return self.reply(cx, op, Self::rejected(codes::CONFIG));
        }
        if let Some(region) = &update.capture.region
            && !valid_region(region)
        {
            return self.reply(cx, op, Self::rejected(codes::CONFIG));
        }
        let Some(id) = self.capture else {
            return self.reply(cx, op, Self::rejected(codes::MISSING));
        };
        if let (Some(region), CaptureTarget::Region { rect, .. }) =
            (update.capture.region, &mut self.spec.target)
        {
            // A live move: later converts crop the new rect.
            *rect = region;
        }
        self.command = Some(op);
        self.set_timer(cx, self.timings.command_timeout_ns, TimerKind::Command(op));
        if let Some(rate) = update.rate {
            self.plan.fps = rate.fps;
            self.plan.bitrate_bps = rate.bitrate_bps;
            if let Some(slot) = &self.encoder {
                let encoder = slot.encoder.clone();
                debug_assert!(is_current(cx.id));
                if let Err(error) = encoder.set_rate(rate) {
                    return self.finish_command(cx, Self::rejected(error.wire_code(codes::UPDATE)));
                }
            }
        }
        if let Some((w, h)) = size
            && (w, h) != (self.plan.encode_width, self.plan.encode_height)
        {
            // A size change is a new session: new generation, config + IDR.
            self.plan.encode_width = w;
            self.plan.encode_height = h;
            self.schedule_recreate(cx);
        }
        let Some(backends) = &self.backends else {
            return;
        };
        let capture = backends.capture.clone();
        self.phase = StreamPhase::Updating;
        let token = self.new_op(OpKind::Update);
        self.set_timer(cx, self.timings.op_timeout_ns, TimerKind::Op(token));
        debug_assert!(is_current(cx.id));
        capture.update(id, update.capture, token);
    }

    fn on_stop(&mut self, cx: &Arc<Shared>, op: OpToken) {
        match self.phase {
            StreamPhase::Idle if self.capture.is_none() => {
                self.reply(cx, op, OpOutcome::Completed);
            }
            // A missing native answer is no proof its operation ended: keep the
            // capture until a stop is confirmed.
            StreamPhase::StartUnknown | StreamPhase::UpdateUnknown | StreamPhase::StopUnknown => {
                self.reply(cx, op, OpOutcome::OutcomeUnknown);
            }
            _ if self.stop_waiters.len() >= MAX_STOP_WAITERS => {
                self.reply(cx, op, Self::rejected(codes::BUSY));
            }
            phase => {
                self.stop_waiters.push(op);
                if matches!(phase, StreamPhase::Running | StreamPhase::StopFailed) {
                    self.begin_stop(cx);
                }
            }
        }
    }

    /// Clears every lease, invalidates the encoder, then stops the capture.
    fn begin_stop(&mut self, cx: &Arc<Shared>) {
        if self.phase == StreamPhase::Stopping {
            return;
        }
        let Some(id) = self.capture else {
            self.phase = StreamPhase::Idle;
            return;
        };
        self.latest = None;
        self.clear_recovery();
        self.teardown_encoder(cx);
        let Some(backends) = &self.backends else {
            return;
        };
        let capture = backends.capture.clone();
        self.phase = StreamPhase::Stopping;
        let token = self.new_op(OpKind::Stop);
        self.set_timer(cx, self.timings.op_timeout_ns, TimerKind::Op(token));
        debug_assert!(is_current(cx.id));
        capture.stop(id, token);
    }

    fn on_op_done(&mut self, cx: &Arc<Shared>, token: OpToken, error: Option<MediaError>) {
        let Some(op) = self.op.filter(|op| op.token == token) else {
            self.stats.stale_events += 1;
            return;
        };
        self.cancel_timers(|kind| *kind == TimerKind::Op(token));
        let wants_stop =
            |s: &Self| s.closing || cx.revoked() || s.stop_requested || !s.stop_waiters.is_empty();
        match (op.kind, self.phase) {
            (OpKind::Start, StreamPhase::Starting) => {
                self.op = None;
                self.phase = StreamPhase::Running;
                if let Some(error) = error {
                    self.finish_command(cx, Self::rejected(error.wire_code(codes::START)));
                    self.begin_stop(cx);
                } else if wants_stop(self) {
                    self.finish_command(cx, Self::rejected(codes::CANCELLED));
                    self.begin_stop(cx);
                } else {
                    if let Some(slot) = &self.encoder {
                        let mut plan = self.plan.clone();
                        plan.codec = slot.info.codec;
                        self.notify(
                            cx,
                            ObserverJob::Started {
                                plan,
                                info: slot.info.clone(),
                            },
                        );
                    }
                    self.finish_command(cx, OpOutcome::Completed);
                    self.submit_due(cx);
                }
            }
            (OpKind::Update, StreamPhase::Updating) => {
                self.op = None;
                self.phase = StreamPhase::Running;
                let outcome = match error {
                    None => OpOutcome::Completed,
                    Some(error) => Self::rejected(error.wire_code(codes::UPDATE)),
                };
                self.finish_command(cx, outcome);
                if wants_stop(self) {
                    self.begin_stop(cx);
                }
            }
            // A late answer to a timed-out start or update settles the native
            // operation; the capture is then stopped.
            (OpKind::Start, StreamPhase::StartUnknown)
            | (OpKind::Update, StreamPhase::UpdateUnknown) => {
                self.op = None;
                self.phase = StreamPhase::Running;
                self.begin_stop(cx);
            }
            (OpKind::Stop, StreamPhase::Stopping | StreamPhase::StopUnknown) => {
                self.op = None;
                match error {
                    Some(error) => {
                        self.phase = StreamPhase::StopFailed;
                        self.resolve_stops(cx, Self::rejected(error.wire_code(codes::STOP)));
                        if self.closing {
                            self.finish_close(cx, CloseOutcome::StopUnknown);
                        }
                    }
                    None => {
                        self.capture = None;
                        self.phase = StreamPhase::Idle;
                        self.stop_requested = false;
                        self.resolve_stops(cx, OpOutcome::Completed);
                        if self.closing {
                            self.finish_close(cx, CloseOutcome::Closed);
                        }
                    }
                }
            }
            _ => self.stats.stale_events += 1,
        }
    }

    fn on_op_timeout(&mut self, cx: &Arc<Shared>, token: OpToken) {
        let Some(op) = self.op.filter(|op| op.token == token) else {
            return;
        };
        match (op.kind, self.phase) {
            (OpKind::Start, StreamPhase::Starting) | (OpKind::Update, StreamPhase::Updating) => {
                self.phase = if op.kind == OpKind::Start {
                    StreamPhase::StartUnknown
                } else {
                    StreamPhase::UpdateUnknown
                };
                self.finish_command(cx, OpOutcome::OutcomeUnknown);
                self.resolve_stops(cx, OpOutcome::OutcomeUnknown);
                if self.closing {
                    self.finish_close(cx, CloseOutcome::StopUnknown);
                }
            }
            (OpKind::Stop, StreamPhase::Stopping) => {
                self.phase = StreamPhase::StopUnknown;
                self.resolve_stops(cx, OpOutcome::OutcomeUnknown);
                if self.closing {
                    self.finish_close(cx, CloseOutcome::StopUnknown);
                }
            }
            _ => {}
        }
    }

    fn on_source_gone(&mut self, cx: &Arc<Shared>, capture: CaptureId, reason: SourceGone) {
        if self.capture != Some(capture) || self.closing {
            self.stats.stale_events += 1;
            return;
        }
        self.notify(cx, ObserverJob::SourceGone { reason });
        match self.plan.on_source_gone {
            OnSourceGone::Fail => self.fail(cx, codes::CONTENT),
            // Re-targeting the main display is the consumer's restart for now.
            OnSourceGone::FallBackToMain => self.request_stop(cx),
        }
    }

    fn request_stop(&mut self, cx: &Arc<Shared>) {
        match self.phase {
            StreamPhase::Running | StreamPhase::StopFailed => self.begin_stop(cx),
            StreamPhase::Starting | StreamPhase::Updating => self.stop_requested = true,
            _ => {}
        }
    }

    /// A stream failure: report the code once, drop the pipeline, stop capture.
    fn fail(&mut self, cx: &Arc<Shared>, code: &'static str) {
        if self.closing {
            return;
        }
        self.notify(cx, ObserverJob::Failed { code: code.into() });
        self.finish_command(cx, Self::rejected(code));
        self.latest = None;
        self.clear_recovery();
        self.teardown_encoder(cx);
        self.request_stop(cx);
    }

    /// A drain panicked: report `stream.output` and close, as for a failure the
    /// consumer cannot recover from.
    pub(crate) fn fail_after_panic(&mut self, cx: &Arc<Shared>) {
        self.fail(cx, codes::OUTPUT);
        self.on_close(cx);
    }

    fn on_close(&mut self, cx: &Arc<Shared>) {
        if self.closing {
            return;
        }
        self.closing = true;
        cx.work.close();
        self.finish_command(cx, Self::rejected(codes::CANCELLED));
        self.latest = None;
        self.clear_recovery();
        self.teardown_encoder(cx);
        match self.phase {
            StreamPhase::Idle if self.capture.is_none() => {
                self.finish_close(cx, CloseOutcome::Closed);
            }
            StreamPhase::Running | StreamPhase::StopFailed | StreamPhase::Idle => {
                self.begin_stop(cx);
                if self.phase == StreamPhase::Idle {
                    self.finish_close(cx, CloseOutcome::Closed);
                }
            }
            // An op is outstanding: its answer (or its timeout) continues the close.
            StreamPhase::Starting | StreamPhase::Updating | StreamPhase::Stopping => {}
            _ => self.finish_close(cx, CloseOutcome::StopUnknown),
        }
    }

    /// Enter `Closed`: report on the observer queue, then drop every reference
    /// to native code. The executor completes the close after this drain.
    fn finish_close(&mut self, cx: &Arc<Shared>, outcome: CloseOutcome) {
        if self.closed.is_some() {
            return;
        }
        self.resolve_stops(cx, OpOutcome::OutcomeUnknown);
        self.finish_command(cx, Self::rejected(codes::CANCELLED));
        self.notify(cx, ObserverJob::Closed { outcome });
        self.teardown_encoder(cx);
        self.latest = None;
        self.in_flight.clear();
        self.timers.clear();
        self.closed = Some(outcome);
        self.phase = StreamPhase::Closed;
        self.backends = None;
    }

    // -----------------------------------------------------------------------
    // Timers.

    fn on_timer(&mut self, cx: &Arc<Shared>, token: TimerToken, kind: TimerKind) {
        match kind {
            TimerKind::Recovery => {
                if self.recovery_timer == Some(token) {
                    self.recovery_timer = None;
                    self.recover(cx);
                }
            }
            TimerKind::Op(op) => self.on_op_timeout(cx, op),
            TimerKind::Command(op) => {
                if self.command == Some(op) {
                    self.finish_command(cx, OpOutcome::OutcomeUnknown);
                    self.fail(cx, codes::TIMEOUT);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Encoder sessions.

    fn reset_stream_state(&mut self) {
        self.latest = None;
        self.clear_recovery();
        self.stalled = false;
        self.recreate_scheduled = false;
        self.stop_requested = false;
    }

    fn create_encoder(&mut self, cx: &Arc<Shared>, allow_fallback: bool) -> Result<(), MediaError> {
        let Some(backends) = &self.backends else {
            return Err(MediaError::Internal);
        };
        let factory = backends.encoders.clone();
        let (color, source_format) = match &self.latest {
            Some(latest) => (latest.mapped, latest.info.format),
            None => (self.plan.color_out, self.spec.format),
        };
        let input_format = match self.plan.convert {
            ConvertPath::Identity => source_format,
            ConvertPath::Convert => self.plan.encoder_input,
        };
        let mut codec = self.announced_codec.unwrap_or(self.plan.codec);
        loop {
            self.encoder_generation += 1;
            let generation = self.encoder_generation;
            let sink = VideoSinkHandle::new(self.executor.clone(), generation);
            let spec = EncoderSpec {
                codec,
                width: self.plan.encode_width,
                height: self.plan.encode_height,
                fps: self.plan.fps,
                bitrate_bps: self.plan.bitrate_bps,
                input_format,
                color,
                adapter: self.plan.adapter_luid,
                sink: sink.clone(),
            };
            let events: Arc<dyn EncoderEvents> = Arc::new(EncoderPort {
                executor: self.executor.clone(),
                encoder: generation,
                sink: sink.clone(),
            });
            debug_assert!(is_current(cx.id));
            // `create` may block (3.5): it runs on a helper thread and the
            // executor waits at most the create bound. A late session is
            // invalidated on that thread.
            let creating = factory.clone();
            let created = run_bounded_value(
                Duration::from_nanos(self.timings.create_bound_ns),
                move || creating.create(spec, events),
                |late| {
                    if let Ok(encoder) = late {
                        encoder.invalidate();
                    }
                },
            );
            let Some(created) = created else {
                self.stats.create_timeouts += 1;
                return Err(MediaError::EncoderFailed { recoverable: false });
            };
            match created {
                Ok(encoder) => {
                    let info = encoder.info();
                    // The announced codec is the one created; a replacement keeps it.
                    if info.codec != codec
                        || self
                            .announced_codec
                            .is_some_and(|announced| announced != info.codec)
                    {
                        encoder.invalidate();
                        return Err(MediaError::Unsupported);
                    }
                    self.announced_codec = Some(info.codec);
                    self.stats.encoders_created += 1;
                    self.encoder = Some(EncoderSlot {
                        generation,
                        native_submit: sink.native_submit(),
                        encoder,
                        info,
                        color,
                        source_format,
                        sink,
                        needs_config: true,
                        first_submit: true,
                    });
                    return Ok(());
                }
                Err(MediaError::Unsupported)
                    if allow_fallback && codec == Codec::Hevc && self.announced_codec.is_none() =>
                {
                    codec = Codec::H264;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// `invalidate` (the one allowed wait), then forget the session. In-flight
    /// entries of this session drop Rust's permit references without releasing
    /// them: a permit native code still holds keeps counting (2.5.1). An
    /// invalidate that overran its bound reports `EncoderFailed` and releases
    /// the hung session's permits, so a hung flush cannot stall admission.
    fn teardown_encoder(&mut self, cx: &Shared) {
        let Some(slot) = self.encoder.take() else {
            return;
        };
        slot.sink.close();
        let started = Instant::now();
        debug_assert!(is_current(cx.id));
        slot.encoder.invalidate();
        let bound = Duration::from_nanos(self.timings.invalidate_bound_ns);
        let hung = started.elapsed() >= bound;
        let entries: Vec<_> = self
            .in_flight
            .keys()
            .filter(|(generation, _)| *generation == slot.generation)
            .copied()
            .collect();
        for key in entries {
            if let Some(entry) = self.in_flight.remove(&key)
                && hung
            {
                entry.finish();
            }
        }
        if hung {
            // Late outputs carry the old generation and are discarded.
            self.stats.invalidate_timeouts += 1;
            self.notify(
                cx,
                ObserverJob::EncoderFailed {
                    error: MediaError::EncoderFailed { recoverable: true },
                },
            );
        }
        self.stalled = false;
    }

    fn schedule_recreate(&mut self, cx: &Shared) {
        if !self.recreate_scheduled {
            self.recreate_scheduled = true;
            self.post_internal(cx, Internal::Recreate);
        }
    }

    fn on_internal(&mut self, cx: &Arc<Shared>, job: Internal) {
        match job {
            Internal::Recreate => {
                self.recreate_scheduled = false;
                if self.closing
                    || cx.revoked()
                    || self.capture.is_none()
                    || !matches!(
                        self.phase,
                        StreamPhase::Running | StreamPhase::Updating | StreamPhase::Starting
                    )
                {
                    return;
                }
                self.teardown_encoder(cx);
                match self.create_encoder(cx, false) {
                    Ok(()) => self.submit_due(cx),
                    Err(error) => self.fail(cx, error.wire_code(codes::ENCODER)),
                }
            }
            Internal::SubmitFailed(error) => self.on_encoder_error(cx, None, error),
            Internal::ConvertFailed(error) => self.fail(cx, error.wire_code(codes::OUTPUT)),
            Internal::Fail(code) => self.fail(cx, code),
        }
    }

    fn on_encoder_error(&mut self, cx: &Arc<Shared>, seq: Option<u64>, error: MediaError) {
        match error {
            MediaError::EncoderFailed { recoverable: true } => {
                self.record_loss(cx, seq.unwrap_or(self.submitted_seq.saturating_add(1)));
                self.submit_due(cx);
            }
            MediaError::DeviceLost => {
                let now = cx.clock.now_ns();
                self.device_losses.push_back(now);
                while self
                    .device_losses
                    .front()
                    .is_some_and(|t| now.saturating_sub(*t) > DEVICE_LOSS_WINDOW_NS)
                {
                    self.device_losses.pop_front();
                }
                if self.device_losses.len() > MAX_DEVICE_LOSSES {
                    self.fail(cx, codes::ENCODER);
                } else {
                    self.schedule_recreate(cx);
                }
            }
            MediaError::OutOfResources => {
                self.stalled = true;
            }
            error => self.fail(cx, error.wire_code(codes::ENCODE)),
        }
    }

    // -----------------------------------------------------------------------
    // Frames, admission and submit.

    fn accepts_frames(&self) -> bool {
        matches!(
            self.phase,
            StreamPhase::Starting
                | StreamPhase::StartUnknown
                | StreamPhase::Running
                | StreamPhase::Updating
                | StreamPhase::UpdateUnknown
        )
    }

    fn on_frame(
        &mut self,
        cx: &Arc<Shared>,
        capture: CaptureId,
        member: Option<u8>,
        lease: SurfaceLease,
        info: SurfaceInfo,
        status: FrameStatus,
    ) {
        self.stats.frames_received += 1;
        if status != FrameStatus::Complete
            || self.closing
            || cx.revoked()
            || self.capture != Some(capture)
            || !self.accepts_frames()
            || member.is_some()
            || !valid_surface(&info)
        {
            // The lease drops here and releases the surface.
            return;
        }
        let output = match self.plan.convert {
            ConvertPath::Identity => info.format,
            ConvertPath::Convert => self.plan.encoder_input,
        };
        let mapped = self.plan.color_mapping.map(info.color, info.format, output);
        if self.latest.as_ref().is_some_and(|latest| latest.pending) {
            self.stats.frames_replaced += 1;
        }
        self.time_anchor = Some((info.capture_ns, cx.clock.now_ns()));
        let format = info.format;
        self.latest = Some(Retained {
            lease: Arc::new(lease),
            info,
            mapped,
            pending: true,
            restamp: None,
        });
        self.stalled = false;
        if let Some(slot) = &self.encoder
            && (slot.color != mapped || slot.source_format != format)
        {
            // Keep the frame as the latest without submitting it; the session is
            // re-created on the next wake, never inside submit.
            self.schedule_recreate(cx);
            return;
        }
        self.submit_due(cx);
    }

    /// The only caller of `convert` and `submit`.
    fn submit_due(&mut self, cx: &Arc<Shared>) {
        if self.closing || cx.revoked() || self.recreate_scheduled || self.stalled {
            return;
        }
        let Some(slot) = &self.encoder else {
            return;
        };
        let Some(latest) = &self.latest else {
            return;
        };
        if !latest.pending || cx.work.in_flight() >= u32::from(self.plan.permit_target) {
            return;
        }
        let Some(permit) = cx.work.acquire() else {
            return;
        };
        let encoder = slot.encoder.clone();
        let native_submit = slot.native_submit;
        let sink = slot.sink.clone();
        let generation = slot.generation;
        let force_keyframe = self.force_keyframe || slot.first_submit;
        let mut surface_lease = Some(latest.lease.clone());
        let mut surface = latest.info.id;
        let mut converted = None;
        if self.plan.convert == ConvertPath::Convert {
            let Some(converter) = self.backends.as_ref().and_then(|b| b.converter.clone()) else {
                permit.release();
                return;
            };
            let source = match &self.spec.target {
                CaptureTarget::Region { rect, .. } if self.crop_region => {
                    Some(crop_rect(rect, latest.info.width, latest.info.height))
                }
                _ => None,
            };
            let job = ConvertJob {
                format: self.plan.encoder_input,
                width: self.plan.encode_width,
                height: self.plan.encode_height,
                color: latest.mapped,
                source,
            };
            debug_assert!(is_current(cx.id));
            match converter.convert(latest.info.id, job) {
                Ok(output) => {
                    surface = output.id;
                    // The encoder reads the output: the capture surface is no
                    // longer pinned by this frame (the converter's contract).
                    surface_lease = None;
                    converted = Some(SurfaceLease::new(
                        output.id,
                        Releaser::Converter(Arc::downgrade(&converter)),
                    ));
                }
                Err(MediaError::OutOfResources) => {
                    // Retried when an output, drop or newer frame arrives.
                    self.stalled = true;
                    permit.release();
                    return;
                }
                Err(error) => {
                    permit.release();
                    if let Some(latest) = &mut self.latest {
                        latest.pending = false;
                    }
                    self.post_internal(cx, Internal::ConvertFailed(error));
                    return;
                }
            }
        }
        let base = latest.restamp.unwrap_or(latest.info.capture_ns);
        let pts_ns = if self.last_pts_ns != 0 && base <= self.last_pts_ns {
            self.last_pts_ns + 1_000
        } else {
            base
        };
        let seq = self.submitted_seq + 1;
        let source_present_ns = latest.info.source_present_ns;
        let image_capture_ns = latest.info.capture_ns;
        let opts = SubmitOpts {
            submit_seq: seq,
            pts_ns,
            capture_ns: source_present_ns.unwrap_or(image_capture_ns),
            force_keyframe,
        };
        debug_assert!(is_current(cx.id));
        let result = match native_submit {
            // The sink keeps the encoder's permit until the frame is posted;
            // held before the call, since the output may be posted inside it.
            Some(native) => {
                sink.hold(seq, permit.clone());
                let result =
                    match native.call(surface, seq, pts_ns, opts.capture_ns, force_keyframe) {
                        crate::native::Submitted::Ok => SubmitResult::Submitted,
                        crate::native::Submitted::Busy => SubmitResult::Busy,
                        crate::native::Submitted::Failed(error) => SubmitResult::Failed { error },
                    };
                if result != SubmitResult::Submitted {
                    sink.unhold(seq);
                }
                result
            }
            None => encoder.submit(surface, opts, permit.clone()),
        };
        match result {
            SubmitResult::Submitted => {
                self.submitted_seq = seq;
                self.last_pts_ns = pts_ns;
                self.force_keyframe = false;
                self.stats.submitted += 1;
                if let Some(slot) = &mut self.encoder {
                    slot.first_submit = false;
                }
                if let Some(latest) = &mut self.latest {
                    latest.pending = false;
                    latest.restamp = None;
                }
                self.in_flight.insert(
                    (generation, seq),
                    InFlight {
                        permit,
                        _surface: surface_lease,
                        _converted: converted,
                        source_present_ns,
                        capture_ns: image_capture_ns,
                    },
                );
            }
            SubmitResult::Busy => {
                self.stalled = true;
                permit.release();
            }
            SubmitResult::Failed { error } => {
                permit.release();
                if let Some(latest) = &mut self.latest {
                    latest.pending = false;
                }
                // No endless retry: the failure is handled on the next wake.
                self.post_internal(cx, Internal::SubmitFailed(error));
            }
        }
    }

    // -----------------------------------------------------------------------
    // Encoder output, the VideoOut hop and receipts.

    fn is_current_encoder(&self, generation: u64) -> bool {
        self.encoder
            .as_ref()
            .is_some_and(|slot| slot.generation == generation)
    }

    fn on_encoded(&mut self, cx: &Arc<Shared>, generation: u64, frame: EncodedFrame) {
        let entry = self.in_flight.remove(&(generation, frame.submit_seq));
        let timing = FrameTiming {
            source_present_ns: entry.as_ref().and_then(|e| e.source_present_ns),
            capture_ns: entry.as_ref().map_or(0, |e| e.capture_ns),
            encode: frame.timings,
        };
        if !self.is_current_encoder(generation) || self.closing || cx.revoked() {
            // Stale: discarded after its permit is released. Its sink was closed.
            self.stats.stale_events += 1;
            if let Some(entry) = entry {
                entry.finish();
            }
            self.submit_due(cx);
            return;
        }
        self.stalled = false;
        if frame.form != BitstreamForm::Avcc4 {
            // Outputs are length-prefixed by contract; an Annex-B adapter
            // converts on its output thread (bitstream::annexb_to_length_prefixed).
            if let Some(entry) = entry {
                entry.finish();
            }
            self.fail(cx, codes::ENCODE);
            return;
        }
        let Some(slot) = &self.encoder else {
            return;
        };
        let sink = slot.sink.clone();
        let payload = sink.take(frame.payload_token);
        // This frame's own configuration; without one it is sent with the last one
        // (not cloned per frame).
        let new_config = frame.config_token.and_then(|token| sink.take(token));
        let needs_config = slot.needs_config;
        let Some(payload) = payload else {
            // The bytes never arrived: a loss.
            if let Some(entry) = entry {
                entry.finish();
            }
            self.stats.encoder_drops += 1;
            self.record_loss(cx, frame.submit_seq);
            self.submit_due(cx);
            return;
        };
        let config_changed = needs_config
            || new_config
                .as_ref()
                .is_some_and(|config| self.last_config.as_ref() != Some(config));
        let no_config = new_config.is_none() && self.last_config.is_none();
        if no_config || (config_changed && !frame.keyframe) {
            // Nothing to configure a decoder with, or a delta that would open a
            // generation: this session's IDR was lost or overtaken. Count the
            // frame as lost and re-send the retained image as an IDR now.
            if let Some(entry) = entry {
                entry.finish();
            }
            self.stats.encoder_drops += 1;
            self.force_keyframe = true;
            self.record_loss(cx, frame.submit_seq);
            if let Some(latest) = &mut self.latest {
                latest.pending = true;
            }
            self.submit_due(cx);
            return;
        }
        let headers = match self.packetizer.next(
            frame.keyframe,
            config_changed,
            frame.submit_seq,
            frame.pts_ns / 1_000,
        ) {
            Ok(headers) => headers,
            Err(_) => {
                if let Some(entry) = entry {
                    entry.finish();
                }
                self.post_internal(cx, Internal::Fail(codes::SEQUENCE));
                return;
            }
        };
        if headers.is_empty() {
            // Late or duplicate sequence.
            self.stats.stale_events += 1;
            if let Some(entry) = entry {
                entry.finish();
            }
            self.submit_due(cx);
            return;
        }
        if let Some(slot) = &mut self.encoder {
            slot.needs_config = false;
        }
        if let Some(config) = new_config {
            self.last_config = Some(config);
        }
        self.stats.encoded += 1;
        let Some(out) = self.backends.as_ref().map(|b| b.out.clone()) else {
            return;
        };
        let mut payload = Some(payload);
        for header in headers {
            self.stats.wire_generation = header.generation;
            // The authority is checked before each outbound call.
            if cx.revoked() {
                break;
            }
            let is_config = header.flags == 2;
            let bytes = if is_config {
                self.last_config.clone()
            } else {
                payload.take()
            };
            let Some(bytes) = bytes else {
                continue;
            };
            let receipt = SinkReceipt::new(
                self.executor.clone(),
                generation,
                header.sequence,
                frame.keyframe,
                is_config,
            );
            let meta = VideoOutMeta {
                header,
                droppable: !is_config && !frame.keyframe,
                keyframe: frame.keyframe && !is_config,
                config: is_config,
                timing,
            };
            debug_assert!(is_current(cx.id));
            // `out` (the C send's context) is held for the call.
            match cx.native().send {
                Some(send) => crate::native::send(send, cx.id, &meta, bytes, receipt),
                None => out.send(meta, bytes, Arc::new(receipt)),
            }
            self.stats.packets_sent += 1;
        }
        // The sink accepted the frame: release admission, then admit the frame
        // waiting in the latest slot.
        if let Some(entry) = entry {
            entry.finish();
        }
        self.submit_due(cx);
    }

    fn on_dropped(&mut self, cx: &Arc<Shared>, generation: u64, seq: u64, reason: DropReason) {
        if let Some(entry) = self.in_flight.remove(&(generation, seq)) {
            entry.finish();
        }
        if !self.is_current_encoder(generation) || self.closing {
            self.stats.stale_events += 1;
            self.submit_due(cx);
            return;
        }
        self.stalled = false;
        self.stats.encoder_drops += 1;
        match reason {
            // A no-output completion is a loss, even of the last idle capture:
            // recovery must not depend on another screen update.
            DropReason::NoOutput => {
                self.record_loss(cx, seq);
                self.submit_due(cx);
            }
            DropReason::Error { error } => self.on_encoder_error(cx, Some(seq), error),
        }
    }

    fn on_receipt(
        &mut self,
        cx: &Shared,
        generation: u64,
        seq: u64,
        keyframe: bool,
        config: bool,
        ok: bool,
    ) {
        if config || self.closing || !self.is_current_encoder(generation) {
            return;
        }
        if ok {
            if keyframe {
                self.delivered_keyframe = self.delivered_keyframe.max(seq);
                // A keyframe submitted before the loss cannot end its recovery.
                if seq > self.lost_seq {
                    self.recovery_needed = false;
                    self.recovery_timer = None;
                    self.cancel_timers(|kind| *kind == TimerKind::Recovery);
                }
            }
        } else {
            self.stats.packets_refused += 1;
            self.record_loss(cx, seq);
        }
    }

    // -----------------------------------------------------------------------
    // Recovery.

    fn clear_recovery(&mut self) {
        self.recovery_needed = false;
        self.force_keyframe = false;
        self.recovery_timer = None;
        self.cancel_timers(|kind| *kind == TimerKind::Recovery);
        self.delivered_keyframe = 0;
        self.lost_seq = 0;
        self.last_recovery_ns = None;
    }

    fn record_loss(&mut self, cx: &Shared, seq: u64) {
        if seq <= self.delivered_keyframe {
            return;
        }
        self.lost_seq = self.lost_seq.max(seq);
        self.recovery_needed = true;
        self.schedule_recovery(cx);
    }

    fn schedule_recovery(&mut self, cx: &Shared) {
        if cx.revoked()
            || self.closing
            || self.encoder.is_none()
            || !self.recovery_needed
            || self.recovery_timer.is_some()
        {
            return;
        }
        let token = self.set_timer(cx, self.timings.recovery_delay_ns, TimerKind::Recovery);
        self.recovery_timer = Some(token);
    }

    /// A client keyframe request: a keyframe already submitted cannot satisfy it.
    fn request_recovery(&mut self, cx: &Arc<Shared>) {
        if cx.revoked() || self.closing || self.encoder.is_none() {
            return;
        }
        self.lost_seq = self.lost_seq.max(self.submitted_seq);
        self.recovery_needed = true;
        self.recover(cx);
    }

    fn recover(&mut self, cx: &Arc<Shared>) {
        if cx.revoked() || self.closing || !self.recovery_needed || self.encoder.is_none() {
            return;
        }
        let now = cx.clock.now_ns();
        let too_soon = self
            .last_recovery_ns
            .is_some_and(|last| now.saturating_sub(last) < self.timings.recovery_interval_ns);
        let pressured = self
            .backends
            .as_ref()
            .is_some_and(|b| b.out.backpressured());
        if too_soon || pressured {
            self.schedule_recovery(cx);
            return;
        }
        self.last_recovery_ns = Some(now);
        self.stats.recoveries += 1;
        self.force_keyframe = true;
        let restamp = self
            .time_anchor
            .map(|(host, local)| host.saturating_add(now.saturating_sub(local)));
        if let Some(latest) = &mut self.latest {
            latest.pending = true;
            latest.restamp = restamp;
        }
        self.submit_due(cx);
        self.schedule_recovery(cx);
    }
}
